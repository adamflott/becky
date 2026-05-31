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
use tracing::{debug, info};

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

    /// Optional directory reserved for pid-file based tracking.
    ///
    /// The current implementation does not write pid files, but the field is
    /// part of the command configuration for callers that need to carry this
    /// setting.
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
    handle: JoinHandle<tokio::io::Result<ExitStatus>>,
    _tx: Sender<()>,
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
                write!(f, "tokio process: {:?} pid:{}", handle.handle, handle.pid)
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
    pub fn destroy(&self) -> Result<(), FxSysCommandError> {
        match self {
            FxSystemProcessRunning::Tokio(handle) => signal_process_group(handle.pid, libc::SIGKILL),
            FxSystemProcessRunning::Pid(pid) => signal_process(*pid, sysinfo::Signal::Kill),
        }
    }
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
    let sent = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if sent == 0 { Ok(()) } else { Err(FxSysCommandError::SignalFailedToSend) }
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

    /// Returns a stable command identifier made from the executable and
    /// arguments joined with spaces.
    fn id(&self) -> Self::Id {
        let mut cmd_and_args = self.args.clone();
        cmd_and_args.insert(0, self.command.clone());
        cmd_and_args.join(" ")
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

    type FxSpawnResult = FxSystemProcessRunning;
    type FxSpawnError = FxSysCommandError;

    /// Starts the command or attaches to an already-running matching process.
    ///
    /// If a matching process is found, this updates the recorded state and
    /// returns a pid-only handle. Otherwise it spawns the configured command,
    /// optionally wrapping it with `nice` or, on Linux, `taskset`.
    ///
    /// On Linux, CPU pinning takes precedence over `nice` when both are set,
    /// because the `taskset` wrapper replaces the command line prepared for
    /// `nice`.
    async fn fx_start<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError> {
        self.desired_state = FxDesiredExecutionState::Running;
        match self.current_system_state() {
            FxExecutionState::Running(pid) => {
                // update internal state if it happens to not be registered (current_system_state() does not mutate self)
                self.state = FxExecutionState::Running(pid);
                Ok(FxSystemProcessRunning::Pid(pid))
            }
            _ => {
                let (cmd, args) = match &self.nice_level {
                    None => (self.command.clone(), self.args.clone()),
                    Some(nice_level) => {
                        let mut nice_args = vec!["-n".to_string(), nice_level.to_string(), self.command.to_string()];
                        nice_args.append(&mut self.args.clone());
                        ("nice".to_string(), nice_args)
                    }
                };

                #[cfg(target_os = "linux")]
                let (cmd, args) = match &self.pin_to_cpus {
                    Some(cpus) => {
                        let mut cpu_args = vec!["-c".to_string(), cpus.iter().map(|x| x.to_string()).collect::<Vec<String>>().join(","), cmd];
                        cpu_args.extend(args);
                        ("taskset".to_string(), cpu_args)
                    }
                    None => (cmd, args),
                };

                let mut proc = tokio::process::Command::new(cmd);
                proc.args(args);

                match &self.stderr_path {
                    Some(path) => {
                        proc.stderr(Stdio::from(std::fs::File::create(path)?));
                    }
                    None => {
                        proc.stderr(Stdio::null());
                    }
                }
                match &self.stdout_path {
                    Some(path) => {
                        proc.stdout(Stdio::from(std::fs::File::create(path)?));
                    }
                    None => {
                        proc.stdout(Stdio::null());
                    }
                }
                proc.stdin(Stdio::null());
                proc.kill_on_drop(false);
                #[allow(unsafe_code)]
                unsafe {
                    proc.pre_exec(|| {
                        // Creates a new process group for the child
                        libc::setpgid(0, 0);
                        Ok(())
                    });
                }

                let child = proc.spawn()?;
                info!("spawned process: {} {:?}", self.command, self.args);
                let maybe_pid = child.id();
                let shared_child = Arc::new(RwLock::new(child));

                let pid = match maybe_pid {
                    None => {
                        self.state = FxExecutionState::Unknown;
                        return Err(FxSysCommandError::String("pid is None".to_string()));
                    }
                    Some(found_pid) => {
                        self.state = FxExecutionState::Running(found_pid);
                        info!("spawned process {} {:?} at pid {:?}", self.command, self.args, found_pid);
                        found_pid
                    }
                };
                let value = shared_child.clone();
                let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                let handle = tokio::spawn(async move {
                    while rx.recv().await.is_some() {}
                    info!("sending sigkill to process");
                    let _ = value.write().await.kill().await;
                    info!("sent sigkill to process");
                    value.write().await.wait().await
                });

                let handle = SystemProcessRunningTokio {
                    handle,
                    _tx: tx,
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
        process.destroy()
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
}
