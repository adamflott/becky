use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};

use crate::comm::{try_connect_ctl_socket, try_connect_ga_socket, try_monitor_qemu_with_api};
use crate::handle::QemuHandle;
use crate::{
    AllocateError, ArchiveError, CallApiError, CreateError, QEMU_PID_FILENAME, QemuMachineConfiguration, QemuMachineConfigurationByArch, QemuSupportedArch,
    SpawnError, VerifyError,
};
use async_trait::async_trait;
use becky_engine::FxAccounting;
use becky_engine::control::FxControl;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::FxResourceConstraints;
use becky_engine::metadata::MetadataManager;
use becky_engine::state::{FxDesiredExecutionState, FxExecutionState, StateCollect, StatsCollect};
use becky_engine::storage::SysStorage;
use becky_engine::sys_conf::SystemConfiguration;
use becky_engine::verify::{FxVerify, Verification};
use becky_fx_id::FxId;
use becky_fx_system_command::FxSystemCommand;
use qapi::qga::QgaCommand;
use qapi::qmp::QmpCommand;
use qemu_command_builder::args::accel::Accel;
use qemu_command_builder::args::cpu::{CpuAarch64, CpuX86};
use qemu_command_builder::args::name::Name;
use qemu_command_builder::args::serial::SpecialDevice;
use qemu_command_builder::args::smp::SMP;
use qemu_command_builder::shell_string::ShellString;
use qemu_command_builder::to_command::ToCommand;
use qemu_command_builder::{QemuInstanceForAarch64, QemuInstanceForX86_64};
use sysinfo::{DiskUsage, Pid, System};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info, trace, warn};

pub const QEMU_WAIT_TIME_FOR_UDS_AVAILABLE_MS: u64 = 100;
pub const QEMU_WAIT_UDS_TIMEOUT_SECS: u64 = 3;
pub const QEMU_API_EVENT_BUFFER_SIZE: usize = 100;
const QEMU_ARCHIVE_DIR: &str = "archive";

fn vec_to_btreemap_shell_string(vs: Vec<(std::string::String, std::string::String)>) -> BTreeMap<ShellString, ShellString> {
    let mut btreemap = BTreeMap::new();
    for (k, v) in vs {
        btreemap.insert(ShellString::new(k), ShellString::new(v));
    }
    btreemap
}

pub struct QemuInstance {
    cmd: FxSystemCommand,
    qemu: QemuSupportedArch,
    fx_id: FxId,
    machine_configuration: QemuMachineConfiguration,
    system_configuration: SystemConfiguration,
    status: qapi::qmp::StatusInfo,
    qapi_event_tx: Sender<qapi::qmp::Event>,
    pub(crate) qapi_event_rx: Option<Receiver<qapi::qmp::Event>>,
    //qga_cmd_tx: Option<Sender<qapi::qga::QgaCommand>>,
}

impl Debug for QemuInstance {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QemuInstance")
            .field("cmd", &self.cmd)
            .field("qemu", &self.qemu)
            .field("fx_id", &self.fx_id)
            .field("machine_configuration", &self.machine_configuration)
            .field("system_configuration", &self.system_configuration)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Ord for QemuInstance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.fx_id.cmp(&other.fx_id)
    }
}

impl PartialOrd for QemuInstance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for QemuInstance {}

// TODO - compare machine_configuration as well
impl PartialEq for QemuInstance {
    fn eq(&self, other: &Self) -> bool {
        self.fx_id == other.fx_id
    }
}

impl QemuInstance {
    pub fn existing_or_new(
        system_configuration: &SystemConfiguration,
        machine_configuration: QemuMachineConfiguration,
        fx_id: &FxId,
    ) -> Result<Self, CreateError> {
        let qemu = qemu_from_machine_configuration(system_configuration, &machine_configuration, fx_id)?;
        let (qapi_event_tx, qapi_event_rx) = tokio::sync::mpsc::channel(QEMU_API_EVENT_BUFFER_SIZE);
        let mut cmd = command_from_qemu(&qemu);
        reconcile_existing_files(system_configuration, fx_id);

        if let Some(pid) = live_pid_from_pidfile(&QemuInstance::pid_path_s(&system_configuration.vm_root_path, fx_id)) {
            cmd.state = FxExecutionState::Running(pid);
            cmd.desired_state = FxDesiredExecutionState::Running;
        }

        Ok(Self {
            cmd,
            qemu,
            fx_id: fx_id.clone(),
            machine_configuration,
            system_configuration: system_configuration.clone(),
            status: qapi::qmp::StatusInfo {
                running: false,
                status: qapi::qmp::RunState::prelaunch,
            },
            qapi_event_tx,
            qapi_event_rx: Some(qapi_event_rx),
        })
    }

    pub(crate) fn take_qapi_event_rx(&mut self) -> Option<Receiver<qapi::qmp::Event>> {
        self.qapi_event_rx.take()
    }

    pub fn ctl_socket_path(&self) -> PathBuf {
        QemuInstance::ctl_socket_path_s(&self.system_configuration.vm_root_path, &self.fx_id)
    }
    pub fn ctl_socket_log_path(&self) -> PathBuf {
        QemuInstance::ctl_socket_log_path_s(&self.system_configuration.vm_root_path, &self.fx_id)
    }

    pub fn ctl_socket_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("run");
        mon_path.push("ctl.sock");
        mon_path
    }
    pub fn ctl_socket_log_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("log");
        mon_path.push("ctl.log");
        mon_path
    }

    pub fn pid_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("run");
        mon_path.push(QEMU_PID_FILENAME);
        mon_path
    }

    pub fn archive_root_path(&self) -> PathBuf {
        let mut path = self.system_configuration.vm_root_path.clone();
        path.push(self.fx_id.to_string());
        path.push(QEMU_ARCHIVE_DIR);
        path
    }

    pub fn ctl_debug_socket_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("run");
        mon_path.push("ctl_debug.sock");
        mon_path
    }

    pub fn ctl_debug_socket_log_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("log");
        mon_path.push("ctl_debug.log");
        mon_path
    }

    pub fn ctl_readline_socket_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("run");
        mon_path.push("ctl_readline.sock");
        mon_path
    }

    pub fn ctl_readline_socket_log_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("log");
        mon_path.push("ctl_readline.log");
        mon_path
    }

    pub fn ga_socket_path(&self) -> PathBuf {
        QemuInstance::ga_socket_path_s(&self.system_configuration.vm_root_path, &self.fx_id)
    }
    pub fn ga_socket_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("run");
        mon_path.push("ga.sock");
        mon_path
    }
    pub fn ga_socket_log_path_s(root: &Path, fx_id: &FxId) -> PathBuf {
        let mut mon_path = root.to_path_buf();
        mon_path.push(fx_id.to_string());
        mon_path.push("log");
        mon_path.push("guest_agent.log");
        mon_path
    }

    pub fn has_ga(&self) -> bool {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => qemu.common.enable_guest_agent,
            QemuMachineConfigurationByArch::Aarch64(qemu) => qemu.common.enable_guest_agent,
        }
    }

    pub async fn call_api<T: qapi::Command + QmpCommand + Debug>(&mut self, qemu_handle: &QemuHandle, cmd: T) -> Result<T::Ok, CallApiError> {
        trace!("qemu:api cmd:{:?}", &cmd);
        match qemu_handle.ctl.execute(cmd).await {
            Ok(result) => Ok(result),
            Err(err) => {
                error!("qemu:api:cmd error:{:#?}", err);
                Err(CallApiError::Executor(err))
            }
        }
    }

    pub async fn call_ga_api<T: qapi::Command + QgaCommand + Debug>(&mut self, qemu_handle: &QemuHandle, cmd: T) -> Result<T::Ok, CallApiError> {
        match &qemu_handle.ga {
            None => Err(CallApiError::NoGaAvailable),
            Some(qga_handle) => {
                debug!("qemu:qga cmd:{:?}", &cmd);
                match tokio::time::timeout(std::time::Duration::from_secs(3), qga_handle.execute(cmd)).await {
                    Ok(g) => match g {
                        Ok(result) => Ok(result),
                        Err(err) => {
                            error!("qemu:qga:cmd error:{:#?}", err);
                            Err(CallApiError::Executor(err))
                        }
                    },
                    Err(timeout_error) => Err(CallApiError::Timeout(timeout_error)),
                }
            }
        }
    }

    pub async fn try_attach_existing<T: MetadataManager>(&mut self, host_id: &HostId, fx_id: &FxId, mdt: &mut T) -> Result<Option<QemuHandle>, SpawnError> {
        let pidfile = QemuInstance::pid_path_s(&self.system_configuration.vm_root_path, &self.fx_id);
        let Some(pid) = live_pid_from_pidfile(&pidfile) else {
            reconcile_existing_files(&self.system_configuration, &self.fx_id);
            return Ok(None);
        };

        if !self.ctl_socket_path().exists() {
            warn!(
                "qemu:attach pidfile is live but qmp socket is missing pid:{} path:{}",
                pid,
                self.ctl_socket_path().display()
            );
            return Ok(None);
        }

        match try_connect_ctl_socket(&self.ctl_socket_path()).await {
            Ok((api, events)) => {
                let qapi_event_tx = self.qapi_event_tx.clone();
                let mut handle = try_monitor_qemu_with_api(api, events, qapi_event_tx, pidfile).await?;
                mdt.metadata_fx_state_update(host_id, fx_id, FxExecutionState::Running(handle.process.pid))
                    .await
                    .map_err(|_| SpawnError::Db)?;

                if self.has_ga() {
                    let (qga, _qga_handle) = try_connect_ga_socket(&self.ga_socket_path()).await.map_err(SpawnError::Timeout)?;
                    handle.ga = Some(qga);
                }

                Ok(Some(handle))
            }
            Err(timeout_err) => {
                debug!("qemu:attach qmp connection timed out pid:{} error:{:?}", pid, timeout_err);
                Ok(None)
            }
        }
    }
}

fn command_from_qemu(qemu: &QemuSupportedArch) -> FxSystemCommand {
    let mut args = qemu.to_command();
    let cmd = if args.is_empty() { String::new() } else { args.remove(0) };
    FxSystemCommand::new(cmd, args, FxDesiredExecutionState::Running)
}

fn live_pid_from_pidfile(pidfile: &Path) -> Option<u32> {
    let pid = std::fs::read_to_string(pidfile).ok()?.trim().parse::<u32>().ok()?;
    System::new_all().process(Pid::from_u32(pid)).is_some().then_some(pid)
}

fn reconcile_existing_files(system_configuration: &SystemConfiguration, fx_id: &FxId) {
    let pidfile = QemuInstance::pid_path_s(&system_configuration.vm_root_path, fx_id);
    if live_pid_from_pidfile(&pidfile).is_some() {
        return;
    }

    remove_stale_file(&pidfile);
    remove_stale_file(&QemuInstance::ctl_socket_path_s(&system_configuration.vm_root_path, fx_id));
    remove_stale_file(&QemuInstance::ga_socket_path_s(&system_configuration.vm_root_path, fx_id));
}

fn remove_stale_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => debug!("qemu:reconcile removed stale file:{}", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!("qemu:reconcile failed to remove stale file:{} error:{}", path.display(), err),
    }
}

fn state_from_qmp_status(status: &qapi::qmp::StatusInfo, pid: u32) -> FxExecutionState {
    match status.status {
        qapi::qmp::RunState::running => FxExecutionState::Running(pid),
        qapi::qmp::RunState::paused
        | qapi::qmp::RunState::debug
        | qapi::qmp::RunState::inmigrate
        | qapi::qmp::RunState::postmigrate
        | qapi::qmp::RunState::finish_migrate
        | qapi::qmp::RunState::restore_vm
        | qapi::qmp::RunState::save_vm
        | qapi::qmp::RunState::suspended
        | qapi::qmp::RunState::colo => FxExecutionState::Paused(pid),
        qapi::qmp::RunState::prelaunch => FxExecutionState::NotStarted,
        qapi::qmp::RunState::shutdown => FxExecutionState::Stopped,
        qapi::qmp::RunState::internal_error | qapi::qmp::RunState::io_error | qapi::qmp::RunState::watchdog | qapi::qmp::RunState::guest_panicked => {
            FxExecutionState::Error(format!("{:?}", status.status))
        }
    }
}

fn process_metric<T>(pid: u32, fallback: T, f: impl FnOnce(&sysinfo::Process) -> T) -> T {
    let system = System::new_all();
    match system.process(Pid::from_u32(pid)) {
        Some(process) => f(process),
        None => fallback,
    }
}

fn empty_disk_usage() -> DiskUsage {
    DiskUsage {
        total_written_bytes: 0,
        written_bytes: 0,
        total_read_bytes: 0,
        read_bytes: 0,
    }
}

fn parse_special_device(value: String) -> Result<SpecialDevice, CreateError> {
    value.parse::<SpecialDevice>().map_err(CreateError::Config)
}

fn parse_name(value: &str) -> Result<Name, CreateError> {
    value.parse::<Name>().map_err(|err| CreateError::Config(err.to_string()))
}

fn qemu_from_machine_configuration(
    system_configuration: &SystemConfiguration,
    machine_configuration: &QemuMachineConfiguration,
    fx_id: &FxId,
) -> Result<QemuSupportedArch, CreateError> {
    match &machine_configuration.conf {
        QemuMachineConfigurationByArch::Amd64(machine) => {
            let mut qemu = QemuInstanceForX86_64::builder().qemu_binary(PathBuf::from("qemu-system-x86_64")).build();
            qemu.cpu = Some(CpuX86::new(machine.cpu.clone()));
            qemu.smp = Some(SMP::new(machine.cpus));
            qemu.m = Some(machine.common.memory.clone());
            qemu.accel = Some(Accel::new(machine.common.accel_type.clone()));
            configure_common_qemu_args(system_configuration, machine_configuration, fx_id, &machine.common, &mut qemu)?;
            Ok(QemuSupportedArch::X86_64(qemu))
        }
        QemuMachineConfigurationByArch::Aarch64(machine) => {
            let mut qemu = QemuInstanceForAarch64::builder().qemu_binary(PathBuf::from("qemu-system-aarch64")).build();
            qemu.cpu = Some(CpuAarch64 { cpu_type: machine.cpu.clone() });
            qemu.smp = Some(SMP::new(machine.cpus));
            qemu.m = Some(machine.common.memory.clone());
            qemu.accel = Some(Accel::new(machine.common.accel_type.clone()));
            configure_common_qemu_args(system_configuration, machine_configuration, fx_id, &machine.common, &mut qemu)?;
            Ok(QemuSupportedArch::Aarch64(qemu))
        }
    }
}

fn configure_common_qemu_args<Machine, Cpu>(
    system_configuration: &SystemConfiguration,
    machine_configuration: &QemuMachineConfiguration,
    fx_id: &FxId,
    common: &crate::QemuCommonOptions,
    qemu: &mut qemu_command_builder::QemuInstanceBase<Machine, Cpu>,
) -> Result<(), CreateError> {
    qemu.name = Some(parse_name(&machine_configuration.name)?);
    qemu.pidfile = Some(QemuInstance::pid_path_s(&system_configuration.vm_root_path, fx_id));
    qemu.qmp = Some(parse_special_device(format!(
        "unix:{},server=on,wait=off",
        QemuInstance::ctl_socket_path_s(&system_configuration.vm_root_path, fx_id).display()
    ))?);
    qemu.kernel = common.kernel.clone();
    qemu.initrd = common.initrd.clone();
    qemu.snapshot = common.snapshot;
    Ok(())
}

#[async_trait]
impl FxControl for QemuInstance {
    type Id = FxId;

    fn id(&self) -> Self::Id {
        self.fx_id.clone()
    }

    type FxAllocateResult = ();
    type FxAllocateError = AllocateError;
    async fn fx_allocate<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        let mut root_path = PathBuf::from(&self.system_configuration.vm_root_path);
        root_path.push(self.fx_id.to_string());
        tokio::fs::create_dir_all(&root_path).await?;
        for dir in ["log", "run"] {
            let mut new_path = root_path.clone();
            new_path.push(dir);
            trace!("qemu:spawn creating dir:{}", &new_path.display());
            tokio::fs::create_dir_all(&new_path).await?;
        }

        let mut data_root_path = PathBuf::from(&self.system_configuration.vm_data_root_path);
        data_root_path.push(self.fx_id.to_string());
        tokio::fs::create_dir_all(&data_root_path).await?;

        match storage.sys_storage_create(&self.system_configuration, host_id, mdt, fx_id, rc).await {
            Ok(result) => {
                info!("qemu:instance:allocate result:success {:?}", result);
                Ok(())
            }
            Err(err) => {
                error!("qemu:instance:allocate result:{:?}", err);
                Err(AllocateError::StorageNotOk)
            }
        }
    }

    type FxSpawnResult = QemuHandle;
    type FxSpawnError = SpawnError;

    async fn fx_start<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError> {
        if let Some(handle) = self.try_attach_existing(host_id, fx_id, mdt).await? {
            return Ok(handle);
        }

        let root_path = PathBuf::from(&self.system_configuration.vm_root_path);

        let mut qemu_stdout_path = root_path.clone();
        qemu_stdout_path.push("log");
        qemu_stdout_path.push("qemu_stdout.log");
        let _qemu_stdout_file = tokio::fs::File::create(qemu_stdout_path).await?;

        let mut qemu_stderr_path = root_path.clone();
        qemu_stderr_path.push("log");
        qemu_stderr_path.push("qemu_stderr.log");
        let _qemu_stderr_file = tokio::fs::File::create(qemu_stderr_path).await?;

        self.cmd = command_from_qemu(&self.qemu);

        match self.cmd.fx_start(host_id, fx_id, mdt, rc, storage).await {
            Ok(_x) => {
                match try_connect_ctl_socket(&self.ctl_socket_path()).await {
                    Ok((api, events)) => {
                        let pidfile = QemuInstance::pid_path_s(&self.system_configuration.vm_root_path, &self.fx_id);
                        let qapi_event_tx = self.qapi_event_tx.clone();

                        match try_monitor_qemu_with_api(api, events, qapi_event_tx, pidfile).await {
                            Ok(mut handle) => {
                                match mdt
                                    .metadata_fx_state_update(host_id, fx_id, FxExecutionState::Running(handle.process.pid))
                                    .await
                                {
                                    Ok(_) => {
                                        if self.has_ga() {
                                            match try_connect_ga_socket(&self.ga_socket_path()).await {
                                                Ok((qga, _qga_handle)) => {
                                                    // TODO does this fail because qga_handle is dropped?
                                                    handle.ga = Some(qga);
                                                    Ok(handle)
                                                }
                                                Err(qga_connect_timed_out) => {
                                                    error!("qemu:spawn guest_agent timed out {}", qga_connect_timed_out);
                                                    Err(SpawnError::Timeout(qga_connect_timed_out))
                                                }
                                            }
                                        } else {
                                            Ok(handle)
                                        }
                                    }
                                    Err(metadata_update_err) => {
                                        error!("qemu:spawn metadata:update error:{:?}", metadata_update_err);
                                        Err(SpawnError::Db)
                                    }
                                }
                            }
                            Err(spawn_err) => {
                                error!("qemu:spawn metadata:update error:{:?}", spawn_err);
                                Err(spawn_err)
                            }
                        }
                    }
                    Err(timeout_err) => {
                        error!("qemu:spawn timed out spawning+monitoring:{:?}", timeout_err);
                        Err(SpawnError::Timeout(timeout_err))
                    }
                }
            }
            Err(err) => {
                error!("qemu:spawn system command failed:{:?}", err);
                Err(SpawnError::SystemCommand(err))
            }
        }
    }

    type FxStatusResult = FxExecutionState;
    type FxStatusError = CallApiError;

    async fn fx_status(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError> {
        match self.call_api(handle, qapi::qmp::query_status {}).await {
            Ok(status) => {
                debug!("qemu:operation status info:{:?}", &status);
                let state = state_from_qmp_status(&status, handle.process.pid);
                self.status = status;
                Ok(state)
            }
            Err(status_err) => {
                error!("qemu:operation status error:{:?}", status_err);
                self.status = qapi::qmp::StatusInfo {
                    running: false,
                    status: qapi::qmp::RunState::io_error,
                };
                Err(status_err)
            }
        }
    }

    type FxStopResult = ();
    type FxStopError = ();

    async fn fx_stop(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        let x = self.call_api(handle, qapi::qmp::system_powerdown {}).await;
        info!("qemu:operation stop {:#?}", x);
        Ok(())
    }

    type FxDestroyResult = ();
    type FxDestroyError = CallApiError;

    async fn fx_destroy(&self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError> {
        fnr.ctl.execute(qapi::qmp::quit {}).await?;
        Ok(())
    }

    type FxArchiveResult = PathBuf;
    type FxArchiveError = ArchiveError;

    async fn fx_archive(&self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxArchiveResult, Self::FxArchiveError> {
        let archive_root = self.archive_root_path();
        tokio::fs::create_dir_all(&archive_root).await?;
        let archive_path = archive_root.join("vmstate.snap");
        let tag = format!("becky-{}", self.fx_id);
        fnr.ctl
            .execute(qapi::qmp::snapshot_save {
                devices: vec![],
                job_id: tag.clone(),
                tag,
                vmstate: archive_path.display().to_string(),
            })
            .await
            .map_err(CallApiError::Executor)?;
        Ok(archive_path)
    }
}
#[async_trait]
impl FxVerify for QemuInstance {
    type FxOpVerifyError = VerifyError;

    async fn fx_op_verify(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Verification, Self::FxOpVerifyError> {
        if System::new_all().process(Pid::from_u32(handle.process.pid)).is_none() {
            return Ok(Verification::Unknown);
        }

        let status = self.call_api(handle, qapi::qmp::query_status {}).await?;
        self.status = status;
        Ok(Verification::Match)
    }
}

#[async_trait]
impl StatsCollect for QemuInstance {
    type FxStatCollectResult = ();
    type FxStatCollectError = CallApiError;

    async fn stat_collect(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStatCollectResult, Self::FxStatCollectError> {
        let _ = self.call_api(handle, qapi::qmp::query_block {}).await?;
        Ok(())
    }
}

#[async_trait]
impl StateCollect for QemuInstance {
    type FxStateCollectResult = FxExecutionState;
    type FxStateCollectError = CallApiError;

    async fn state_collect(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStateCollectResult, Self::FxStateCollectError> {
        let status = self.call_api(handle, qapi::qmp::query_status {}).await?;
        let state = state_from_qmp_status(&status, handle.process.pid);
        self.status = status;
        Ok(state)
    }
}
#[async_trait]
impl FxAccounting for QemuInstance {
    type Instance = QemuHandle;

    async fn accumulated_cpu_time(&self, i: &Self::Instance) -> u64 {
        process_metric(i.process.pid, 0, |process| process.accumulated_cpu_time())
    }

    async fn disk_usage(&self, i: &Self::Instance) -> DiskUsage {
        process_metric(i.process.pid, empty_disk_usage(), |process| process.disk_usage())
    }

    async fn memory(&self, i: &Self::Instance) -> u64 {
        process_metric(i.process.pid, 0, |process| process.memory())
    }

    async fn virtual_memory(&self, i: &Self::Instance) -> u64 {
        process_metric(i.process.pid, 0, |process| process.virtual_memory())
    }

    async fn run_time(&self, i: &Self::Instance) -> u64 {
        process_metric(i.process.pid, 0, |process| process.run_time())
    }
}
