//! Shared utilities for Becky crates.
//!
//! This crate contains small helpers that are used by providers and support
//! crates, but do not belong to a specific Becky backend. Today that means
//! process-table lookup through `sysinfo` and async system-command execution
//! through Tokio.

use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, error, info};

use sysinfo::{Pid, System};

/// A snapshot of a process visible in the host process table.
///
/// The `name` and `args` fields are normalized to UTF-8 strings using the
/// platform display representation supplied by `sysinfo`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Process {
    /// The operating-system process id.
    pub pid: u32,

    /// The process executable name as reported by the platform.
    pub name: String,

    /// The command-line arguments as reported by the platform.
    pub args: Vec<String>,
}

/// Looks up a process by pid.
///
/// Returns [`None`] when the pid is not present in the current process table.
pub fn get_process(pid: u32) -> Option<Process> {
    let s = System::new_all();
    s.process(Pid::from_u32(pid)).map(|process| Process {
        pid,
        name: process.name().display().to_string(),
        args: process.cmd().iter().map(|a| a.display().to_string()).collect(),
    })
}

/// The result of a completed system command.
#[derive(Clone, Debug)]
pub struct CommandRanResult {
    /// The process exit status and captured stdout/stderr.
    pub output: std::process::Output,

    /// Wall-clock time spent spawning and waiting for the command.
    pub duration: Duration,
}

/// Options controlling system-command execution.
#[derive(Default)]
pub struct CommandOptions {
    /// Maximum wall-clock duration to wait before terminating the child.
    pub timeout: Option<Duration>,
}

/// Errors returned while running a system command.
#[derive(Error, Debug)]
pub enum CommandRanError {
    /// An operating-system I/O error occurred while spawning or waiting for the
    /// command.
    #[error("i/o error")]
    Io(#[from] std::io::Error),

    /// The command exceeded the configured timeout and was terminated.
    #[error("command timed out after {0:?}")]
    TimedOut(Duration),
}

/// Runs a system command and captures its output.
///
/// The command is spawned with stdout and stderr piped, matching
/// [`tokio::process::Command::output`] behavior. If a timeout is configured in
/// [`CommandOptions`], the child process is killed when the deadline expires and
/// [`CommandRanError::TimedOut`] is returned.
pub async fn run_system_command(cmd: &str, args: Vec<&str>, cmd_options: CommandOptions) -> Result<CommandRanResult, CommandRanError> {
    debug!("Running system command: {} args: {}", cmd, args.join(" "));
    let t0 = tokio::time::Instant::now();
    let mut command = tokio::process::Command::new(cmd);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    match command.spawn() {
        Ok(child) => {
            debug!("Spawned system command {} with args {:?}", cmd, args);
            let output = match cmd_options.timeout {
                Some(timeout) => match tokio::time::timeout(timeout, child.wait_with_output()).await {
                    Ok(output) => output,
                    Err(_elapsed) => {
                        let duration = t0.elapsed();
                        error!("cmd:{} args:{} timeout:{:?} duration:{:?}", cmd, args.join(" "), timeout, duration);
                        return Err(CommandRanError::TimedOut(timeout));
                    }
                },
                None => child.wait_with_output().await,
            };

            match output {
                Ok(output) => {
                    let duration = t0.elapsed();
                    info!("cmd:{} args:{} exit_code:{} duration:{:?}", cmd, args.join(" "), output.status, duration);
                    Ok(CommandRanResult { output, duration })
                }
                Err(wait_error) => {
                    let duration = t0.elapsed();
                    error!("cmd:{} args:{} error:{} duration:{:?}", cmd, args.join(" "), wait_error, duration);
                    Err(CommandRanError::Io(wait_error))
                }
            }
        }
        Err(spawn_err) => {
            error!("cmd:{} args:{} error:{}", cmd, args.join(" "), spawn_err);
            Err(CommandRanError::Io(spawn_err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_process_returns_current_process() {
        let pid = std::process::id();
        let process = match get_process(pid) {
            Some(process) => process,
            None => panic!("current process should be visible"),
        };

        assert_eq!(process.pid, pid);
        assert!(!process.name.is_empty());
    }

    #[test]
    fn get_process_returns_none_for_missing_pid() {
        assert_eq!(get_process(u32::MAX), None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_system_command_captures_stdout_and_stderr() {
        let result = match run_system_command("/bin/sh", vec!["-c", "printf 'hello'; printf 'warn' >&2"], CommandOptions::default()).await {
            Ok(result) => result,
            Err(err) => panic!("command should run successfully: {err}"),
        };

        assert!(result.output.status.success());
        assert_eq!(result.output.stdout, b"hello");
        assert_eq!(result.output.stderr, b"warn");
        assert!(result.duration > Duration::ZERO);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_system_command_returns_nonzero_status_without_error() {
        let result = match run_system_command("/bin/sh", vec!["-c", "exit 7"], CommandOptions::default()).await {
            Ok(result) => result,
            Err(err) => panic!("nonzero command should return output: {err}"),
        };

        assert_eq!(result.output.status.code(), Some(7));
    }

    #[tokio::test]
    async fn run_system_command_reports_spawn_errors() {
        let err = match run_system_command("becky-utils-command-that-should-not-exist", vec![], CommandOptions::default()).await {
            Ok(result) => panic!("missing command should fail, got {result:?}"),
            Err(err) => err,
        };

        assert!(matches!(err, CommandRanError::Io(_)));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_system_command_times_out() {
        let timeout = Duration::from_millis(25);
        let err = match run_system_command("/bin/sh", vec!["-c", "sleep 5"], CommandOptions { timeout: Some(timeout) }).await {
            Ok(result) => panic!("sleep command should time out, got {result:?}"),
            Err(err) => err,
        };

        assert!(matches!(err, CommandRanError::TimedOut(actual) if actual == timeout));
    }
}
