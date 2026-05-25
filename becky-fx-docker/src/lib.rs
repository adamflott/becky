//! Docker container integration for the Becky engine.
//!
//! This provider controls a Docker container by name. It can attach to an
//! existing container, or create one from a configured image. Image availability
//! is handled in `fx_allocate()` so startup does not need to download layers.

use std::fmt::{Debug, Formatter};
use std::process::{ExitStatus, Stdio};
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
use becky_utils::{CommandOptions, CommandRanError, run_system_command};
use serde::Deserialize;
use sysinfo::DiskUsage;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

const DOCKER_BIN: &str = "docker";
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const DOCKER_PULL_TIMEOUT: Duration = Duration::from_secs(600);
const DOCKER_MONITOR_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A Docker network name used by a container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerNetwork(String);

impl DockerNetwork {
    /// Creates a Docker network reference from a network name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Returns the Docker network name.
    pub fn name(&self) -> &str {
        &self.0
    }
}

/// A Docker container managed as a Becky effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FxContainerDocker {
    name: String,
    image: Option<String>,
    command: Vec<String>,
    env: Vec<(String, String)>,
    network: Option<DockerNetwork>,
}

impl FxContainerDocker {
    /// Creates a Docker provider for an existing named container.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: None,
            command: vec![],
            env: vec![],
            network: None,
        }
    }

    /// Creates a Docker provider that can create the named container from an
    /// image when it does not already exist.
    pub fn from_image(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: Some(image.into()),
            command: vec![],
            env: vec![],
            network: None,
        }
    }

    /// Creates a Docker provider for an existing named container attached to a
    /// named Docker network.
    pub fn with_network(name: impl Into<String>, network: DockerNetwork) -> Self {
        Self {
            name: name.into(),
            image: None,
            command: vec![],
            env: vec![],
            network: Some(network),
        }
    }

    /// Sets the image used to create the container if it does not exist.
    pub fn set_image(&mut self, image: impl Into<String>) {
        self.image = Some(image.into());
    }

    /// Returns a copy of this provider with an image configured.
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.set_image(image);
        self
    }

    /// Sets the command appended after the image in `docker create`.
    pub fn set_command<I, S>(&mut self, command: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = command.into_iter().map(Into::into).collect();
    }

    /// Returns a copy of this provider with a container command configured.
    pub fn command<I, S>(mut self, command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_command(command);
        self
    }

    /// Adds an environment variable passed to `docker create --env`.
    pub fn add_env(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.env.push((name.into(), value.into()));
    }

    /// Returns a copy of this provider with an environment variable added.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.add_env(name, value);
        self
    }

    /// Sets the network used by `fx_allocate()` and `docker create`.
    pub fn set_network(&mut self, network: DockerNetwork) {
        self.network = Some(network);
    }

    /// Returns a copy of this provider attached to a Docker network.
    pub fn networked(mut self, network: DockerNetwork) -> Self {
        self.set_network(network);
        self
    }

    /// Returns the managed container name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the configured image, if this provider can create containers.
    pub fn image_ref(&self) -> Option<&str> {
        self.image.as_deref()
    }

    /// Returns the configured container command.
    pub fn command_args(&self) -> &[String] {
        &self.command
    }

    /// Returns configured environment variables.
    pub fn env_vars(&self) -> &[(String, String)] {
        &self.env
    }

    /// Returns the configured Docker network, if any.
    pub fn network(&self) -> Option<&DockerNetwork> {
        self.network.as_ref()
    }

    async fn inspect(&self) -> Result<DockerInspectContainer, FxDockerError> {
        inspect_container(&self.name).await
    }

    async fn create_container(&self) -> Result<(), FxDockerError> {
        let image = self.image.as_deref().ok_or(FxDockerError::MissingImage)?;
        let args = docker_create_args(&self.name, image, self.network.as_ref(), &self.env, &self.command);
        info!("docker:create container:{} image:{}", self.name, image);
        docker_owned(&args, DOCKER_COMMAND_TIMEOUT).await.map(|_| ())
    }
}

/// Handle for a Docker container started or attached by this provider.
pub struct DockerContainerHandle {
    /// Docker container id.
    pub id: String,

    /// Docker container name.
    pub name: String,

    /// Whether this invocation created the container.
    pub created: bool,

    latest_state: Arc<RwLock<FxExecutionState>>,
    cancel_monitor: Sender<()>,
    monitor: Option<JoinHandle<()>>,
}

impl Debug for DockerContainerHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerContainerHandle")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("created", &self.created)
            .field("monitor_finished", &self.monitor.as_ref().is_none_or(JoinHandle::is_finished))
            .finish_non_exhaustive()
    }
}

impl DockerContainerHandle {
    /// Returns the latest state observed by the monitor task.
    pub async fn latest_state(&self) -> FxExecutionState {
        self.latest_state.read().await.clone()
    }

    /// Requests monitor shutdown and waits for the monitor task to finish.
    ///
    /// This does not stop the Docker container.
    pub async fn stop_monitor(&mut self) {
        let _ = self.cancel_monitor.send(()).await;
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.await;
        }
    }
}

/// Errors returned while controlling Docker.
#[derive(Debug, Error)]
pub enum FxDockerError {
    /// Docker CLI process failed to spawn or timed out.
    #[error("docker command failed: {0}")]
    Command(#[from] CommandRanError),

    /// Docker CLI exited unsuccessfully.
    #[error("docker {command} exited with {status}: {stderr}")]
    DockerExit { command: String, status: ExitStatus, stderr: String },

    /// Docker returned JSON this crate could not parse.
    #[error("failed to parse docker output for {command}: {source}")]
    Json { command: String, source: serde_json::Error },

    /// Docker did not return the expected field.
    #[error("docker {command} did not return {field}")]
    MissingField { command: String, field: &'static str },

    /// Container creation was requested without an image.
    #[error("container {0} does not exist and no image is configured to create it")]
    ContainerMissingNoImage(String),

    /// A create operation needs an image but none is configured.
    #[error("docker image is required to create a missing container")]
    MissingImage,
}

#[derive(Clone, Debug, Deserialize)]
struct DockerInspectContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "State")]
    state: DockerInspectState,
}

#[derive(Clone, Debug, Deserialize)]
struct DockerInspectState {
    #[serde(rename = "Pid")]
    pid: u32,
    #[serde(rename = "Running")]
    running: bool,
    #[serde(rename = "Paused")]
    paused: bool,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "StartedAt")]
    started_at: String,
}

#[derive(Clone, Debug, Deserialize)]
struct DockerStats {
    #[serde(rename = "MemUsage")]
    mem_usage: String,
    #[serde(rename = "BlockIO")]
    block_io: String,
}

async fn docker_with_timeout(args: &[&str], timeout: Duration) -> Result<Vec<u8>, FxDockerError> {
    let result = run_system_command(DOCKER_BIN, args.to_vec(), CommandOptions { timeout: Some(timeout) }).await?;
    if result.output.status.success() {
        Ok(result.output.stdout)
    } else {
        Err(FxDockerError::DockerExit {
            command: args.join(" "),
            status: result.output.status,
            stderr: String::from_utf8_lossy(&result.output.stderr).trim().to_string(),
        })
    }
}

async fn docker(args: &[&str]) -> Result<Vec<u8>, FxDockerError> {
    docker_with_timeout(args, DOCKER_COMMAND_TIMEOUT).await
}

async fn docker_owned(args: &[String], timeout: Duration) -> Result<Vec<u8>, FxDockerError> {
    let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
    docker_with_timeout(&borrowed, timeout).await
}

async fn docker_ok(args: &[&str]) -> Result<(), FxDockerError> {
    docker(args).await.map(|_| ())
}

async fn inspect_container(name: &str) -> Result<DockerInspectContainer, FxDockerError> {
    let output = docker(&["container", "inspect", name]).await?;
    let mut inspected = serde_json::from_slice::<Vec<DockerInspectContainer>>(&output).map_err(|source| FxDockerError::Json {
        command: "container inspect".to_string(),
        source,
    })?;
    inspected.pop().ok_or(FxDockerError::MissingField {
        command: "container inspect".to_string(),
        field: "container",
    })
}

async fn container_exists(name: &str) -> Result<bool, FxDockerError> {
    match docker_ok(&["container", "inspect", name]).await {
        Ok(()) => Ok(true),
        Err(err @ FxDockerError::DockerExit { .. }) if docker_not_found(&err, DockerResourceKind::Container) => Ok(false),
        Err(err) => Err(err),
    }
}

async fn image_exists(image: &str) -> Result<bool, FxDockerError> {
    match docker_ok(&["image", "inspect", image]).await {
        Ok(()) => Ok(true),
        Err(err @ FxDockerError::DockerExit { .. }) if docker_not_found(&err, DockerResourceKind::Image) => Ok(false),
        Err(err) => Err(err),
    }
}

async fn ensure_image(image: &str) -> Result<(), FxDockerError> {
    if image_exists(image).await? {
        Ok(())
    } else {
        info!("docker:pull image:{}", image);
        docker_with_timeout(&["pull", image], DOCKER_PULL_TIMEOUT).await.map(|_| ())
    }
}

async fn network_exists(name: &str) -> Result<bool, FxDockerError> {
    match docker_ok(&["network", "inspect", name]).await {
        Ok(()) => Ok(true),
        Err(err @ FxDockerError::DockerExit { .. }) if docker_not_found(&err, DockerResourceKind::Network) => Ok(false),
        Err(err) => Err(err),
    }
}

async fn ensure_network(name: &str) -> Result<(), FxDockerError> {
    if network_exists(name).await? {
        Ok(())
    } else {
        docker_ok(&["network", "create", name]).await
    }
}

fn trim_docker_name(name: &str) -> String {
    name.strip_prefix('/').unwrap_or(name).to_string()
}

enum DockerResourceKind {
    Container,
    Image,
    Network,
}

fn docker_not_found(err: &FxDockerError, kind: DockerResourceKind) -> bool {
    let FxDockerError::DockerExit { stderr, .. } = err else {
        return false;
    };

    let stderr = stderr.to_ascii_lowercase();
    match kind {
        DockerResourceKind::Container => stderr.contains("no such container"),
        DockerResourceKind::Image => stderr.contains("no such image") || stderr.contains("no such object"),
        DockerResourceKind::Network => stderr.contains("no such network"),
    }
}

fn inspect_to_state(inspect: &DockerInspectContainer) -> FxExecutionState {
    if inspect.state.running {
        FxExecutionState::Running(inspect.state.pid)
    } else if inspect.state.paused {
        FxExecutionState::Paused(inspect.state.pid)
    } else if inspect.state.status.eq_ignore_ascii_case("exited") || inspect.state.status.eq_ignore_ascii_case("created") {
        FxExecutionState::Stopped
    } else {
        FxExecutionState::Unknown
    }
}

fn monitor_handle(inspect: DockerInspectContainer, created: bool) -> DockerContainerHandle {
    let name = trim_docker_name(&inspect.name);
    let latest_state = Arc::new(RwLock::new(inspect_to_state(&inspect)));
    let (cancel_monitor, cancel_rx) = tokio::sync::mpsc::channel(1);
    let monitor = tokio::spawn(monitor_container(name.clone(), latest_state.clone(), cancel_rx));

    DockerContainerHandle {
        id: inspect.id,
        name,
        created,
        latest_state,
        cancel_monitor,
        monitor: Some(monitor),
    }
}

async fn monitor_container(name: String, latest_state: Arc<RwLock<FxExecutionState>>, mut cancel_rx: tokio::sync::mpsc::Receiver<()>) {
    loop {
        match inspect_container(&name).await {
            Ok(inspect) => {
                let state = inspect_to_state(&inspect);
                *latest_state.write().await = state.clone();

                if matches!(state, FxExecutionState::Running(_)) {
                    match wait_for_container_stop(&name, &mut cancel_rx).await {
                        MonitorWaitAction::Stopped => match inspect_container(&name).await {
                            Ok(inspect) => {
                                *latest_state.write().await = inspect_to_state(&inspect);
                            }
                            Err(err) => {
                                warn!("docker:monitor inspect after wait failed container:{} error:{}", name, err);
                                *latest_state.write().await = FxExecutionState::Unknown;
                            }
                        },
                        MonitorWaitAction::Cancelled => break,
                        MonitorWaitAction::Retry => {
                            tokio::time::sleep(DOCKER_MONITOR_POLL_INTERVAL).await;
                        }
                    }
                } else {
                    tokio::select! {
                        _ = cancel_rx.recv() => break,
                        _ = tokio::time::sleep(DOCKER_MONITOR_POLL_INTERVAL) => {}
                    }
                }
            }
            Err(err) => {
                warn!("docker:monitor inspect failed container:{} error:{}", name, err);
                *latest_state.write().await = FxExecutionState::Unknown;
                tokio::select! {
                    _ = cancel_rx.recv() => break,
                    _ = tokio::time::sleep(DOCKER_MONITOR_POLL_INTERVAL) => {}
                }
            }
        }
    }
    debug!("docker:monitor stopped container:{}", name);
}

enum MonitorWaitAction {
    Stopped,
    Cancelled,
    Retry,
}

async fn wait_for_container_stop(name: &str, cancel_rx: &mut tokio::sync::mpsc::Receiver<()>) -> MonitorWaitAction {
    let mut child = match tokio::process::Command::new(DOCKER_BIN)
        .args(["wait", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            error!("docker:monitor wait spawn failed container:{} error:{}", name, err);
            return MonitorWaitAction::Retry;
        }
    };

    tokio::select! {
        _ = cancel_rx.recv() => {
            let _ = child.kill().await;
            MonitorWaitAction::Cancelled
        }
        wait_result = child.wait() => {
            match wait_result {
                Ok(status) if status.success() => MonitorWaitAction::Stopped,
                Ok(status) => {
                    warn!("docker:monitor wait exited unsuccessfully container:{} status:{}", name, status);
                    MonitorWaitAction::Retry
                }
                Err(err) => {
                    warn!("docker:monitor wait failed container:{} error:{}", name, err);
                    MonitorWaitAction::Retry
                }
            }
        }
    }
}

fn docker_create_args(name: &str, image: &str, network: Option<&DockerNetwork>, env: &[(String, String)], command: &[String]) -> Vec<String> {
    let mut args = vec!["create".to_string(), "--name".to_string(), name.to_string()];
    if let Some(network) = network {
        args.push("--network".to_string());
        args.push(network.name().to_string());
    }
    for (key, value) in env {
        args.push("--env".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(image.to_string());
    args.extend(command.iter().cloned());
    args
}

async fn container_stats(name: &str) -> Result<DockerStats, FxDockerError> {
    let output = docker(&["stats", "--no-stream", "--format", "{{json .}}", name]).await?;
    serde_json::from_slice::<DockerStats>(&output).map_err(|source| FxDockerError::Json {
        command: "stats".to_string(),
        source,
    })
}

fn parse_docker_size_bytes(size: &str) -> Option<u64> {
    let size = size.trim();
    let split_at = size.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = size.split_at(split_at);
    let value = number.parse::<f64>().ok()?;
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "pb" | "pib" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

fn parse_first_docker_size_bytes(value: &str) -> u64 {
    value.split('/').next().and_then(parse_docker_size_bytes).unwrap_or(0)
}

fn parse_block_io(block_io: &str) -> DiskUsage {
    let mut parts = block_io.split('/');
    let read_bytes = parts.next().and_then(parse_docker_size_bytes).unwrap_or(0);
    let written_bytes = parts.next().and_then(parse_docker_size_bytes).unwrap_or(0);
    DiskUsage {
        total_written_bytes: written_bytes,
        written_bytes,
        total_read_bytes: read_bytes,
        read_bytes,
    }
}

#[async_trait]
impl FxControl for FxContainerDocker {
    type Id = String;

    fn id(&self) -> Self::Id {
        self.name.clone()
    }

    type FxAllocateResult = ();
    type FxAllocateError = FxDockerError;

    async fn fx_allocate<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        if let Some(network) = &self.network {
            ensure_network(network.name()).await?;
        }
        if let Some(image) = &self.image {
            ensure_image(image).await?;
        }
        Ok(())
    }

    type FxSpawnResult = DockerContainerHandle;
    type FxSpawnError = FxDockerError;

    async fn fx_start<T: MetadataManager>(
        &mut self,
        _host_id: &HostId,
        _fx_id: &FxId,
        _mdt: &mut T,
        _rc: &impl FxResourceConstraints,
        _storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError> {
        if let Some(network) = &self.network {
            ensure_network(network.name()).await?;
        }

        let created = if !container_exists(&self.name).await? {
            if let Some(image) = &self.image {
                ensure_image(image).await?;
                self.create_container().await?;
                true
            } else {
                return Err(FxDockerError::ContainerMissingNoImage(self.name.clone()));
            }
        } else {
            false
        };

        let inspect = self.inspect().await?;
        if !inspect.state.running {
            info!("docker:start container:{}", self.name);
            docker_ok(&["start", &self.name]).await?;
        } else {
            debug!("docker:start container:{} already running", self.name);
        }

        Ok(monitor_handle(self.inspect().await?, created))
    }

    type FxStatusResult = FxExecutionState;
    type FxStatusError = FxDockerError;

    async fn fx_status(&mut self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError> {
        match self.inspect().await {
            Ok(inspect) => {
                let state = inspect_to_state(&inspect);
                *fnr.latest_state.write().await = state.clone();
                Ok(state)
            }
            Err(err) => {
                warn!("docker:status inspect failed container:{} error:{}", self.name, err);
                Err(err)
            }
        }
    }

    type FxStopResult = ();
    type FxStopError = FxDockerError;

    async fn fx_stop(&mut self, _fnr: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        docker_ok(&["stop", &self.name]).await
    }

    type FxDestroyResult = ();
    type FxDestroyError = FxDockerError;

    async fn fx_destroy(&self, _fnr: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError> {
        _fnr.stop_monitor().await;
        docker_ok(&["rm", "--force", &self.name]).await
    }

    type FxArchiveResult = String;
    type FxArchiveError = FxDockerError;

    async fn fx_archive(&self, _fnr: &mut Self::FxSpawnResult) -> Result<Self::FxArchiveResult, Self::FxArchiveError> {
        let image_ref = format!("becky-archive:{}", self.name);
        docker_ok(&["commit", &self.name, &image_ref]).await?;
        Ok(image_ref)
    }
}

#[async_trait]
impl FxAccounting for FxContainerDocker {
    type Instance = DockerContainerHandle;

    async fn accumulated_cpu_time(&self, _: &Self::Instance) -> u64 {
        0
    }

    async fn disk_usage(&self, _: &Self::Instance) -> DiskUsage {
        match container_stats(&self.name).await {
            Ok(stats) => parse_block_io(&stats.block_io),
            Err(_) => DiskUsage {
                total_written_bytes: 0,
                written_bytes: 0,
                total_read_bytes: 0,
                read_bytes: 0,
            },
        }
    }

    async fn memory(&self, _: &Self::Instance) -> u64 {
        match container_stats(&self.name).await {
            Ok(stats) => parse_first_docker_size_bytes(&stats.mem_usage),
            Err(_) => 0,
        }
    }

    async fn virtual_memory(&self, i: &Self::Instance) -> u64 {
        self.memory(i).await
    }

    async fn run_time(&self, _: &Self::Instance) -> u64 {
        match self.inspect().await {
            Ok(inspect) => {
                if inspect.state.running && !inspect.state.started_at.starts_with("0001-") {
                    // Docker returns RFC3339 timestamps with nanoseconds. Avoid adding
                    // another time dependency for now; expose presence as nonzero.
                    1
                } else {
                    0
                }
            }
            Err(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_leading_slash_from_docker_name() {
        assert_eq!(trim_docker_name("/worker"), "worker");
        assert_eq!(trim_docker_name("worker"), "worker");
    }

    #[test]
    fn parses_docker_size_units() {
        assert_eq!(parse_docker_size_bytes("1B"), Some(1));
        assert_eq!(parse_docker_size_bytes("1KiB"), Some(1024));
        assert_eq!(parse_docker_size_bytes("1.5MiB"), Some(1_572_864));
        assert_eq!(parse_docker_size_bytes("2GB"), Some(2_147_483_648));
    }

    #[test]
    fn parses_first_size_from_docker_pair() {
        assert_eq!(parse_first_docker_size_bytes("12MiB / 128MiB"), 12 * 1024 * 1024);
    }

    #[test]
    fn parses_block_io_into_disk_usage() {
        let usage = parse_block_io("1.5MiB / 2MiB");
        assert_eq!(usage.read_bytes, 1_572_864);
        assert_eq!(usage.written_bytes, 2 * 1024 * 1024);
        assert_eq!(usage.total_read_bytes, usage.read_bytes);
        assert_eq!(usage.total_written_bytes, usage.written_bytes);
    }

    #[test]
    fn builds_minimal_docker_create_args() {
        assert_eq!(
            docker_create_args("worker", "alpine:latest", None, &[], &[]),
            vec!["create", "--name", "worker", "alpine:latest"]
        );
    }

    #[test]
    fn builds_docker_create_args_with_network_env_and_command() {
        let args = docker_create_args(
            "worker",
            "alpine:latest",
            Some(&DockerNetwork::new("becky")),
            &[("RUST_LOG".to_string(), "debug".to_string())],
            &["sh".to_string(), "-c".to_string(), "sleep 60".to_string()],
        );

        assert_eq!(
            args,
            vec![
                "create",
                "--name",
                "worker",
                "--network",
                "becky",
                "--env",
                "RUST_LOG=debug",
                "alpine:latest",
                "sh",
                "-c",
                "sleep 60"
            ]
        );
    }
}
