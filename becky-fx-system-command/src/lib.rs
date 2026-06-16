//! System-command integration for the Becky engine.
//!
//! This crate adapts a plain operating-system command into Becky's effect
//! lifecycle traits. It can discover an already-running matching command,
//! spawn a new process when needed, send termination signals, and report process
//! accounting metrics through `sysinfo`.
//!
//! The controller intentionally treats the command plus all arguments as the
//! identity of the process. A host process with the same executable and argument
//! vector is considered the same effect instance.

use std::ffi::OsString;
use std::fmt::{Debug, Display, Formatter};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use async_trait::async_trait;
use bon::Builder;
use sysinfo::{DiskUsage, Pid, System};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use becky_engine::FxAccounting;
use becky_engine::control::FxControl;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::FxResourceConstraints;
use becky_engine::metadata::MetadataManager;
use becky_engine::state::{FxDesiredExecutionState, FxExecutionState};
use becky_engine::storage::SysStorage;
use becky_fx_id::FxId;

/// Errors returned while managing a system command process.
///
/// These errors cover process creation, signal delivery, and state transitions
/// where the expected process can no longer be found.
#[derive(Debug, Error)]
pub enum FxSysCommandError {
    /// An operating-system I/O error occurred while spawning or interacting with
    /// a child process.
    #[error("i/o: {0}")]
    IO(#[from] std::io::Error),

    /// The tracked process id does not currently refer to a running process.
    #[error("process not running")]
    ProcessNotRunning,

    /// The operating system did not accept or complete a signal request.
    #[error("signal failed to send")]
    SignalFailedToSend,

    /// A string-backed error for command-management failures that do not have a
    /// more specific variant.
    #[error("{0}")]
    String(String),

    /// The requested operation is not implemented by this provider.
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),

    /// The process id was not found in the process table.
    #[error("pid {0} not found")]
    PidNotFound(u32),

    /// The process exists but its command vector is empty.
    #[error("pid {0} has an empty command vector")]
    EmptyCommand(u32),

    /// The process did not exit before the destroy wait deadline elapsed.
    #[error("pid {0} did not exit after destroy")]
    ProcessDidNotExit(u32),
}

/// A Unix process scheduling priority used with `nice`.
///
/// Lower numeric values request higher scheduling priority. The helper
/// constructors use the broadest supported range across Linux and macOS.
///
/// Linux: -20 - 0 - +19
/// OSX: -20 - 0 - +20
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
pub struct NiceLevel(i8);

impl NiceLevel {
    /// Returns the highest priority nice level.
    ///
    /// This maps to `-20`, which is accepted on Linux and macOS.
    pub fn highest_priority() -> Self {
        NiceLevel(-20)
    }

    /// Returns the lowest priority nice level.
    ///
    /// This maps to the lowest supported priority for the target platform:
    /// `19` on Linux and `20` on macOS.
    #[cfg(target_os = "linux")]
    pub fn lowest_priority() -> Self {
        NiceLevel(19)
    }

    /// Returns the lowest priority nice level.
    ///
    /// This maps to the lowest supported priority for the target platform:
    /// `19` on Linux and `20` on macOS.
    #[cfg(target_os = "macos")]
    pub fn lowest_priority() -> Self {
        NiceLevel(20)
    }

    /// Returns the default process priority.
    pub fn default_priority() -> Self {
        NiceLevel(0)
    }
}

impl Display for NiceLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A controllable operating-system command managed through the Becky engine.
///
/// `FxSystemCommand` stores the command line to run, the engine-visible
/// execution state, and optional process-launch settings such as a nice level or
/// CPU affinity. It implements [`FxControl`] for starting, stopping, destroying,
/// and checking the command, and implements [`FxAccounting`] by reading process
/// metrics from `sysinfo`.
#[derive(Builder, Clone, Debug, Eq, PartialEq)]
pub struct FxSystemCommand {
    /// The executable name or path passed to `tokio::process::Command`.
    pub command: String,

    /// Positional arguments passed to the command.
    pub args: Vec<String>,

    // current state,
    /// The last state recorded by this controller.
    pub state: FxExecutionState,

    // desired state,
    /// The state the engine should converge the command toward.
    pub desired_state: FxDesiredExecutionState,

    /// Optional process priority to apply by launching through `nice`.
    pub nice_level: Option<NiceLevel>,

    /// Optional directory for pid-file based tracking.
    ///
    /// When set, starts write pid files under this directory and later starts
    /// use those files before falling back to command-line process scans.
    pub pid_directory: Option<PathBuf>,

    /// Optional Linux CPU affinity list applied by launching through `taskset`.
    #[cfg(target_os = "linux")]
    pub pin_to_cpus: Option<Vec<usize>>,

    /// Optional file path for child stdout.
    pub stdout_path: Option<PathBuf>,

    /// Optional file path for child stderr.
    pub stderr_path: Option<PathBuf>,
}

pub struct SystemProcessRunningTokio {
    handle: Option<JoinHandle<tokio::io::Result<ExitStatus>>>,
    tx: Option<Sender<()>>,
    pid: u32,
    child: Arc<RwLock<tokio::process::Child>>,
}

/// A running process associated with an [`FxSystemCommand`].
///
/// Commands started by this controller are represented by [`Self::Tokio`],
/// which keeps the child handle available for async status checks. Existing
/// processes discovered on the host are represented by [`Self::Pid`].
pub enum FxSystemProcessRunning {
    // this process created a child process
    /// A process spawned by this crate through Tokio.
    ///
    /// The tuple contains the waiter task, a shutdown channel, the child pid,
    /// and the shared child handle.
    Tokio(SystemProcessRunningTokio),

    // created by another process
    /// A process that already existed and is tracked only by pid.
    Pid(u32),
}

#[async_trait]
impl FxAccounting for FxSystemProcessRunning {
    type Instance = ();

    /// Returns accumulated CPU time for the tracked pid, or `0` if it cannot be
    /// found.
    async fn accumulated_cpu_time(&self, _i: &Self::Instance) -> u64 {
        let s = System::new_all();
        let pid = self.get_pid();
        if let Some(proc) = s.process(Pid::from_u32(pid)) {
            return proc.accumulated_cpu_time();
        }
        0
    }

    /// Returns disk usage for the tracked pid.
    async fn disk_usage(&self, _i: &Self::Instance) -> DiskUsage {
        let s = System::new_all();
        let pid = self.get_pid();
        if let Some(proc) = s.process(Pid::from_u32(pid)) {
            return proc.disk_usage();
        }
        DiskUsage {
            total_written_bytes: 0,
            written_bytes: 0,
            total_read_bytes: 0,
            read_bytes: 0,
        }
    }

    /// Returns resident memory for the tracked pid, or `0` if it cannot be
    /// found.
    async fn memory(&self, _i: &Self::Instance) -> u64 {
        let s = System::new_all();
        let pid = self.get_pid();
        if let Some(proc) = s.process(Pid::from_u32(pid)) {
            return proc.memory();
        }
        0
    }

    /// Returns virtual memory for the tracked pid, or `0` if it cannot be
    /// found.
    async fn virtual_memory(&self, _i: &Self::Instance) -> u64 {
        let s = System::new_all();
        let pid = self.get_pid();
        if let Some(proc) = s.process(Pid::from_u32(pid)) {
            return proc.virtual_memory();
        }
        0
    }

    /// Returns run time for the tracked pid, or `0` if it cannot be found.
    async fn run_time(&self, _i: &Self::Instance) -> u64 {
        let s = System::new_all();
        let pid = self.get_pid();
        if let Some(proc) = s.process(Pid::from_u32(pid)) {
            return proc.run_time();
        }
        0
    }
}

impl Debug for FxSystemProcessRunning {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FxSystemProcessRunning::Tokio(handle) => {
                write!(f, "tokio process: {:?} pid:{}", handle.handle.as_ref(), handle.pid)
            }
            FxSystemProcessRunning::Pid(pid) => {
                write!(f, "pid process: {:?}", pid)
            }
        }
    }
}

impl FxSystemProcessRunning {
    /// Returns the operating-system process id for this running process.
    pub fn get_pid(&self) -> u32 {
        match self {
            FxSystemProcessRunning::Tokio(handle) => handle.pid,
            FxSystemProcessRunning::Pid(pid) => *pid,
        }
    }

    /// Sends `SIGTERM` to the tracked process.
    ///
    /// This requests graceful termination and does not wait for the process to
    /// exit.
    pub fn stop(&self) -> Result<(), FxSysCommandError> {
        match self {
            FxSystemProcessRunning::Tokio(handle) => signal_process_group(handle.pid, libc::SIGTERM),
            FxSystemProcessRunning::Pid(pid) => signal_process(*pid, sysinfo::Signal::Term),
        }
    }

    /// Sends `SIGKILL` to the tracked process.
    pub async fn destroy(&mut self) -> Result<(), FxSysCommandError> {
        match self {
            FxSystemProcessRunning::Tokio(handle) => {
                signal_process_group(handle.pid, libc::SIGKILL)?;
                if let Some(tx) = handle.tx.take() {
                    let _ = tx.send(()).await;
                }
                match handle.handle.take() {
                    Some(waiter) => match waiter.await {
                        Ok(Ok(_)) => Ok(()),
                        Ok(Err(err)) => Err(FxSysCommandError::IO(err)),
                        Err(err) => Err(FxSysCommandError::String(format!("destroy waiter task failed: {err}"))),
                    },
                    None => match handle.child.write().await.wait().await {
                        Ok(_) => Ok(()),
                        Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
                        Err(err) => Err(FxSysCommandError::IO(err)),
                    },
                }
            }
            FxSystemProcessRunning::Pid(pid) => {
                signal_process(*pid, sysinfo::Signal::Kill)?;
                wait_for_pid_exit(*pid).await
            }
        }
    }
}

async fn cleanup_spawned_child(mut child: tokio::process::Child) {
    if let Err(err) = child.kill().await {
        error!("failed to clean up spawned child after start error: {err}");
    }
}

fn encode_command_id(command: &str, args: &[String]) -> String {
    let mut encoded = String::new();
    append_id_component(&mut encoded, command);
    for arg in args {
        append_id_component(&mut encoded, arg);
    }
    encoded
}

fn append_id_component(encoded: &mut String, value: &str) {
    encoded.push_str(&value.len().to_string());
    encoded.push(':');
    encoded.push_str(value);
}

fn launch_command_line(command: &FxSystemCommand) -> (String, Vec<String>) {
    let (cmd, args) = match &command.nice_level {
        None => (command.command.clone(), command.args.clone()),
        Some(nice_level) => {
            let mut nice_args = vec!["-n".to_string(), nice_level.to_string(), command.command.clone()];
            nice_args.extend(command.args.iter().cloned());
            ("nice".to_string(), nice_args)
        }
    };

    #[cfg(target_os = "linux")]
    let (cmd, args) = match &command.pin_to_cpus {
        Some(cpus) => {
            let mut cpu_args = vec!["-c".to_string(), cpus.iter().map(|cpu| cpu.to_string()).collect::<Vec<_>>().join(","), cmd];
            cpu_args.extend(args);
            ("taskset".to_string(), cpu_args)
        }
        None => (cmd, args),
    };

    (cmd, args)
}

fn pid_file_path(command: &FxSystemCommand, fx_id: &FxId) -> Option<PathBuf> {
    command.pid_directory.as_ref().map(|dir| dir.join(format!("{}.pid", pid_file_stem(fx_id))))
}

fn pid_file_stem(fx_id: &FxId) -> String {
    let mut stem = String::from("fx-");
    for byte in fx_id.to_string().as_bytes() {
        stem.push_str(&format!("{byte:02x}"));
    }
    stem
}

async fn read_pid_file(path: &PathBuf) -> Result<Option<u32>, FxSysCommandError> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => match contents.trim().parse::<u32>() {
            Ok(pid) => Ok(Some(pid)),
            Err(_) => Ok(None),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(FxSysCommandError::IO(err)),
    }
}

async fn write_pid_file(path: &PathBuf, pid: u32) -> Result<(), FxSysCommandError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, format!("{pid}\n")).await?;
    Ok(())
}

async fn remove_pid_file(path: &PathBuf) -> Result<(), FxSysCommandError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(FxSysCommandError::IO(err)),
    }
}

fn pid_matches_command(pid: u32, cmd: &str, args: &[String]) -> bool {
    let s = System::new_all();
    if let Some(process) = s.process(Pid::from_u32(pid)) {
        command_equal(cmd, args, process.cmd())
    } else {
        false
    }
}

async fn running_pid_from_pid_file(path: &PathBuf, cmd: &str, args: &[String]) -> Result<Option<u32>, FxSysCommandError> {
    let Some(pid) = read_pid_file(path).await? else {
        return Ok(None);
    };

    if pid_matches_command(pid, cmd, args) {
        Ok(Some(pid))
    } else {
        remove_pid_file(path).await?;
        Ok(None)
    }
}

async fn create_stdio_file(path: &PathBuf) -> Result<Stdio, FxSysCommandError> {
    let file = tokio::fs::File::create(path).await?;
    Ok(Stdio::from(file.into_std().await))
}

fn signal_process(pid: u32, signal: sysinfo::Signal) -> Result<(), FxSysCommandError> {
    let s = System::new_all();
    if let Some(process) = s.process(Pid::from_u32(pid)) {
        match process.kill_with(signal) {
            Some(true) => Ok(()),
            Some(false) | None => Err(FxSysCommandError::SignalFailedToSend),
        }
    } else {
        Err(FxSysCommandError::ProcessNotRunning)
    }
}

fn signal_process_group(pid: u32, signal: libc::c_int) -> Result<(), FxSysCommandError> {
    let s = System::new_all();
    if s.process(Pid::from_u32(pid)).is_none() {
        return Err(FxSysCommandError::ProcessNotRunning);
    }

    #[allow(unsafe_code)]
    // SAFETY: `libc::kill` is called with a negative process id to target the
    // child process group created with `setpgid(0, 0)` before exec. The signal
    // value is supplied by this module from libc constants.
    let sent = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if sent == 0 { Ok(()) } else { Err(FxSysCommandError::SignalFailedToSend) }
}

fn process_exists(pid: u32) -> bool {
    System::new_all().process(Pid::from_u32(pid)).is_some()
}

async fn wait_for_pid_exit(pid: u32) -> Result<(), FxSysCommandError> {
    const DESTROY_EXIT_POLL_ATTEMPTS: usize = 100;
    const DESTROY_EXIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    for _ in 0..DESTROY_EXIT_POLL_ATTEMPTS {
        if !process_exists(pid) {
            return Ok(());
        }
        tokio::time::sleep(DESTROY_EXIT_POLL_INTERVAL).await;
    }

    if process_exists(pid) {
        Err(FxSysCommandError::ProcessDidNotExit(pid))
    } else {
        Ok(())
    }
}

/// Compares a command and its arguments with a slice of `OsString`.
///
/// This function returns `true` if the first element of `cmd_b` matches `cmd_a`
/// and the remaining elements of `cmd_b` match the elements of `cmd_a_args`.
/// `OsString` values are compared using `to_string_lossy()`.
fn command_equal(cmd_a: &str, cmd_a_args: &[String], cmd_b: &[OsString]) -> bool {
    if cmd_b.is_empty() {
        return false;
    }
    if cmd_a == cmd_b[0] && cmd_a_args.len() == cmd_b.len() - 1 {
        cmd_a_args.iter().zip(cmd_b.iter().skip(1)).all(|(s, os)| s == &*os.to_string_lossy())
    } else {
        false
    }
}

impl FxSystemCommand {
    /// Creates a command in the `NotStarted` state.
    ///
    /// Optional launch settings such as nice level and CPU affinity are left
    /// unset.
    pub fn new(command: String, args: Vec<String>, desired_state: FxDesiredExecutionState) -> Self {
        Self {
            command,
            args,
            state: FxExecutionState::NotStarted,
            desired_state,
            nice_level: None,
            pid_directory: None,
            #[cfg(target_os = "linux")]
            pin_to_cpus: None,
            stdout_path: None,
            stderr_path: None,
        }
    }

    /// Creates a command from an existing process id.
    ///
    /// The command and arguments are reconstructed from the process table using
    /// lossy string conversion. The returned command starts in the
    /// [`FxExecutionState::Running`] desired and current state, with optional
    /// launch settings unset.
    ///
    /// Returns [`FxSysCommandError::PidNotFound`] when the pid cannot be found.
    ///
    /// Returns [`FxSysCommandError::EmptyCommand`] when the process exists but
    /// has an empty command vector.
    pub fn new_from_pid(pid: u32) -> Result<Self, FxSysCommandError> {
        let s = System::new_all();
        if let Some(process) = s.process(Pid::from_u32(pid)) {
            let Some((command, args)) = process.cmd().split_first() else {
                return Err(FxSysCommandError::EmptyCommand(pid));
            };
            // TODO get nice level from /proc/pid/stat on linux
            Ok(Self {
                command: command.to_string_lossy().to_string(),
                args: args.iter().map(|x| x.to_string_lossy().to_string()).collect(),
                state: FxExecutionState::Running(pid),
                desired_state: FxDesiredExecutionState::Running,
                nice_level: None,
                pid_directory: None,
                #[cfg(target_os = "linux")]
                pin_to_cpus: None,
                stdout_path: None,
                stderr_path: None,
            })
        } else {
            Err(FxSysCommandError::PidNotFound(pid))
        }
    }

    /// Returns whether a matching process is currently running on the host.
    ///
    /// If this command already records a running pid, the pid is checked first
    /// and must still match the configured command line. Otherwise every process
    /// visible through `sysinfo` is scanned for an exact command-line match.
    pub fn is_running(&self) -> bool {
        match &self.state {
            FxExecutionState::Running(pid) => {
                let s = System::new_all();
                if let Some(process) = s.process(Pid::from_u32(*pid))
                    && command_equal(&self.command, &self.args, process.cmd())
                {
                    true
                } else {
                    false
                }
            }
            _ => {
                let s = System::new_all();
                for proc in s.processes().values() {
                    if command_equal(&self.command, &self.args, proc.cmd()) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Returns the current host-observed execution state for this command.
    ///
    /// This method does not mutate `self`. When a recorded pid no longer matches
    /// the configured command, it reports [`FxExecutionState::Unknown`]. When no
    /// recorded pid exists, it scans the process table and returns the first
    /// matching process.
    pub fn current_system_state(&self) -> FxExecutionState {
        match &self.state {
            FxExecutionState::Running(pid) => {
                let s = System::new_all();
                if let Some(process) = s.process(Pid::from_u32(*pid)) {
                    if command_equal(&self.command, &self.args, process.cmd()) {
                        FxExecutionState::Running(*pid)
                    } else {
                        FxExecutionState::Unknown
                    }
                } else {
                    FxExecutionState::NotStarted
                }
            }
            _ => {
                let s = System::new_all();
                for (pid, proc) in s.processes() {
                    if command_equal(&self.command, &self.args, proc.cmd()) {
                        debug!("found running process {} {:?} at pid {:?}", self.command, self.args, pid);
                        return FxExecutionState::Running(pid.as_u32());
                    }
                }
                self.state.clone()
            }
        }
    }
}

#[async_trait]
impl FxControl for FxSystemCommand {
    type Id = String;

    /// Returns a stable command identifier using a length-prefixed encoding of
    /// the executable and arguments.
    fn id(&self) -> Self::Id {
        encode_command_id(&self.command, &self.args)
    }

    type FxAllocateResult = ();
    type FxAllocateError = ();

    // No need to allocate any storage for a system command
    /// Performs allocation for the command effect.
    ///
    /// System commands do not require Becky storage or metadata allocation, so
    /// this is a no-op.
    async fn fx_allocate<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        Ok(())
    }

    type FxBootstrapResult = ();
    type FxBootstrapError = ();

    async fn fx_bootstrap<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        Ok(())
    }

    type FxSpawnResult = FxSystemProcessRunning;
    type FxSpawnError = FxSysCommandError;

    /// Starts the command or attaches to an already-running matching process.
    ///
    /// If a matching process is found, this updates the recorded state and
    /// returns a pid-only handle. Otherwise it spawns the configured command,
    /// optionally wrapping it with `nice` or, on Linux, `taskset`.
    ///
    /// On Linux, CPU pinning composes with `nice` by launching
    /// `taskset -c <cpus> nice -n <level> <command> ...`.
    async fn fx_start<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError> {
        self.desired_state = FxDesiredExecutionState::Running;
        let (cmd, args) = launch_command_line(self);
        let pid_file = pid_file_path(self, fx_id);

        if let Some(path) = &pid_file
            && let Some(pid) = running_pid_from_pid_file(path, &cmd, &args).await?
        {
            self.state = FxExecutionState::Running(pid);
            info!("reattached process {} {:?} from pid file {:?} at pid {:?}", self.command, self.args, path, pid);
            return Ok(FxSystemProcessRunning::Pid(pid));
        }

        let current_state = if pid_file.is_some() {
            match self.state {
                FxExecutionState::Running(pid) if pid_matches_command(pid, &cmd, &args) => FxExecutionState::Running(pid),
                _ => FxExecutionState::NotStarted,
            }
        } else {
            self.current_system_state()
        };

        match current_state {
            FxExecutionState::Running(pid) => {
                // update internal state if it happens to not be registered (current_system_state() does not mutate self)
                self.state = FxExecutionState::Running(pid);
                if let Some(path) = &pid_file {
                    write_pid_file(path, pid).await?;
                }
                Ok(FxSystemProcessRunning::Pid(pid))
            }
            _ => {
                let mut proc = tokio::process::Command::new(&cmd);
                proc.args(&args);

                match &self.stderr_path {
                    Some(path) => {
                        proc.stderr(create_stdio_file(path).await?);
                    }
                    None => {
                        proc.stderr(Stdio::null());
                    }
                }
                match &self.stdout_path {
                    Some(path) => {
                        proc.stdout(create_stdio_file(path).await?);
                    }
                    None => {
                        proc.stdout(Stdio::null());
                    }
                }
                proc.stdin(Stdio::null());
                proc.kill_on_drop(false);
                #[allow(unsafe_code)]
                // SAFETY: `pre_exec` runs in the child after fork and before
                // exec. The closure only calls async-signal-safe `setpgid` and
                // converts its return value into an `io::Result`.
                unsafe {
                    proc.pre_exec(|| {
                        if libc::setpgid(0, 0) == 0 {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    });
                }

                let mut child = proc.spawn()?;

                match child.try_wait() {
                    Ok(f) => {
                        info!("{:?}", f);
                    }
                    Err(e) => {
                        error!("failed to wait for child process {}", e);
                    }
                }

                let maybe_pid = child.id();

                let pid = match maybe_pid {
                    None => {
                        self.state = FxExecutionState::Unknown;
                        cleanup_spawned_child(child).await;
                        return Err(FxSysCommandError::String("pid is None".to_string()));
                    }
                    Some(found_pid) => {
                        self.state = FxExecutionState::Running(found_pid);
                        if let Some(path) = &pid_file {
                            write_pid_file(path, found_pid).await?;
                        }
                        info!("spawned process {} {:?} at pid {:?}", self.command, self.args, found_pid);
                        found_pid
                    }
                };
                let shared_child = Arc::new(RwLock::new(child));
                let value = shared_child.clone();
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                let handle = tokio::spawn(async move {
                    if rx.recv().await.is_some() {
                        info!("sending sigkill to process");
                        let _ = value.write().await.kill().await;
                        info!("sent sigkill to process, wait()ing");
                        value.write().await.wait().await
                    } else {
                        Ok(ExitStatus::default())
                    }
                });

                let handle = SystemProcessRunningTokio {
                    handle: Some(handle),
                    tx: Some(tx),
                    pid,
                    child: shared_child,
                };
                Ok(FxSystemProcessRunning::Tokio(handle))
            }
        }
    }

    type FxStatusResult = FxExecutionState;
    type FxStatusError = FxExecutionState;

    /// Checks the current status of the tracked process.
    ///
    /// Tokio-spawned children are checked with `try_wait`. Pid-only processes
    /// are looked up through `sysinfo`.
    ///
    /// Tokio-spawned children and pid-only handles both report observed
    /// lifecycle states through `Ok(...)`. Actual status lookup failures are
    /// returned as `Err(FxExecutionState::Error(_))`.
    async fn fx_status(&mut self, process: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError> {
        match process {
            FxSystemProcessRunning::Tokio(handle) => match handle.child.write().await.try_wait() {
                Ok(maybe_exit_status) => match maybe_exit_status {
                    None => Ok(FxExecutionState::Running(handle.pid)),
                    Some(exit_status) => Ok(FxExecutionState::Exited(exit_status)),
                },
                Err(err) => Err(FxExecutionState::Error(err.to_string())),
            },
            FxSystemProcessRunning::Pid(pid) => {
                let s = System::new_all();
                if let Some(process) = s.process(Pid::from_u32(*pid)) {
                    if command_equal(&self.command, &self.args, process.cmd()) {
                        Ok(FxExecutionState::Running(*pid))
                    } else {
                        Ok(FxExecutionState::Unknown)
                    }
                } else {
                    Ok(FxExecutionState::NotStarted)
                }
            }
        }
    }

    type FxStopResult = ();
    type FxStopError = FxSysCommandError;

    // send sigterm, does not wait for the process to exit, so fx_destroy() can be called
    /// Requests graceful process termination with `SIGTERM`.
    async fn fx_stop(&mut self, process: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        process.stop()
    }

    type FxDestroyResult = ();
    type FxDestroyError = FxSysCommandError;

    // send sigkill
    /// Force process termination with `SIGKILL`.
    async fn fx_destroy(&self, process: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError> {
        process.destroy().await
    }

    type FxArchiveResult = ();
    type FxArchiveError = FxSysCommandError;

    /// Archives the process effect.
    ///
    /// Process checkpointing is not implemented yet, so this returns
    /// [`FxSysCommandError::Unsupported`].
    ///
    /// TODO - see if <https://github.com/checkpoint-restore/criu> can be used to archive a process
    async fn fx_archive(&self, _fnr: &mut Self::FxSpawnResult) -> Result<Self::FxArchiveResult, Self::FxArchiveError> {
        Err(FxSysCommandError::Unsupported("system-command archive is not implemented"))
    }
}

#[async_trait]
impl FxAccounting for FxSystemCommand {
    type Instance = FxSystemProcessRunning;

    /// Delegates accumulated CPU-time accounting to the running-process handle.
    async fn accumulated_cpu_time(&self, proc: &Self::Instance) -> u64 {
        proc.accumulated_cpu_time(&()).await
    }

    /// Delegates disk-usage accounting to the running-process handle.
    async fn disk_usage(&self, proc: &Self::Instance) -> DiskUsage {
        proc.disk_usage(&()).await
    }

    /// Delegates resident-memory accounting to the running-process handle.
    async fn memory(&self, proc: &Self::Instance) -> u64 {
        proc.memory(&()).await
    }

    /// Delegates virtual-memory accounting to the running-process handle.
    async fn virtual_memory(&self, proc: &Self::Instance) -> u64 {
        proc.virtual_memory(&()).await
    }

    /// Delegates run-time accounting to the running-process handle.
    async fn run_time(&self, proc: &Self::Instance) -> u64 {
        proc.run_time(&()).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use becky_engine::empty_implementations::Metadataless;
    use becky_engine::machine_conf::ResourceConstraintless;
    use becky_engine::storage::Storageless;
    use std::ffi::OsString;

    #[test]
    fn test_command_equal_match() {
        let cmd_a = "ls";
        let cmd_a_args = vec!["-l".to_string(), "/tmp".to_string()];
        let cmd_b = vec![OsString::from("ls"), OsString::from("-l"), OsString::from("/tmp")];
        assert!(command_equal(cmd_a, &cmd_a_args, &cmd_b));
    }

    #[test]
    fn test_command_equal_cmd_mismatch() {
        let cmd_a = "ls";
        let cmd_a_args = vec!["-l".to_string()];
        let cmd_b = vec![OsString::from("ps"), OsString::from("-l")];
        assert!(!command_equal(cmd_a, &cmd_a_args, &cmd_b));
    }

    #[test]
    fn test_command_equal_args_mismatch() {
        let cmd_a = "ls";
        let cmd_a_args = vec!["-l".to_string()];
        let cmd_b = vec![OsString::from("ls"), OsString::from("-a")];
        assert!(!command_equal(cmd_a, &cmd_a_args, &cmd_b));
    }

    #[test]
    fn test_command_equal_length_mismatch_b_longer() {
        let cmd_a = "ls";
        let cmd_a_args = vec!["-l".to_string()];
        let cmd_b = vec![OsString::from("ls"), OsString::from("-l"), OsString::from("/tmp")];
        // Current implementation might return true because zip stops at shortest
        assert!(!command_equal(cmd_a, &cmd_a_args, &cmd_b));
    }

    #[test]
    fn test_command_equal_length_mismatch_a_longer() {
        let cmd_a = "ls";
        let cmd_a_args = vec!["-l".to_string(), "/tmp".to_string()];
        let cmd_b = vec![OsString::from("ls"), OsString::from("-l")];
        assert!(!command_equal(cmd_a, &cmd_a_args, &cmd_b));
    }

    #[test]
    fn test_command_equal_empty_b() {
        let cmd_a = "ls";
        let cmd_a_args = vec![];
        let cmd_b: Vec<OsString> = vec![];
        assert!(!command_equal(cmd_a, &cmd_a_args, &cmd_b));
    }

    #[test]
    fn launch_command_line_composes_nice() {
        let mut command = FxSystemCommand::new(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "true".to_string()],
            FxDesiredExecutionState::Running,
        );
        command.nice_level = Some(NiceLevel::default_priority());

        let (cmd, args) = launch_command_line(&command);

        assert_eq!(cmd, "nice");
        assert_eq!(args, ["-n", "0", "/bin/sh", "-c", "true"]);
    }

    #[test]
    fn encoded_ids_do_not_collide_for_space_join_ambiguity() {
        let command_a = FxSystemCommand::new("ab".to_string(), vec!["c d".to_string()], FxDesiredExecutionState::Running);
        let command_b = FxSystemCommand::new("ab c".to_string(), vec!["d".to_string()], FxDesiredExecutionState::Running);

        assert_ne!(command_a.id(), command_b.id());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn launch_command_line_composes_taskset_and_nice() {
        let mut command = FxSystemCommand::new(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "true".to_string()],
            FxDesiredExecutionState::Running,
        );
        command.nice_level = Some(NiceLevel::default_priority());
        command.pin_to_cpus = Some(vec![0, 1]);

        let (cmd, args) = launch_command_line(&command);

        assert_eq!(cmd, "taskset");
        assert_eq!(args, ["-c", "0,1", "nice", "-n", "0", "/bin/sh", "-c", "true"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn destroy_kills_tokio_child_and_joins_waiter() {
        let unique = format!("becky-system-command-destroy-{}", std::process::id());
        let script = format!("while :; do sleep 1; done # {unique}");
        let mut command = FxSystemCommand::new("/bin/sh".to_string(), vec!["-c".to_string(), script.clone()], FxDesiredExecutionState::Running);
        let mut metadata = Metadataless {};
        let mut storage = Storageless {};

        let mut process = match command
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("fx".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(process) => process,
            Err(err) => panic!("system command should start: {err}"),
        };

        assert!(matching_process_exists("/bin/sh", &["-c", &script]));
        if let Err(err) = command.fx_destroy(&mut process).await {
            panic!("system command should be destroyed: {err}");
        }

        assert!(!matching_process_exists("/bin/sh", &["-c", &script]));
        match process {
            FxSystemProcessRunning::Tokio(handle) => assert!(handle.handle.is_none()),
            FxSystemProcessRunning::Pid(_) => panic!("test should spawn a tokio child"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_tokio_handle_does_not_kill_child() {
        let unique = format!("becky-system-command-drop-{}", std::process::id());
        let script = format!("while :; do sleep 1; done # {unique}");
        let mut command = FxSystemCommand::new("/bin/sh".to_string(), vec!["-c".to_string(), script.clone()], FxDesiredExecutionState::Running);
        let mut metadata = Metadataless {};
        let mut storage = Storageless {};

        let process = match command
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("fx".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(process) => process,
            Err(err) => panic!("system command should start: {err}"),
        };

        let pid = process.get_pid();
        assert!(matching_process_exists("/bin/sh", &["-c", &script]));
        drop(process);

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(process_exists(pid));

        signal_process_group(pid, libc::SIGKILL).expect("test cleanup should kill child");
        for _ in 0..20 {
            if !process_exists(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!process_exists(pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_spawned_child_kills_child() {
        let unique = format!("becky-system-command-start-cleanup-{}", std::process::id());
        let script = format!("while :; do sleep 1; done # {unique}");
        let mut child = match tokio::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => panic!("test child should spawn: {err}"),
        };

        let Some(pid) = child.id() else {
            let _ = child.kill().await;
            panic!("test child should have a pid");
        };
        assert!(matching_process_exists("/bin/sh", &["-c", &script]));

        cleanup_spawned_child(child).await;
        for _ in 0..20 {
            if !process_exists(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!process_exists(pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_accepts_short_lived_child_without_process_table_verification() {
        let mut command = FxSystemCommand::new(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "exit 0".to_string()],
            FxDesiredExecutionState::Running,
        );
        let mut metadata = Metadataless {};
        let mut storage = Storageless {};

        let mut process = match command
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("fx".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(process) => process,
            Err(err) => panic!("short-lived command should still start successfully: {err}"),
        };

        match command.fx_status(&mut process).await {
            Ok(FxExecutionState::Running(_)) | Ok(FxExecutionState::Exited(_)) => {}
            Ok(state) => panic!("unexpected state for short-lived child: {state:?}"),
            Err(state) => panic!("status should not fail for short-lived child: {state:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pid_files_allow_identical_commands_to_spawn_separately() {
        let pid_dir = std::env::temp_dir().join(format!("becky-system-command-identical-{}", std::process::id()));
        let mut command_a = FxSystemCommand::new("sleep".to_string(), vec!["10".to_string()], FxDesiredExecutionState::Running);
        command_a.pid_directory = Some(pid_dir.clone());
        let mut command_b = command_a.clone();
        let mut metadata = Metadataless {};
        let mut storage = Storageless {};

        let mut process_a = match command_a
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("a".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(process) => process,
            Err(err) => panic!("first command should start: {err}"),
        };
        let mut process_b = match command_b
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("b".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(process) => process,
            Err(err) => panic!("second command should start: {err}"),
        };

        assert_ne!(process_a.get_pid(), process_b.get_pid());

        if let Err(err) = process_a.destroy().await {
            panic!("first command should be destroyed: {err}");
        }
        if let Err(err) = process_b.destroy().await {
            panic!("second command should be destroyed: {err}");
        }
        let _ = tokio::fs::remove_dir_all(pid_dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_reattaches_from_matching_pid_file() {
        let pid_dir = std::env::temp_dir().join(format!("becky-system-command-reattach-{}", std::process::id()));
        let script = format!("while :; do sleep 1; done # becky-system-command-reattach-{}", std::process::id());
        let mut child = match tokio::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => panic!("test child should spawn: {err}"),
        };
        let Some(pid) = child.id() else {
            let _ = child.kill().await;
            panic!("test child should have a pid");
        };

        let mut command = FxSystemCommand::new("/bin/sh".to_string(), vec!["-c".to_string(), script], FxDesiredExecutionState::Running);
        command.pid_directory = Some(pid_dir.clone());
        let fx_id = FxId::String("reattach".to_string());
        let Some(path) = pid_file_path(&command, &fx_id) else {
            let _ = child.kill().await;
            panic!("pid file path should exist");
        };
        if let Err(err) = write_pid_file(&path, pid).await {
            let _ = child.kill().await;
            panic!("pid file should be written: {err}");
        }

        let mut metadata = Metadataless {};
        let mut storage = Storageless {};
        let process = match command
            .fx_start(
                &HostId::String("host".to_string()),
                &fx_id,
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(process) => process,
            Err(err) => {
                let _ = child.kill().await;
                panic!("command should reattach from pid file: {err}");
            }
        };

        match process {
            FxSystemProcessRunning::Pid(found_pid) => assert_eq!(found_pid, pid),
            FxSystemProcessRunning::Tokio(_) => panic!("pid file reattach should return pid-only handle"),
        }

        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = tokio::fs::remove_dir_all(pid_dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn destroy_waits_for_attached_pid_to_exit() {
        let unique = format!("becky-system-command-pid-destroy-{}", std::process::id());
        let script = format!("while :; do sleep 1; done # {unique}");
        let mut child = match tokio::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => panic!("test child should spawn: {err}"),
        };

        let Some(pid) = child.id() else {
            let _ = child.kill().await;
            panic!("test child should have a pid");
        };
        let mut process = FxSystemProcessRunning::Pid(pid);

        if let Err(err) = process.destroy().await {
            panic!("destroy should wait for attached pid exit: {err}");
        }

        assert!(!process_exists(pid));
        match child.try_wait() {
            Ok(Some(_)) | Ok(None) => {}
            Err(err) => panic!("child handle should remain usable after destroy: {err}"),
        }
    }

    #[cfg(unix)]
    fn matching_process_exists(command: &str, args: &[&str]) -> bool {
        let expected_args = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
        System::new_all()
            .processes()
            .values()
            .any(|process| command_equal(command, &expected_args, process.cmd()))
    }
}
