//! Rust function workers for the Becky engine.
//!
//! Rust closures cannot be reattached after moving to a separate process, so
//! this crate uses a stable worker entrypoint. The parent process starts the
//! current executable with `--becky-rust-fn <name>`, and the binary registers
//! named functions at startup so worker-mode invocations can dispatch to them.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use becky_engine::FxAccounting;
use becky_engine::control::FxControl;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::FxResourceConstraints;
use becky_engine::metadata::MetadataManager;
use becky_engine::state::FxExecutionState;
use becky_engine::storage::SysStorage;
use becky_fx_id::FxId;
use bon::Builder;
use sysinfo::{DiskUsage, Pid, System};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tracing::{debug, info};

/// Command-line marker used to enter Rust-function worker mode.
pub const DEFAULT_WORKER_ARG: &str = "--becky-rust-fn";

/// Polling interval used for reattached pid monitors.
pub const DEFAULT_REATTACH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Context passed to a registered Rust function worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustFnContext {
    /// Registered function name.
    pub name: String,
    /// User arguments passed after the function name.
    pub args: Vec<String>,
}

/// Exit result returned by a registered Rust function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustFnExit {
    /// Process exit code.
    pub code: i32,
}

impl RustFnExit {
    /// Successful function completion.
    pub fn success() -> Self {
        Self { code: 0 }
    }

    /// Failed function completion with an explicit exit code.
    pub fn failure(code: i32) -> Self {
        Self { code }
    }
}

/// A named async Rust function runnable in worker mode.
#[async_trait]
pub trait RustFn: Send + Sync + 'static {
    /// Runs the registered function.
    async fn run(&self, ctx: RustFnContext) -> RustFnExit;
}

/// Registry for worker-mode function dispatch.
#[derive(Default)]
pub struct RustFnRegistry {
    functions: BTreeMap<String, Arc<dyn RustFn>>,
}

impl RustFnRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a named Rust function.
    pub fn register<F>(&mut self, name: impl Into<String>, function: F)
    where
        F: RustFn,
    {
        self.functions.insert(name.into(), Arc::new(function));
    }

    /// Runs worker mode if `args` contains `worker_arg`.
    ///
    /// Returns `Ok(Some(exit))` when the current process was a worker-mode
    /// invocation and the caller should exit with `exit.code`. Returns
    /// `Ok(None)` when `args` does not contain the worker marker and normal
    /// application startup should continue.
    pub async fn run_worker_from_args<I, S>(&self, args: I, worker_arg: &str) -> Result<Option<RustFnExit>, RustFnError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
        let Some(marker_idx) = args.iter().position(|arg| arg == worker_arg) else {
            return Ok(None);
        };
        let name_idx = marker_idx + 1;
        let name = args.get(name_idx).ok_or(RustFnError::MissingFunctionName)?.clone();
        let fn_args = args.iter().skip(name_idx + 1).filter(|arg| arg.as_str() != "--").cloned().collect::<Vec<_>>();
        let function = self.functions.get(&name).ok_or_else(|| RustFnError::FunctionNotFound(name.clone()))?;
        Ok(Some(function.run(RustFnContext { name, args: fn_args }).await))
    }

    /// Runs worker mode using [`std::env::args`].
    pub async fn run_worker_from_env(&self) -> Result<Option<RustFnExit>, RustFnError> {
        self.run_worker_from_args(env::args(), DEFAULT_WORKER_ARG).await
    }
}

/// Errors returned by function registry and process control.
#[derive(Debug, Error)]
pub enum RustFnError {
    /// Worker marker was present but no function name followed it.
    #[error("missing function name after worker marker")]
    MissingFunctionName,

    /// No function was registered under the requested name.
    #[error("function {0:?} is not registered")]
    FunctionNotFound(String),

    /// Current executable path could not be determined.
    #[error("failed to determine current executable: {0}")]
    CurrentExe(#[source] std::io::Error),

    /// I/O error while creating directories, reading or writing pid files, or
    /// spawning/waiting for a worker process.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    /// PID file contained an invalid process id.
    #[error("invalid pid file {path}: {source}")]
    InvalidPidFile {
        /// PID file path.
        path: PathBuf,
        /// Parse failure.
        source: std::num::ParseIntError,
    },

    /// PID file points at a process that is no longer running.
    #[error("pid {pid} from {path} is not running")]
    StalePidFile {
        /// PID file path.
        path: PathBuf,
        /// Stale process id.
        pid: u32,
    },

    /// Existing process does not match the configured worker command.
    #[error("pid {pid} does not match rust function worker {function:?}")]
    PidMismatch {
        /// Observed process id.
        pid: u32,
        /// Expected function name.
        function: String,
    },

    /// The worker process did not expose a pid after spawn.
    #[error("spawned worker did not expose a pid")]
    MissingPid,

    /// A signal could not be sent to the process.
    #[error("failed to signal process {0}")]
    SignalFailed(u32),
}

/// Rust function worker process configuration.
#[derive(Builder, Clone, Debug, Eq, PartialEq)]
pub struct FxRustFn {
    /// Registered function name.
    pub function: String,
    /// Executable to invoke in worker mode. Defaults to the current executable.
    pub executable: Option<PathBuf>,
    /// Directory used for pid files.
    pub run_dir: PathBuf,
    /// Arguments passed after the function name.
    pub args: Vec<String>,
    /// Environment variables passed to the worker.
    pub env: Vec<(String, String)>,
    /// Worker-mode argument marker.
    pub worker_arg: String,
    /// Poll interval for monitors attached to existing pid-file processes.
    pub reattach_poll_interval: Duration,
}

impl FxRustFn {
    /// Creates a worker config for a named function using `run_dir` for pid
    /// files.
    pub fn new(function: impl Into<String>, run_dir: impl Into<PathBuf>) -> Self {
        Self {
            function: function.into(),
            executable: None,
            run_dir: run_dir.into(),
            args: vec![],
            env: vec![],
            worker_arg: DEFAULT_WORKER_ARG.to_string(),
            reattach_poll_interval: DEFAULT_REATTACH_POLL_INTERVAL,
        }
    }

    /// Adds worker arguments.
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Adds a worker environment variable.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Returns the path of the pid file for this function.
    pub fn pid_file(&self) -> PathBuf {
        self.run_dir.join(format!("{}.pid", sanitize_pid_name(&self.function)))
    }

    fn executable(&self) -> Result<PathBuf, RustFnError> {
        match &self.executable {
            Some(path) => Ok(path.clone()),
            None => env::current_exe().map_err(RustFnError::CurrentExe),
        }
    }

    fn worker_args(&self) -> Vec<String> {
        let mut args = vec![self.worker_arg.clone(), self.function.clone()];
        args.extend(self.args.iter().cloned());
        args
    }

    async fn read_pid_file(&self) -> Result<Option<u32>, RustFnError> {
        let path = self.pid_file();
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let pid = contents.trim().parse::<u32>().map_err(|source| RustFnError::InvalidPidFile { path, source })?;
                Ok(Some(pid))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(RustFnError::Io(err)),
        }
    }

    async fn write_pid_file(&self, pid: u32) -> Result<(), RustFnError> {
        tokio::fs::create_dir_all(&self.run_dir).await?;
        tokio::fs::write(self.pid_file(), pid.to_string()).await?;
        Ok(())
    }

    fn process_matches(&self, pid: u32) -> bool {
        let system = System::new_all();
        system
            .process(Pid::from_u32(pid))
            .is_some_and(|process| process_cmd_contains_worker(process.cmd(), &self.worker_arg, &self.function))
    }
}

/// Handle for an owned or reattached Rust function worker process.
pub struct RustFnHandle {
    /// Worker process id.
    pub pid: u32,
    /// Registered function name.
    pub function: String,
    /// Whether this start call reattached to an existing pid-file process.
    pub reattached: bool,
    latest_state: Arc<RwLock<FxExecutionState>>,
    cancel_monitor: Sender<()>,
    monitor: Option<JoinHandle<()>>,
}

impl Debug for RustFnHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustFnHandle")
            .field("pid", &self.pid)
            .field("function", &self.function)
            .field("reattached", &self.reattached)
            .field("monitor_finished", &self.monitor.as_ref().is_none_or(JoinHandle::is_finished))
            .finish_non_exhaustive()
    }
}

impl RustFnHandle {
    /// Returns the latest state observed by the monitor.
    pub async fn latest_state(&self) -> FxExecutionState {
        self.latest_state.read().await.clone()
    }

    /// Stops the monitor task without stopping the worker process.
    pub async fn stop_monitor(&mut self) {
        let _ = self.cancel_monitor.send(()).await;
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.await;
        }
    }
}

#[async_trait]
impl FxControl for FxRustFn {
    type Id = String;

    fn id(&self) -> Self::Id {
        self.function.clone()
    }

    type FxAllocateResult = ();
    type FxAllocateError = RustFnError;

    async fn fx_allocate<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        tokio::fs::create_dir_all(&self.run_dir).await?;
        Ok(())
    }

    type FxSpawnResult = RustFnHandle;
    type FxSpawnError = RustFnError;

    async fn fx_start<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError> {
        if let Some(pid) = self.read_pid_file().await? {
            if self.process_matches(pid) {
                return Ok(reattached_handle(pid, self.function.clone(), self.reattach_poll_interval));
            }
            if process_exists(pid) {
                return Err(RustFnError::PidMismatch {
                    pid,
                    function: self.function.clone(),
                });
            }
        }

        let executable = self.executable()?;
        let args = self.worker_args();
        let mut command = tokio::process::Command::new(executable);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        for (key, value) in &self.env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
        let pid = child.id().ok_or(RustFnError::MissingPid)?;
        self.write_pid_file(pid).await?;
        info!("rust-fn spawned function:{} pid:{}", self.function, pid);
        let latest_state = Arc::new(RwLock::new(FxExecutionState::Running(pid)));
        let (cancel_monitor, mut cancel_rx) = tokio::sync::mpsc::channel(1);
        let monitor_state = latest_state.clone();
        let monitor = tokio::spawn(async move {
            tokio::select! {
                _ = cancel_rx.recv() => {}
                wait_result = child.wait() => {
                    match wait_result {
                        Ok(status) => *monitor_state.write().await = FxExecutionState::Exited(status),
                        Err(err) => *monitor_state.write().await = FxExecutionState::Error(err.to_string()),
                    }
                }
            }
        });

        Ok(RustFnHandle {
            pid,
            function: self.function.clone(),
            reattached: false,
            latest_state,
            cancel_monitor,
            monitor: Some(monitor),
        })
    }

    type FxStatusResult = FxExecutionState;
    type FxStatusError = RustFnError;

    async fn fx_status(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError> {
        if process_exists(handle.pid) {
            let state = FxExecutionState::Running(handle.pid);
            *handle.latest_state.write().await = state.clone();
            Ok(state)
        } else {
            Ok(handle.latest_state().await)
        }
    }

    type FxStopResult = ();
    type FxStopError = RustFnError;

    async fn fx_stop(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        signal_process(handle.pid, sysinfo::Signal::Term)
    }

    type FxDestroyResult = ();
    type FxDestroyError = RustFnError;

    async fn fx_destroy(&self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError> {
        handle.stop_monitor().await;
        signal_process(handle.pid, sysinfo::Signal::Kill)
    }

    type FxArchiveResult = ();
    type FxArchiveError = ();

    async fn fx_archive(&self, _fnr: &mut Self::FxSpawnResult) -> Result<Self::FxArchiveResult, Self::FxArchiveError> {
        Ok(())
    }
}

#[async_trait]
impl FxAccounting for FxRustFn {
    type Instance = RustFnHandle;

    async fn accumulated_cpu_time(&self, handle: &Self::Instance) -> u64 {
        process_metric(handle.pid, |process| process.accumulated_cpu_time())
    }

    async fn disk_usage(&self, handle: &Self::Instance) -> DiskUsage {
        process_metric(handle.pid, |process| process.disk_usage())
    }

    async fn memory(&self, handle: &Self::Instance) -> u64 {
        process_metric(handle.pid, |process| process.memory())
    }

    async fn virtual_memory(&self, handle: &Self::Instance) -> u64 {
        process_metric(handle.pid, |process| process.virtual_memory())
    }

    async fn run_time(&self, handle: &Self::Instance) -> u64 {
        process_metric(handle.pid, |process| process.run_time())
    }
}

fn reattached_handle(pid: u32, function: String, poll_interval: Duration) -> RustFnHandle {
    let latest_state = Arc::new(RwLock::new(FxExecutionState::Running(pid)));
    let (cancel_monitor, cancel_rx) = tokio::sync::mpsc::channel(1);
    let monitor = tokio::spawn(monitor_reattached_pid(pid, latest_state.clone(), cancel_rx, poll_interval));
    RustFnHandle {
        pid,
        function,
        reattached: true,
        latest_state,
        cancel_monitor,
        monitor: Some(monitor),
    }
}

async fn monitor_reattached_pid(
    pid: u32,
    latest_state: Arc<RwLock<FxExecutionState>>,
    mut cancel_rx: tokio::sync::mpsc::Receiver<()>,
    poll_interval: Duration,
) {
    loop {
        if !process_exists(pid) {
            *latest_state.write().await = FxExecutionState::NotStarted;
            break;
        }

        tokio::select! {
            _ = cancel_rx.recv() => break,
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }
    debug!("rust-fn reattach monitor stopped pid:{}", pid);
}

fn process_exists(pid: u32) -> bool {
    System::new_all().process(Pid::from_u32(pid)).is_some()
}

fn signal_process(pid: u32, signal: sysinfo::Signal) -> Result<(), RustFnError> {
    let system = System::new_all();
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        match process.kill_with(signal) {
            Some(true) => Ok(()),
            Some(false) | None => Err(RustFnError::SignalFailed(pid)),
        }
    } else {
        Err(RustFnError::StalePidFile { path: PathBuf::new(), pid })
    }
}

fn process_metric<T>(pid: u32, f: impl FnOnce(&sysinfo::Process) -> T) -> T
where
    T: Default,
{
    System::new_all().process(Pid::from_u32(pid)).map(f).unwrap_or_default()
}

fn process_cmd_contains_worker(cmd: &[impl AsRef<OsStr>], worker_arg: &str, function: &str) -> bool {
    cmd.windows(2)
        .any(|pair| pair[0].as_ref() == OsStr::new(worker_arg) && pair[1].as_ref() == OsStr::new(function))
}

fn sanitize_pid_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect()
}

/// Returns the default pid path for `function` under `run_dir`.
pub fn pid_file_path(run_dir: &Path, function: &str) -> PathBuf {
    run_dir.join(format!("{}.pid", sanitize_pid_name(function)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SuccessFn;

    #[async_trait]
    impl RustFn for SuccessFn {
        async fn run(&self, _ctx: RustFnContext) -> RustFnExit {
            RustFnExit::success()
        }
    }

    #[tokio::test]
    async fn registry_ignores_non_worker_args() {
        let registry = RustFnRegistry::new();
        let result = registry.run_worker_from_args(["app", "--other"], DEFAULT_WORKER_ARG).await;
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn registry_dispatches_worker_args() {
        let mut registry = RustFnRegistry::new();
        registry.register("work", SuccessFn);
        let result = registry
            .run_worker_from_args(["app", DEFAULT_WORKER_ARG, "work", "a", "b"], DEFAULT_WORKER_ARG)
            .await;
        assert!(matches!(result, Ok(Some(RustFnExit { code: 0 }))));
    }

    #[tokio::test]
    async fn registry_reports_missing_function() {
        let registry = RustFnRegistry::new();
        let result = registry.run_worker_from_args(["app", DEFAULT_WORKER_ARG, "missing"], DEFAULT_WORKER_ARG).await;
        assert!(matches!(result, Err(RustFnError::FunctionNotFound(name)) if name == "missing"));
    }

    #[test]
    fn pid_names_are_sanitized() {
        assert_eq!(sanitize_pid_name("module::work/1"), "module__work_1");
    }

    #[test]
    fn detects_worker_command_line() {
        let cmd = vec!["/tmp/app", DEFAULT_WORKER_ARG, "work"];
        assert!(process_cmd_contains_worker(&cmd, DEFAULT_WORKER_ARG, "work"));
        assert!(!process_cmd_contains_worker(&cmd, DEFAULT_WORKER_ARG, "other"));
    }
}
