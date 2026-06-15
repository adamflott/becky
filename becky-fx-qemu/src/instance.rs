use std::cmp::Ordering;
use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};

use crate::comm::{try_connect_ctl_socket, try_connect_ga_socket, try_monitor_qemu_with_api};
use crate::handle::QemuHandle;
use crate::utils::convert_cmd_line_to_qemu_instance;
use crate::{
    AllocateError, ArchiveError, CallApiError, CreateError, QEMU_IMG_FORMAT_QCOW2, QEMU_METADATA_PROVIDER, QEMU_PID_FILENAME, QemuArchivePolicy,
    QemuDesiredArchitecture, QemuDesiredCommonOptions, QemuDesiredMetadataRecord, QemuMachineConfiguration, QemuMachineConfigurationAarch64,
    QemuMachineConfigurationAmd64, QemuMachineConfigurationByArch, QemuMetadataRecord, QemuStorageType, QemuSupportedArch, SpawnError, VerifyError,
    cloud_image_overlay_path, default_common_options, iso_dest_path, qemu_disk_format_arg, qemu_disk_path,
};
use async_trait::async_trait;
use becky_engine::FxAccounting;
use becky_engine::boot_methods::BootMethod;
use becky_engine::control::FxControl;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::{BootStrapMethod, FxResourceConstraints, NetworkingConfiguration};
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
use qemu_command_builder::args::chardev::{CharDev, CharSocket, CharSocketUds};
use qemu_command_builder::args::cpu::{CpuAarch64, CpuX86};
use qemu_command_builder::args::cpu_type::{CpuTypeAarch64, CpuTypeX86_64};
use qemu_command_builder::args::device::Device;
use qemu_command_builder::args::drive::{Drive, DriveInterface, DriveMedia};
use qemu_command_builder::args::name::Name;
use qemu_command_builder::args::netdev::{NetDev, User};
use qemu_command_builder::args::serial::SpecialDevice;
use qemu_command_builder::args::smp::SMP;
use qemu_command_builder::common::{AccelType, OnOff};
use qemu_command_builder::shell_path::ShellPath;
use qemu_command_builder::shell_string::ShellString;
use qemu_command_builder::to_command::{ToArg, ToCommand};
use qemu_command_builder::{QemuInstanceForAarch64, QemuInstanceForX86_64};
use sysinfo::{DiskUsage, Pid, System};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info, trace, warn};

pub const QEMU_API_EVENT_BUFFER_SIZE: usize = 100;
const QEMU_ARCHIVE_DIR: &str = "archive";
const QEMU_GA_CHARDEV_ID: &str = "qga0";
const QEMU_GA_PORT_NAME: &str = "org.qemu.guest_agent.0";
const QEMU_USER_NETDEV_ID: &str = "net0";

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

impl PartialEq for QemuInstance {
    fn eq(&self, other: &Self) -> bool {
        self.fx_id == other.fx_id && self.qemu.to_command() == other.qemu.to_command()
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

    pub(crate) fn existing_from_metadata_record(
        system_configuration: &SystemConfiguration,
        record: QemuMetadataRecord,
        fx_id: &FxId,
    ) -> Result<Self, CreateError> {
        let qemu = convert_cmd_line_to_qemu_instance(&record.command_line).map_err(CreateError::ParseQemuCommand)?;
        let machine_configuration = machine_configuration_from_metadata_record(system_configuration, &record, &qemu);
        let (qapi_event_tx, qapi_event_rx) = tokio::sync::mpsc::channel(QEMU_API_EVENT_BUFFER_SIZE);
        let mut cmd = command_from_qemu(&qemu);

        if let Some(pid) = live_pid_from_pidfile(&record.pidfile) {
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

    pub(crate) fn metadata_record(&self, runtime_pid: Option<u32>) -> QemuMetadataRecord {
        let mut log_dir = self.system_configuration.vm_root_path.clone();
        log_dir.push(self.fx_id.to_string());
        log_dir.push("log");

        let mut data_dir = self.system_configuration.vm_data_root_path.clone();
        data_dir.push(self.fx_id.to_string());

        QemuMetadataRecord {
            name: self.machine_configuration.name.clone(),
            command_line: self.qemu.to_single_command(),
            desired: Some(QemuDesiredMetadataRecord::from(&self.machine_configuration)),
            runtime_pid,
            guest_agent_enabled: self.has_ga(),
            pidfile: QemuInstance::pid_path_s(&self.system_configuration.vm_root_path, &self.fx_id),
            qmp_socket: self.ctl_socket_path(),
            qga_socket: self.has_ga().then(|| self.ga_socket_path()),
            log_dir,
            data_dir,
        }
    }

    pub(crate) fn take_qapi_event_rx(&mut self) -> Option<Receiver<qapi::qmp::Event>> {
        self.qapi_event_rx.take()
    }

    pub(crate) fn existing_runtime_pid(&self) -> Option<u32> {
        live_pid_from_pidfile(&QemuInstance::pid_path_s(&self.system_configuration.vm_root_path, &self.fx_id))
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

    pub(crate) fn qmp_connect_timeout_secs(&self) -> u64 {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => qemu.common.qmp_connect_timeout_secs,
            QemuMachineConfigurationByArch::Aarch64(qemu) => qemu.common.qmp_connect_timeout_secs,
        }
    }

    pub(crate) fn qga_connect_timeout_secs(&self) -> u64 {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => qemu.common.qga_connect_timeout_secs,
            QemuMachineConfigurationByArch::Aarch64(qemu) => qemu.common.qga_connect_timeout_secs,
        }
    }

    pub(crate) fn uds_retry_interval_millis(&self) -> u64 {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => qemu.common.uds_retry_interval_millis,
            QemuMachineConfigurationByArch::Aarch64(qemu) => qemu.common.uds_retry_interval_millis,
        }
    }

    pub(crate) fn qga_command_timeout_secs(&self) -> u64 {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => qemu.common.qga_command_timeout_secs,
            QemuMachineConfigurationByArch::Aarch64(qemu) => qemu.common.qga_command_timeout_secs,
        }
    }

    pub(crate) fn status_poll_interval_secs(&self) -> u64 {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => qemu.common.status_poll_interval_secs,
            QemuMachineConfigurationByArch::Aarch64(qemu) => qemu.common.status_poll_interval_secs,
        }
    }

    pub(crate) fn archive_policy(&self) -> &QemuArchivePolicy {
        match &self.machine_configuration.conf {
            QemuMachineConfigurationByArch::Amd64(qemu) => &qemu.common.archive_policy,
            QemuMachineConfigurationByArch::Aarch64(qemu) => &qemu.common.archive_policy,
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
                match tokio::time::timeout(std::time::Duration::from_secs(self.qga_command_timeout_secs()), qga_handle.execute(cmd)).await {
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

    pub async fn call_supported_ga_api<T: qapi::Command + QgaCommand + Debug>(
        &mut self,
        qemu_handle: &QemuHandle,
        cmd_name: &'static str,
        cmd: T,
    ) -> Result<T::Ok, CallApiError> {
        if !qemu_handle.supported_ga_command(cmd_name) {
            return Err(CallApiError::UnsupportedGuestAgentCommand(cmd_name));
        }

        self.call_ga_api(qemu_handle, cmd).await
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

        match try_connect_ctl_socket(&self.ctl_socket_path(), self.qmp_connect_timeout_secs(), self.uds_retry_interval_millis()).await {
            Ok((api, events)) => {
                let qapi_event_tx = self.qapi_event_tx.clone();
                let mut handle = try_monitor_qemu_with_api(api, events, qapi_event_tx, pidfile).await?;
                mdt.metadata_fx_state_update(host_id, fx_id, FxExecutionState::Running(handle.process.pid))
                    .await
                    .map_err(|_| SpawnError::Db)?;
                mdt.metadata_fx_record_upsert(host_id, QEMU_METADATA_PROVIDER, fx_id, self.metadata_record(Some(handle.process.pid)))
                    .await
                    .map_err(|_| SpawnError::Db)?;

                if self.has_ga() {
                    self.attach_guest_agent(&mut handle).await?;
                }

                Ok(Some(handle))
            }
            Err(timeout_err) => {
                debug!("qemu:attach qmp connection timed out pid:{} error:{:?}", pid, timeout_err);
                Ok(None)
            }
        }
    }

    async fn attach_guest_agent(&mut self, handle: &mut QemuHandle) -> Result<(), SpawnError> {
        let (qga, qga_handle) = try_connect_ga_socket(&self.ga_socket_path(), self.qga_connect_timeout_secs(), self.uds_retry_interval_millis())
            .await
            .map_err(SpawnError::Timeout)?;

        tokio::time::timeout(
            std::time::Duration::from_secs(self.qga_command_timeout_secs()),
            qga.execute(qapi::qga::guest_ping {}),
        )
        .await
        .map_err(SpawnError::Timeout)??;
        let ga_info = tokio::time::timeout(
            std::time::Duration::from_secs(self.qga_command_timeout_secs()),
            qga.execute(qapi::qga::guest_info {}),
        )
        .await
        .map_err(SpawnError::Timeout)??;
        info!(
            "qemu:qga:ready version:{} supported_commands:{}",
            ga_info.version,
            ga_info.supported_commands.len()
        );

        handle.ga = Some(qga);
        handle.ga_info = Some(ga_info);
        handle.ga_task = Some(qga_handle);
        Ok(())
    }
}

fn command_from_qemu(qemu: &QemuSupportedArch) -> FxSystemCommand {
    let mut args = qemu.to_command();
    let cmd = if args.is_empty() { String::new() } else { args.remove(0) };
    FxSystemCommand::new(cmd, args, FxDesiredExecutionState::Running)
}

fn machine_configuration_from_metadata_record(
    system_configuration: &SystemConfiguration,
    record: &QemuMetadataRecord,
    qemu: &QemuSupportedArch,
) -> QemuMachineConfiguration {
    if let Some(desired) = &record.desired {
        return desired_machine_configuration_from_metadata(system_configuration, desired);
    }

    let mut common = default_common_options();
    common.enable_guest_agent = record.guest_agent_enabled;

    let conf = match qemu {
        QemuSupportedArch::X86_64(_) => QemuMachineConfigurationByArch::Amd64(QemuMachineConfigurationAmd64 {
            common,
            cpu: CpuTypeX86_64::Max,
            cpus: 1,
            boot_method: BootMethod::Bios,
        }),
        QemuSupportedArch::Aarch64(_) => QemuMachineConfigurationByArch::Aarch64(QemuMachineConfigurationAarch64 {
            common,
            cpu: CpuTypeAarch64::Max,
            cpus: 1,
        }),
    };

    QemuMachineConfiguration {
        name: record.name.clone(),
        system_configuration: system_configuration.clone(),
        conf,
        storage: Vec::new(),
        networking: None,
    }
}

impl From<&QemuMachineConfiguration> for QemuDesiredMetadataRecord {
    fn from(value: &QemuMachineConfiguration) -> Self {
        match &value.conf {
            QemuMachineConfigurationByArch::Amd64(machine) => Self {
                name: value.name.clone(),
                arch: QemuDesiredArchitecture::X86_64,
                common: desired_common_options(&machine.common),
                cpus: machine.cpus,
                boot_method: Some(machine.boot_method.clone()),
                storage: value.storage.clone(),
                networking: value.networking.clone(),
            },
            QemuMachineConfigurationByArch::Aarch64(machine) => Self {
                name: value.name.clone(),
                arch: QemuDesiredArchitecture::Aarch64,
                common: desired_common_options(&machine.common),
                cpus: machine.cpus,
                boot_method: None,
                storage: value.storage.clone(),
                networking: value.networking.clone(),
            },
        }
    }
}

fn desired_common_options(common: &crate::QemuCommonOptions) -> QemuDesiredCommonOptions {
    QemuDesiredCommonOptions {
        memory: common.memory.to_args().join(","),
        enable_guest_agent: common.enable_guest_agent,
        kernel: common.kernel.clone(),
        initrd: common.initrd.clone(),
        extra_options: common.extra_options.clone(),
        snapshot: common.snapshot,
        boot_kernel: common.boot_kernel,
        accel_type: common.accel_type.to_arg().to_string(),
        bootstrap_method: common.bootstrap_method.clone(),
        qmp_connect_timeout_secs: common.qmp_connect_timeout_secs,
        qga_connect_timeout_secs: common.qga_connect_timeout_secs,
        uds_retry_interval_millis: common.uds_retry_interval_millis,
        qga_command_timeout_secs: common.qga_command_timeout_secs,
        status_poll_interval_secs: common.status_poll_interval_secs,
        archive_policy: common.archive_policy.clone(),
    }
}

fn desired_machine_configuration_from_metadata(system_configuration: &SystemConfiguration, desired: &QemuDesiredMetadataRecord) -> QemuMachineConfiguration {
    let mut common = default_common_options();
    if let Ok(memory) = desired.common.memory.parse() {
        common.memory = memory;
    }
    common.enable_guest_agent = desired.common.enable_guest_agent;
    common.kernel = desired.common.kernel.clone();
    common.initrd = desired.common.initrd.clone();
    common.extra_options = desired.common.extra_options.clone();
    common.snapshot = desired.common.snapshot;
    common.boot_kernel = desired.common.boot_kernel;
    if let Ok(accel_type) = desired.common.accel_type.parse() {
        common.accel_type = accel_type;
    }
    common.bootstrap_method = desired.common.bootstrap_method.clone();
    common.qmp_connect_timeout_secs = desired.common.qmp_connect_timeout_secs;
    common.qga_connect_timeout_secs = desired.common.qga_connect_timeout_secs;
    common.uds_retry_interval_millis = desired.common.uds_retry_interval_millis;
    common.qga_command_timeout_secs = desired.common.qga_command_timeout_secs;
    common.status_poll_interval_secs = desired.common.status_poll_interval_secs;
    common.archive_policy = desired.common.archive_policy.clone();

    let conf = match desired.arch {
        QemuDesiredArchitecture::X86_64 => QemuMachineConfigurationByArch::Amd64(QemuMachineConfigurationAmd64 {
            common,
            cpu: CpuTypeX86_64::Max,
            cpus: desired.cpus,
            boot_method: desired.boot_method.clone().unwrap_or(BootMethod::Bios),
        }),
        QemuDesiredArchitecture::Aarch64 => QemuMachineConfigurationByArch::Aarch64(QemuMachineConfigurationAarch64 {
            common,
            cpu: CpuTypeAarch64::Max,
            cpus: desired.cpus,
        }),
    };

    QemuMachineConfiguration {
        name: desired.name.clone(),
        system_configuration: system_configuration.clone(),
        conf,
        storage: desired.storage.clone(),
        networking: desired.networking.clone(),
    }
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

fn validate_accelerator(accel_type: &AccelType) -> Result<(), CreateError> {
    match accel_type {
        AccelType::Tcg => Ok(()),
        AccelType::Kvm => {
            if cfg!(target_os = "linux") {
                if Path::new("/dev/kvm").exists() {
                    Ok(())
                } else {
                    Err(CreateError::Config("KVM acceleration requested but /dev/kvm is not available".to_string()))
                }
            } else {
                Err(CreateError::Config("KVM acceleration is only supported on Linux hosts".to_string()))
            }
        }
        AccelType::Hvf => {
            if cfg!(target_os = "macos") {
                Ok(())
            } else {
                Err(CreateError::Config("HVF acceleration is only supported on macOS hosts".to_string()))
            }
        }
        AccelType::Whpx => {
            if cfg!(target_os = "windows") {
                Ok(())
            } else {
                Err(CreateError::Config("WHPX acceleration is only supported on Windows hosts".to_string()))
            }
        }
        other => Err(CreateError::Config(format!("accelerator {:?} is not validated by becky-fx-qemu yet", other))),
    }
}

fn qemu_binary_path(system_configuration: &SystemConfiguration, binary_name: &str) -> Result<PathBuf, CreateError> {
    for configured in &system_configuration.emulator_paths {
        if configured.file_name().is_some_and(|name| name == binary_name) {
            return Ok(configured.clone());
        }

        let candidate = configured.join(binary_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Ok(which::which(binary_name)?)
}

fn drive_id(prefix: &str, id: &str, index: usize) -> ShellString {
    let suffix = if id.is_empty() { index.to_string() } else { id.to_string() };
    ShellString::new(format!("{prefix}-{suffix}"))
}

fn drive_for_disk(file: PathBuf, format: &str, id: ShellString) -> Drive {
    Drive {
        file: Some(ShellPath::new(file.display().to_string())),
        interface: Some(DriveInterface::Virtio),
        media: Some(DriveMedia::Disk),
        format: Some(ShellString::new(format.to_string())),
        id: Some(id),
        ..Default::default()
    }
}

fn drive_for_iso(file: PathBuf, id: ShellString) -> Drive {
    Drive {
        file: Some(ShellPath::new(file.display().to_string())),
        media: Some(DriveMedia::Cdrom),
        id: Some(id),
        ..Default::default()
    }
}

fn configured_storage_drives(system_configuration: &SystemConfiguration, machine_configuration: &QemuMachineConfiguration, fx_id: &FxId) -> Vec<Drive> {
    let mut drives = Vec::new();

    for storage in &machine_configuration.storage {
        match storage {
            QemuStorageType::Qcow2(files) => {
                for (index, (disk, format, _opts)) in files.iter().enumerate() {
                    drives.push(drive_for_disk(
                        qemu_disk_path(system_configuration, fx_id, disk, format),
                        qemu_disk_format_arg(format),
                        drive_id("disk", &disk.id, index),
                    ));
                }
            }
            QemuStorageType::HostBlock(devices) => {
                for (index, device) in devices.iter().enumerate() {
                    drives.push(drive_for_disk(device.path.clone(), "raw", drive_id("host-block", &device.id, index)));
                }
            }
            QemuStorageType::Iso(isos) => {
                for (index, iso) in isos.iter().enumerate() {
                    drives.push(drive_for_iso(iso_dest_path(system_configuration, fx_id, iso), drive_id("iso", &iso.id, index)));
                }
            }
            QemuStorageType::CloudImage(images) => {
                for (index, (image, _opts)) in images.iter().enumerate() {
                    drives.push(drive_for_disk(
                        cloud_image_overlay_path(system_configuration, fx_id, image),
                        QEMU_IMG_FORMAT_QCOW2,
                        drive_id("cloud-image", &image.id, index),
                    ));
                }
            }
        }
    }

    drives
}

fn qemu_from_machine_configuration(
    system_configuration: &SystemConfiguration,
    machine_configuration: &QemuMachineConfiguration,
    fx_id: &FxId,
) -> Result<QemuSupportedArch, CreateError> {
    match &machine_configuration.conf {
        QemuMachineConfigurationByArch::Amd64(machine) => {
            validate_common_options(&machine.common)?;
            validate_boot_method(&machine.boot_method)?;
            validate_accelerator(&machine.common.accel_type)?;
            let mut qemu = QemuInstanceForX86_64::builder()
                .qemu_binary(qemu_binary_path(system_configuration, "qemu-system-x86_64")?)
                .build();
            qemu.cpu = Some(CpuX86::new(machine.cpu.clone()));
            qemu.smp = Some(SMP::new(machine.cpus));
            qemu.m = Some(machine.common.memory.clone());
            qemu.accel = Some(Accel::new(machine.common.accel_type.clone()));
            configure_common_qemu_args(system_configuration, machine_configuration, fx_id, &machine.common, &mut qemu)?;
            Ok(QemuSupportedArch::X86_64(qemu))
        }
        QemuMachineConfigurationByArch::Aarch64(machine) => {
            validate_common_options(&machine.common)?;
            validate_accelerator(&machine.common.accel_type)?;
            let mut qemu = QemuInstanceForAarch64::builder()
                .qemu_binary(qemu_binary_path(system_configuration, "qemu-system-aarch64")?)
                .build();
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
    configure_networking(machine_configuration, qemu)?;
    if common.enable_guest_agent {
        configure_guest_agent(system_configuration, fx_id, qemu);
    }
    let drives = configured_storage_drives(system_configuration, machine_configuration, fx_id);
    if !drives.is_empty() {
        qemu.drive = Some(drives);
    }
    Ok(())
}

fn validate_common_options(common: &crate::QemuCommonOptions) -> Result<(), CreateError> {
    if !common.extra_options.is_empty() {
        return Err(CreateError::Config(
            "QEMU extra_options is not implemented; use typed configuration fields instead".to_string(),
        ));
    }
    if common.boot_kernel {
        return Err(CreateError::Config(
            "QEMU boot_kernel is not implemented; set kernel/initrd directly or leave boot_kernel=false".to_string(),
        ));
    }
    match common.bootstrap_method {
        BootStrapMethod::None => Ok(()),
        BootStrapMethod::CloudInit => Err(CreateError::Config(
            "QEMU bootstrap_method=CloudInit is not implemented as a command-line feature yet".to_string(),
        )),
    }
}

fn validate_boot_method(boot_method: &BootMethod) -> Result<(), CreateError> {
    match boot_method {
        BootMethod::Bios => Ok(()),
        BootMethod::Uefi => Err(CreateError::Config(
            "QEMU boot_method=Uefi is not implemented; configure firmware support before selecting UEFI".to_string(),
        )),
    }
}

fn configure_networking<Machine, Cpu>(
    machine_configuration: &QemuMachineConfiguration,
    qemu: &mut qemu_command_builder::QemuInstanceBase<Machine, Cpu>,
) -> Result<(), CreateError> {
    match &machine_configuration.networking {
        None => Ok(()),
        Some(NetworkingConfiguration::User) => {
            qemu.netdev
                .get_or_insert_with(Vec::new)
                .push(NetDev::User(User::builder().id(QEMU_USER_NETDEV_ID.to_string()).build()));
            let mut device = Device::new("virtio-net-pci");
            device.add_prop("netdev", QEMU_USER_NETDEV_ID);
            qemu.device.get_or_insert_with(Vec::new).push(device);
            Ok(())
        }
    }
}

fn configure_guest_agent<Machine, Cpu>(
    system_configuration: &SystemConfiguration,
    fx_id: &FxId,
    qemu: &mut qemu_command_builder::QemuInstanceBase<Machine, Cpu>,
) {
    let qga_socket = QemuInstance::ga_socket_path_s(&system_configuration.vm_root_path, fx_id);
    let chardev = CharDev::Socket(CharSocket::Uds(
        CharSocketUds::builder()
            .id(QEMU_GA_CHARDEV_ID)
            .path(qga_socket)
            .server(OnOff::On)
            .wait(OnOff::Off)
            .build(),
    ));

    qemu.chardev.get_or_insert_with(Vec::new).push(chardev);
    qemu.device.get_or_insert_with(Vec::new).push(Device::new("virtio-serial"));

    let mut qga_port = Device::new("virtserialport");
    qga_port.add_prop("chardev", QEMU_GA_CHARDEV_ID);
    qga_port.add_prop("name", QEMU_GA_PORT_NAME);
    qemu.device.get_or_insert_with(Vec::new).push(qga_port);
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

    type FxBootstrapResult = ();
    type FxBootstrapError = ();

    async fn fx_bootstrap<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError> {
        todo!()
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

        let mut log_path = PathBuf::from(&self.system_configuration.vm_root_path);
        log_path.push(self.fx_id.to_string());
        log_path.push("log");
        tokio::fs::create_dir_all(&log_path).await?;

        let qemu_stdout_path = log_path.join("qemu_stdout.log");
        let qemu_stderr_path = log_path.join("qemu_stderr.log");

        self.cmd = command_from_qemu(&self.qemu);
        self.cmd.stdout_path = Some(qemu_stdout_path);
        self.cmd.stderr_path = Some(qemu_stderr_path);

        match self.cmd.fx_start(host_id, fx_id, mdt, rc, storage).await {
            Ok(_x) => match try_connect_ctl_socket(&self.ctl_socket_path(), self.qmp_connect_timeout_secs(), self.uds_retry_interval_millis()).await {
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
                                    mdt.metadata_fx_record_upsert(host_id, QEMU_METADATA_PROVIDER, fx_id, self.metadata_record(Some(handle.process.pid)))
                                        .await
                                        .map_err(|_| SpawnError::Db)?;

                                    if self.has_ga() {
                                        self.attach_guest_agent(&mut handle).await?;
                                        Ok(handle)
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
            },
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
    type FxStopError = CallApiError;

    async fn fx_stop(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError> {
        if !handle.supported_command("system_powerdown") && !handle.supported_command("system-powerdown") {
            return Err(CallApiError::UnsupportedCommand("system_powerdown"));
        }

        let x = self.call_api(handle, qapi::qmp::system_powerdown {}).await;
        info!("qemu:operation stop {:#?}", x);
        x.map(|_| ())
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
        match self.archive_policy() {
            QemuArchivePolicy::StateOnly => {}
            QemuArchivePolicy::Disabled => return Err(ArchiveError::UnsupportedPolicy("archive disabled by QEMU archive policy")),
        }

        let archive_root = self.archive_root_path();
        tokio::fs::create_dir_all(&archive_root).await?;
        let archive_path = archive_root.join("vmstate.snap");
        let tag = format!("becky-{}", self.fx_id);
        if !fnr.supported_command("snapshot-save") && !fnr.supported_command("snapshot_save") {
            return Err(ArchiveError::CallApiError(CallApiError::UnsupportedCommand("snapshot-save")));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcg_accelerator_is_always_valid() {
        assert!(validate_accelerator(&AccelType::Tcg).is_ok());
    }

    #[test]
    fn platform_specific_accelerators_are_rejected_on_wrong_hosts() {
        if !cfg!(target_os = "linux") {
            assert!(validate_accelerator(&AccelType::Kvm).is_err());
        }
        if !cfg!(target_os = "macos") {
            assert!(validate_accelerator(&AccelType::Hvf).is_err());
        }
        if !cfg!(target_os = "windows") {
            assert!(validate_accelerator(&AccelType::Whpx).is_err());
        }
    }

    #[test]
    fn guest_agent_options_are_added_to_command_line() -> Result<(), CreateError> {
        let system_configuration = SystemConfiguration::default();
        let fx_id = FxId::String("qga-test".to_string());
        let mut common = default_common_options();
        common.enable_guest_agent = true;
        let machine_configuration = QemuMachineConfiguration {
            name: "qga-test".to_string(),
            system_configuration: system_configuration.clone(),
            conf: QemuMachineConfigurationByArch::Amd64(QemuMachineConfigurationAmd64 {
                common: common.clone(),
                cpu: CpuTypeX86_64::Max,
                cpus: 1,
                boot_method: BootMethod::Bios,
            }),
            storage: vec![],
            networking: None,
        };
        let mut qemu = QemuInstanceForX86_64::builder().qemu_binary(PathBuf::from("qemu-system-x86_64")).build();

        configure_common_qemu_args(&system_configuration, &machine_configuration, &fx_id, &common, &mut qemu)?;

        let args = qemu.to_command();
        assert!(args.iter().any(|arg| arg == "-chardev"));
        assert!(args.iter().any(|arg| {
            arg.starts_with("socket,id=qga0,path=") && arg.contains("qga-test/run/ga.sock") && arg.contains("server=on") && arg.contains("wait=off")
        }));
        assert!(args.iter().any(|arg| arg == "virtio-serial"));
        assert!(args.iter().any(|arg| arg == "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0"));
        Ok(())
    }

    #[test]
    fn user_networking_options_are_added_to_command_line() -> Result<(), CreateError> {
        let system_configuration = SystemConfiguration::default();
        let fx_id = FxId::String("net-test".to_string());
        let common = default_common_options();
        let machine_configuration = QemuMachineConfiguration {
            name: "net-test".to_string(),
            system_configuration: system_configuration.clone(),
            conf: QemuMachineConfigurationByArch::Amd64(QemuMachineConfigurationAmd64 {
                common: common.clone(),
                cpu: CpuTypeX86_64::Max,
                cpus: 1,
                boot_method: BootMethod::Bios,
            }),
            storage: vec![],
            networking: Some(NetworkingConfiguration::User),
        };
        let mut qemu = QemuInstanceForX86_64::builder().qemu_binary(PathBuf::from("qemu-system-x86_64")).build();

        configure_common_qemu_args(&system_configuration, &machine_configuration, &fx_id, &common, &mut qemu)?;

        let args = qemu.to_command();
        assert!(args.iter().any(|arg| arg == "-netdev"));
        assert!(args.iter().any(|arg| arg == "user,id=net0"));
        assert!(args.iter().any(|arg| arg == "virtio-net-pci,netdev=net0"));
        Ok(())
    }

    #[test]
    fn inert_common_options_return_config_errors() {
        let mut common = default_common_options();
        common.extra_options = vec!["-s".to_string()];
        assert!(validate_common_options(&common).is_err());

        let mut common = default_common_options();
        common.boot_kernel = true;
        assert!(validate_common_options(&common).is_err());

        let mut common = default_common_options();
        common.bootstrap_method = BootStrapMethod::CloudInit;
        assert!(validate_common_options(&common).is_err());

        assert!(validate_boot_method(&BootMethod::Uefi).is_err());
    }

    #[test]
    fn metadata_record_preserves_desired_configuration() {
        let system_configuration = SystemConfiguration::default();
        let fx_id = FxId::String("metadata-test".to_string());
        let mut common = default_common_options();
        common.qmp_connect_timeout_secs = 9;
        common.archive_policy = QemuArchivePolicy::Disabled;
        let machine_configuration = QemuMachineConfiguration {
            name: "metadata-test".to_string(),
            system_configuration: system_configuration.clone(),
            conf: QemuMachineConfigurationByArch::Amd64(QemuMachineConfigurationAmd64 {
                common,
                cpu: CpuTypeX86_64::Max,
                cpus: 4,
                boot_method: BootMethod::Bios,
            }),
            storage: vec![],
            networking: Some(NetworkingConfiguration::User),
        };
        let qemu_cmd = QemuSupportedArch::X86_64(QemuInstanceForX86_64::builder().qemu_binary(PathBuf::from("qemu-system-x86_64")).build());
        let (qapi_event_tx, qapi_event_rx) = tokio::sync::mpsc::channel(QEMU_API_EVENT_BUFFER_SIZE);
        let qemu = QemuInstance {
            cmd: command_from_qemu(&qemu_cmd),
            qemu: qemu_cmd,
            fx_id,
            machine_configuration,
            system_configuration: system_configuration.clone(),
            status: qapi::qmp::StatusInfo {
                running: false,
                status: qapi::qmp::RunState::prelaunch,
            },
            qapi_event_tx,
            qapi_event_rx: Some(qapi_event_rx),
        };

        let record = qemu.metadata_record(Some(42));
        let restored = machine_configuration_from_metadata_record(&system_configuration, &record, &qemu.qemu);

        assert_eq!(restored.name, "metadata-test");
        assert_eq!(restored.storage.len(), 0);
        assert!(matches!(restored.networking, Some(NetworkingConfiguration::User)));
        match restored.conf {
            QemuMachineConfigurationByArch::Amd64(restored) => {
                assert_eq!(restored.cpus, 4);
                assert_eq!(restored.common.qmp_connect_timeout_secs, 9);
                assert!(matches!(restored.common.archive_policy, QemuArchivePolicy::Disabled));
            }
            QemuMachineConfigurationByArch::Aarch64(_) => panic!("expected amd64 metadata"),
        }
    }
}
