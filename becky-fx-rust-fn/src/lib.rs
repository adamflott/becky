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
use tracing::{debug, info, warn};

/// Command-line marker used to enter Rust-function worker mode.
pub const DEFAULT_WORKER_ARG: &str = "--becky-rust-fn";

/// Polling interval used for reattached pid monitors.
pub const DEFAULT_REATTACH_POLL_INTERVAL: Duration = Duration::from_secs(2);
pub const DEFAULT_STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

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

    /// Worker did not exit before the stop timeout elapsed.
    #[error("process {pid} did not exit within {timeout:?}")]
    StopTimeout {
        /// Process id.
        pid: u32,
        /// Timeout that elapsed.
        timeout: Duration,
    },
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

    async fn remove_pid_file(&self) -> Result<(), RustFnError> {
        match tokio::fs::remove_file(self.pid_file()).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(RustFnError::Io(err)),
        }
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
    worker_arg: String,
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
            .field("worker_arg", &self.worker_arg)
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

    type FxBootstrapResult = ();
    type FxBootstrapError = ();

    async fn fx_bootstrap<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxBootstrapResult, Self::FxBootstrapError> {
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
            match observe_worker_process(pid, &self.worker_arg, &self.function) {
                ObservedProcess::Matching => {
                    return Ok(reattached_handle(
                        pid,
                        self.function.clone(),
                        self.worker_arg.clone(),
                        self.reattach_poll_interval,
                    ));
                }
                ObservedProcess::Mismatched => {
                    return Err(RustFnError::PidMismatch {
                        pid,
                        function: self.function.clone(),
                    });
                }
                ObservedProcess::Missing => self.remove_pid_file().await?,
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
        if let Err(err) = self.write_pid_file(pid).await {
            if let Err(kill_err) = child.kill().await {
                warn!("rust-fn failed to clean up worker after pid-file error pid:{} error:{}", pid, kill_err);
            }
            return Err(err);
        }
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
            worker_arg: self.worker_arg.clone(),
            latest_state,
            cancel_monitor,
            monitor: Some(monitor),
        })
    }

    type FxStatusResult = FxExecutionState;
    type FxStatusError = RustFnError;

    async fn fx_status(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError> {
        match observe_worker_process(handle.pid, &handle.worker_arg, &handle.function) {
            ObservedProcess::Matching => {
                let state = FxExecutionState::Running(handle.pid);
                *handle.latest_state.write().await = state.clone();
                Ok(state)
            }
            ObservedProcess::Mismatched => {
                let state = FxExecutionState::Error(format!("pid {} no longer matches rust function worker {:?}", handle.pid, handle.function));
                *handle.latest_state.write().await = state;
                Err(RustFnError::PidMismatch {
                    pid: handle.pid,
                    function: handle.function.clone(),
                })
            }
            ObservedProcess::Missing => {
                let state = handle.latest_state().await;
                Ok(state)
            }
        }
    }

    type FxStopResult = ();
    type FxStopError = RustFnError;

    async fn fx_stop(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        signal_process(handle.pid, sysinfo::Signal::Term)?;
        wait_for_process_exit(handle.pid, DEFAULT_STOP_WAIT_TIMEOUT).await?;
        handle.stop_monitor().await;
        self.remove_pid_file().await?;
        *handle.latest_state.write().await = FxExecutionState::Stopped;
        Ok(())
    }

    type FxDestroyResult = ();
    type FxDestroyError = RustFnError;

    async fn fx_destroy(&self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError> {
        match observe_worker_process(handle.pid, &handle.worker_arg, &handle.function) {
            ObservedProcess::Matching => {
                signal_process(handle.pid, sysinfo::Signal::Kill)?;
                wait_for_process_exit(handle.pid, DEFAULT_STOP_WAIT_TIMEOUT).await?;
            }
            ObservedProcess::Mismatched => {
                return Err(RustFnError::PidMismatch {
                    pid: handle.pid,
                    function: handle.function.clone(),
                });
            }
            ObservedProcess::Missing => {}
        }
        handle.stop_monitor().await;
        match tokio::fs::remove_file(self.pid_file()).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(RustFnError::Io(err)),
        }
        Ok(())
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

fn reattached_handle(pid: u32, function: String, worker_arg: String, poll_interval: Duration) -> RustFnHandle {
    let latest_state = Arc::new(RwLock::new(FxExecutionState::Running(pid)));
    let (cancel_monitor, cancel_rx) = tokio::sync::mpsc::channel(1);
    let monitor = tokio::spawn(monitor_reattached_pid(
        pid,
        function.clone(),
        worker_arg.clone(),
        latest_state.clone(),
        cancel_rx,
        poll_interval,
    ));
    RustFnHandle {
        pid,
        function,
        reattached: true,
        worker_arg,
        latest_state,
        cancel_monitor,
        monitor: Some(monitor),
    }
}

async fn monitor_reattached_pid(
    pid: u32,
    function: String,
    worker_arg: String,
    latest_state: Arc<RwLock<FxExecutionState>>,
    mut cancel_rx: tokio::sync::mpsc::Receiver<()>,
    poll_interval: Duration,
) {
    loop {
        match observe_worker_process(pid, &worker_arg, &function) {
            ObservedProcess::Matching => {}
            ObservedProcess::Missing => {
                *latest_state.write().await = FxExecutionState::NotStarted;
                break;
            }
            ObservedProcess::Mismatched => {
                *latest_state.write().await = FxExecutionState::Error(format!("pid {pid} no longer matches rust function worker {function:?}"));
                break;
            }
        }

        tokio::select! {
            _ = cancel_rx.recv() => break,
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }
    debug!("rust-fn reattach monitor stopped pid:{}", pid);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservedProcess {
    Matching,
    Mismatched,
    Missing,
}

fn observe_worker_process(pid: u32, worker_arg: &str, function: &str) -> ObservedProcess {
    let system = System::new_all();
    match system.process(Pid::from_u32(pid)) {
        Some(process) if process_cmd_contains_worker(process.cmd(), worker_arg, function) => ObservedProcess::Matching,
        Some(_) => ObservedProcess::Mismatched,
        None => ObservedProcess::Missing,
    }
}

fn process_exists(pid: u32) -> bool {
    System::new_all().process(Pid::from_u32(pid)).is_some()
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), RustFnError> {
    let start = tokio::time::Instant::now();
    loop {
        if !process_exists(pid) {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(RustFnError::StopTimeout { pid, timeout });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    use becky_engine::empty_implementations::Metadataless;
    use becky_engine::machine_conf::ResourceConstraintless;
    use becky_engine::storage::Storageless;

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

    #[tokio::test]
    async fn status_reports_pid_mismatch_for_live_non_worker_process() {
        let (cancel_monitor, _cancel_rx) = tokio::sync::mpsc::channel(1);
        let mut handle = RustFnHandle {
            pid: std::process::id(),
            function: "work".to_string(),
            reattached: true,
            worker_arg: DEFAULT_WORKER_ARG.to_string(),
            latest_state: Arc::new(RwLock::new(FxExecutionState::Running(std::process::id()))),
            cancel_monitor,
            monitor: None,
        };
        let mut fx = FxRustFn::new("work", std::env::temp_dir());

        let result = fx.fx_status(&mut handle).await;

        assert!(matches!(
            result,
            Err(RustFnError::PidMismatch {
                pid,
                function
            }) if pid == std::process::id() && function == "work"
        ));
        assert!(matches!(handle.latest_state().await, FxExecutionState::Error(_)));
    }

    #[tokio::test]
    async fn reattach_monitor_marks_live_non_worker_pid_as_error() {
        let latest_state = Arc::new(RwLock::new(FxExecutionState::Running(std::process::id())));
        let (_cancel_tx, cancel_rx) = tokio::sync::mpsc::channel(1);

        monitor_reattached_pid(
            std::process::id(),
            "work".to_string(),
            DEFAULT_WORKER_ARG.to_string(),
            latest_state.clone(),
            cancel_rx,
            Duration::from_secs(60),
        )
        .await;

        assert!(matches!(*latest_state.read().await, FxExecutionState::Error(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_cleans_up_child_when_pid_file_write_fails() {
        let unique = format!("becky-rust-fn-cleanup-{}", std::process::id());
        let script = format!("while :; do sleep 1; done # {unique}");
        let run_dir = std::env::temp_dir().join(format!("{unique}.run-dir-file"));
        let _ = tokio::fs::remove_file(&run_dir).await;
        let _ = tokio::fs::remove_dir_all(&run_dir).await;
        if let Err(err) = tokio::fs::write(&run_dir, "not a directory").await {
            panic!("test run_dir file should be created: {err}");
        }

        let mut fx = FxRustFn::new(script.clone(), &run_dir);
        fx.executable = Some(PathBuf::from("/bin/sh"));
        fx.worker_arg = "-c".to_string();
        let mut metadata = Metadataless {};
        let mut storage = Storageless {};

        let result = fx
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("fx".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await;

        assert!(matches!(result, Err(RustFnError::Io(_))));
        for _ in 0..20 {
            if !matching_worker_process_exists("-c", &script) {
                let _ = tokio::fs::remove_file(&run_dir).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = tokio::fs::remove_file(&run_dir).await;
        assert!(!matching_worker_process_exists("-c", &script));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn destroy_kills_worker_waits_and_removes_pid_file() {
        let unique = format!("becky-rust-fn-destroy-{}", std::process::id());
        let script = format!("while :; do sleep 1; done # {unique}");
        let run_dir = std::env::temp_dir().join(format!("{unique}.run-dir"));
        let _ = tokio::fs::remove_file(&run_dir).await;
        let _ = tokio::fs::remove_dir_all(&run_dir).await;

        let mut fx = FxRustFn::new(script.clone(), &run_dir);
        fx.executable = Some(PathBuf::from("/bin/sh"));
        fx.worker_arg = "-c".to_string();
        let mut metadata = Metadataless {};
        let mut storage = Storageless {};
        let mut handle = match fx
            .fx_start(
                &HostId::String("host".to_string()),
                &FxId::String("fx".to_string()),
                &mut metadata,
                &ResourceConstraintless,
                &mut storage,
            )
            .await
        {
            Ok(handle) => handle,
            Err(err) => panic!("worker should start: {err}"),
        };

        let pid_file = fx.pid_file();
        assert!(pid_file.exists());
        assert!(matching_worker_process_exists("-c", &script));

        if let Err(err) = fx.fx_destroy(&mut handle).await {
            panic!("worker should be destroyed: {err}");
        }

        assert!(!pid_file.exists());
        assert!(!matching_worker_process_exists("-c", &script));
        assert!(handle.monitor.is_none());
        let _ = tokio::fs::remove_dir_all(&run_dir).await;
    }

    #[cfg(unix)]
    fn matching_worker_process_exists(worker_arg: &str, function: &str) -> bool {
        System::new_all()
            .processes()
            .values()
            .any(|process| process_cmd_contains_worker(process.cmd(), worker_arg, function))
    }
}
