pub mod comm;
pub mod handle;
pub mod img_json;
pub mod instance;
pub mod manager;
pub mod storage_qcow;
pub mod utils;

use crate::img_json::QemuImgInfo;
use crate::storage_qcow::{QEMU_IMG_FILE_EXIT_RAW, QEMU_IMG_FILE_EXT_QCOW2, QEMU_IMG_FORMAT_QCOW2, QEMU_IMG_FORMAT_RAW, QcowOptions, is_qcow_image_corrupt};
use async_trait::async_trait;
use becky_engine::boot_methods::BootMethod;
use becky_engine::empy_implementations::Metadataless;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::{BootStrapMethod, FxResourceConstraints, NetworkingConfiguration, StorageConfigurationDisk, StorageConfigurationIso};
use becky_engine::metadata::MetadataManager;
use becky_engine::storage::{StorageResizeRequest, StorageResizeRequestDirection, SysStorage};
use becky_engine::sys_conf::SystemConfiguration;
use becky_fx_id::FxId;
use becky_fx_system_command::FxSysCommandError;
use becky_utils::{CommandOptions, CommandRanError, run_system_command};
use bon::Builder;
use qemu_command_builder::args::cpu_type::{CpuNotFound, CpuTypeAarch64, CpuTypeX86_64};
use qemu_command_builder::args::memory::{Memory, MemoryUnit};
use qemu_command_builder::common::AccelType;
use qemu_command_builder::to_command::ToCommand;
use qemu_command_builder::{QemuCommand, QemuInstanceForAarch64, QemuInstanceForX86_64};
use std::env::JoinPathsError;
use std::fmt::Debug;
use std::num::ParseIntError;
use std::path::PathBuf;
use std::str::FromStr;
use thiserror::Error;
use tokio::time::error::Elapsed;
use tracing::{error, info};

/// QEMU binary name for image utility
const QEMU_BIN_IMG: &str = "qemu-img";

/// filename where to put the process id for the `-pidfile` QEMU command line argument, lives under `<vm-root>/<fx-id>/run/qemu.pid`
const QEMU_PID_FILENAME: &str = "qemu.pid";

#[derive(Builder, Debug, Clone)]
pub struct QemuCommonOptions {
    pub memory: Memory,
    pub enable_guest_agent: bool,
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub extra_options: Vec<String>,
    pub snapshot: Option<bool>,
    pub boot_kernel: bool, // ???
    pub accel_type: AccelType,
    pub bootstrap_method: BootStrapMethod,
}

/// QEMU-specific Configuration for the desired VM.
#[derive(Builder, Debug, Clone)]
pub struct QemuMachineConfigurationAmd64 {
    pub common: QemuCommonOptions,
    pub cpu: CpuTypeX86_64, // until it needs changing to support cloud-hypervisor, firecracker, etc.
    pub cpus: u64,
    pub boot_method: BootMethod,
}

/// QEMU-specific Configuration for the desired VM.
#[derive(Builder, Debug, Clone)]
pub struct QemuMachineConfigurationAarch64 {
    pub common: QemuCommonOptions,
    pub cpu: CpuTypeAarch64, // until it needs changing to support cloud-hypervisor, firecracker, etc.
    pub cpus: u64,
}

#[derive(Debug, Clone)]
pub enum QemuMachineConfigurationByArch {
    Amd64(QemuMachineConfigurationAmd64),
    Aarch64(QemuMachineConfigurationAarch64),
}

#[derive(Debug, Clone)]
pub enum QemuQcowFormat {
    Raw,
    Qcow2,
}

#[derive(Debug, Clone)]
pub enum QemuStorageType {
    // file backed
    Qcow2(Vec<(StorageConfigurationDisk, QemuQcowFormat, QcowOptions)>),
    // host block dev
    HostBlock,
    //Network,
    // TODO
    //CopyOnWrite,
    //InMemory,
    //DevicePassthrough,
    Iso(Vec<StorageConfigurationIso>),
    CloudImage,
}

#[derive(Debug, Clone)]
pub struct QemuMachineConfiguration {
    pub name: String,
    pub system_configuration: SystemConfiguration,
    pub conf: QemuMachineConfigurationByArch,
    pub storage: Vec<QemuStorageType>,
    pub networking: Option<NetworkingConfiguration>,
}

fn vm_data_dir(system_configuration: &SystemConfiguration, fx_id: &FxId) -> PathBuf {
    let mut path = system_configuration.vm_data_root_path.clone();
    path.push(fx_id.to_string());
    path
}

fn sanitize_storage_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

fn disk_stem(props: &StorageConfigurationDisk) -> String {
    if !props.id.is_empty() {
        sanitize_storage_name(&props.id)
    } else {
        props
            .path
            .file_name()
            .map(|name| sanitize_storage_name(&name.to_string_lossy()))
            .unwrap_or_else(|| "disk".to_string())
    }
}

fn qemu_disk_path(system_configuration: &SystemConfiguration, fx_id: &FxId, props: &StorageConfigurationDisk, format: &QemuQcowFormat) -> PathBuf {
    let ext = match format {
        QemuQcowFormat::Raw => QEMU_IMG_FILE_EXIT_RAW,
        QemuQcowFormat::Qcow2 => QEMU_IMG_FILE_EXT_QCOW2,
    };

    let mut filename = vm_data_dir(system_configuration, fx_id);
    filename.push(format!("{}.{}", disk_stem(props), ext));
    filename
}

fn iso_dest_path(system_configuration: &SystemConfiguration, fx_id: &FxId, iso: &StorageConfigurationIso) -> PathBuf {
    let name = if !iso.id.is_empty() {
        sanitize_storage_name(&iso.id)
    } else {
        iso.path
            .file_name()
            .map(|name| sanitize_storage_name(&name.to_string_lossy()))
            .unwrap_or_else(|| "media.iso".to_string())
    };

    let mut dest = vm_data_dir(system_configuration, fx_id);
    dest.push(name);
    dest
}

/// QEMU-specific resource request accepted by [`manager::QemuManager`].
///
/// This type carries the full VM shape that the generic Becky
/// [`FxResourceConstraints`] trait cannot currently express directly.
#[derive(Builder, Debug, Clone)]
pub struct QemuMachineRequest {
    pub name: String,
    pub conf: QemuMachineConfigurationByArch,
    pub storage: Vec<QemuStorageType>,
    pub networking: Option<NetworkingConfiguration>,
}

impl QemuMachineRequest {
    pub fn default_for_host() -> Self {
        Self {
            name: "becky-qemu".to_string(),
            conf: default_machine_configuration_by_host(default_common_options()),
            storage: vec![],
            networking: Some(NetworkingConfiguration::User),
        }
    }

    pub fn to_machine_configuration(&self, system_configuration: &SystemConfiguration) -> QemuMachineConfiguration {
        QemuMachineConfiguration {
            name: self.name.clone(),
            system_configuration: system_configuration.clone(),
            conf: self.conf.clone(),
            storage: self.storage.clone(),
            networking: self.networking.clone(),
        }
    }
}

impl Default for QemuMachineRequest {
    fn default() -> Self {
        Self::default_for_host()
    }
}

impl FxResourceConstraints for QemuMachineRequest {
    type Metadata = Metadataless;
    type FxStorageConfiguration = Vec<QemuStorageType>;
    type FxConfiguration = QemuMachineRequest;

    fn convert_from_metadata_to_fx_configuration(&self, _mdt: Self::Metadata) -> Result<Self::FxConfiguration, ()> {
        Ok(self.clone())
    }

    fn storage_configurations(&self) -> Self::FxStorageConfiguration {
        self.storage.clone()
    }
}

/// Resource constraints that can be converted into concrete QEMU machine
/// configuration by a QEMU manager.
pub trait QemuFxResourceConstraints: FxResourceConstraints {
    fn qemu_machine_configuration(&self, system_configuration: &SystemConfiguration) -> QemuMachineConfiguration;
}

impl QemuFxResourceConstraints for QemuMachineRequest {
    fn qemu_machine_configuration(&self, system_configuration: &SystemConfiguration) -> QemuMachineConfiguration {
        self.to_machine_configuration(system_configuration)
    }
}

pub fn default_common_options() -> QemuCommonOptions {
    QemuCommonOptions {
        memory: Memory::builder().mem(MemoryUnit::MegaBytes(512)).build(),
        enable_guest_agent: false,
        kernel: None,
        initrd: None,
        extra_options: vec![],
        snapshot: None,
        boot_kernel: false,
        accel_type: AccelType::Tcg,
        bootstrap_method: BootStrapMethod::None,
    }
}

#[cfg(target_arch = "aarch64")]
pub fn default_machine_configuration_by_host(common: QemuCommonOptions) -> QemuMachineConfigurationByArch {
    QemuMachineConfigurationByArch::Aarch64(QemuMachineConfigurationAarch64 {
        common,
        cpu: CpuTypeAarch64::Max,
        cpus: 1,
    })
}

#[cfg(not(target_arch = "aarch64"))]
pub fn default_machine_configuration_by_host(common: QemuCommonOptions) -> QemuMachineConfigurationByArch {
    QemuMachineConfigurationByArch::Amd64(QemuMachineConfigurationAmd64 {
        common,
        cpu: CpuTypeX86_64::Max,
        cpus: 1,
        boot_method: BootMethod::Bios,
    })
}

#[async_trait]
impl SysStorage for QemuMachineConfiguration {
    type SysStorageInfoResult = ();
    type SysStorageInfoError = Vec<QemuStorageCreateError>;

    async fn sys_storage_info<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageInfoResult, Self::SysStorageInfoError> {
        let mut errors = vec![];
        for storage in &self.storage {
            match storage {
                QemuStorageType::Qcow2(files) => {
                    for (props, format, _opts) in files {
                        let filename = qemu_disk_path(&self.system_configuration, fx_id, props, format);

                        match run_system_command(
                            QEMU_BIN_IMG,
                            vec!["info", "--force-share", "--output", "json", filename.display().to_string().as_str()],
                            CommandOptions::default(),
                        )
                        .await
                        {
                            Ok(cmd) => match serde_json::from_slice::<QemuImgInfo>(cmd.output.stdout.as_slice()) {
                                Ok(img) => {
                                    if img.format_specific.data.corrupt {
                                        errors.push(QemuStorageCreateError::CorruptImage(filename.clone()));
                                    }
                                }
                                Err(json_parse_err) => {
                                    errors.push(QemuStorageCreateError::JsonParse(json_parse_err));
                                }
                            },
                            Err(cmd_err) => errors.push(QemuStorageCreateError::CommandRanError(cmd_err)),
                        }
                    }
                }
                QemuStorageType::HostBlock => {
                    errors.push(QemuStorageCreateError::UnsupportedStorage("host block storage is not implemented"));
                }
                QemuStorageType::Iso(isos) => {
                    for iso in isos {
                        match tokio::fs::try_exists(&iso.path).await {
                            Ok(found) => {
                                if found {
                                } else {
                                    errors.push(QemuStorageCreateError::FileNotFound(iso.path.clone()));
                                }
                            }
                            Err(_exists_err) => {
                                errors.push(QemuStorageCreateError::FileNotFound(iso.path.clone()));
                            }
                        }
                    }
                }
                QemuStorageType::CloudImage => {
                    errors.push(QemuStorageCreateError::UnsupportedStorage("cloud image storage is not implemented"));
                }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    type SysStorageCheckResult = ();
    type SysStorageCheckError = Vec<QemuStorageCreateError>;

    async fn sys_storage_check<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCheckResult, Self::SysStorageCheckError> {
        self.sys_storage_info(host_id, mdm, fx_id, rc).await
    }

    type SysStorageCreateResult = ();
    type SysStorageCreateError = Vec<QemuStorageCreateError>;

    async fn sys_storage_create<T: MetadataManager + Send + Sync>(
        &mut self,
        _sys_conf: &SystemConfiguration,
        _host_id: &HostId,
        _mdm: &mut T,
        fx_id: &FxId,
        _rc: &impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCreateResult, Self::SysStorageCreateError> {
        let mut errors = vec![];
        for storage in &self.storage {
            match storage {
                QemuStorageType::Qcow2(files) => {
                    for (props, format, _opts) in files {
                        let data_dir = vm_data_dir(&self.system_configuration, fx_id);

                        if let Err(err) = tokio::fs::create_dir_all(&data_dir).await {
                            errors.push(QemuStorageCreateError::IO(err));
                            continue;
                        }

                        let fmt = match format {
                            QemuQcowFormat::Raw => QEMU_IMG_FORMAT_RAW,
                            QemuQcowFormat::Qcow2 => QEMU_IMG_FORMAT_QCOW2,
                        };

                        let filename = qemu_disk_path(&self.system_configuration, fx_id, props, format);

                        match tokio::fs::try_exists(&filename).await {
                            Ok(true) => {
                                if let Err(corrupt_err) = is_qcow_image_corrupt(&filename).await {
                                    errors.push(corrupt_err);
                                }
                            }
                            Ok(false) => {
                                let filename_arg = filename.display().to_string();
                                let size_arg = props.size.as_u64().to_string();
                                match run_system_command(
                                    QEMU_BIN_IMG,
                                    vec!["create", "--format", fmt, filename_arg.as_str(), size_arg.as_str()],
                                    CommandOptions::default(),
                                )
                                .await
                                {
                                    Ok(_created) => {}
                                    Err(cmd_err) => {
                                        errors.push(QemuStorageCreateError::CommandRanError(cmd_err));
                                    }
                                }
                            }
                            Err(err) => errors.push(QemuStorageCreateError::IO(err)),
                        }
                    }
                }
                QemuStorageType::HostBlock => {
                    errors.push(QemuStorageCreateError::UnsupportedStorage("host block storage is not implemented"));
                }
                QemuStorageType::Iso(isos) => {
                    for iso in isos {
                        let data_dir = vm_data_dir(&self.system_configuration, fx_id);
                        if let Err(err) = tokio::fs::create_dir_all(&data_dir).await {
                            errors.push(QemuStorageCreateError::IO(err));
                            continue;
                        }

                        let src_filename = iso.path.clone();
                        let dest_filename = iso_dest_path(&self.system_configuration, fx_id, iso);

                        info!("cp {} -> {}", &src_filename.display(), dest_filename.display());

                        match tokio::fs::copy(&src_filename, &dest_filename).await {
                            Ok(f) => {
                                info!("copied {} bytes", f);
                            }
                            Err(copy_err) => {
                                error!("copy failed: {}", copy_err);
                                errors.push(QemuStorageCreateError::IO(copy_err));
                            }
                        }
                    }
                }
                QemuStorageType::CloudImage => {
                    errors.push(QemuStorageCreateError::UnsupportedStorage("cloud image storage is not implemented"));
                }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    type SysStorageOpenResult = ();
    type SysStorageOpenError = ();

    async fn sys_storage_open<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageOpenResult, Self::SysStorageOpenError> {
        Ok(())
    }

    type SysStorageCloseResult = ();
    type SysStorageCloseError = ();

    async fn sys_storage_close<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCloseResult, Self::SysStorageCloseError> {
        Ok(())
    }

    type SysStorageResizeResult = ();
    type SysStorageResizeError = Vec<QemuStorageCreateError>;

    async fn sys_storage_resize<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        fx_id: &FxId,
        _rc: impl FxResourceConstraints,
        resize_requests: Vec<StorageResizeRequest>,
    ) -> Result<Self::SysStorageResizeResult, Self::SysStorageResizeError> {
        let mut errors = vec![];

        for request in resize_requests {
            let mut found = false;
            for storage in &self.storage {
                if let QemuStorageType::Qcow2(files) = storage {
                    for (props, format, _opts) in files {
                        if props.id != request.filepath.id {
                            continue;
                        }

                        found = true;
                        let filename = qemu_disk_path(&self.system_configuration, fx_id, props, format);
                        let filename_arg = filename.display().to_string();
                        let size_arg = request.new_size.as_u64().to_string();
                        let mut args = vec!["resize"];
                        if matches!(request.dir, StorageResizeRequestDirection::Shrink) {
                            args.push("--shrink");
                        }
                        args.push(filename_arg.as_str());
                        args.push(size_arg.as_str());

                        if let Err(err) = run_system_command(QEMU_BIN_IMG, args, CommandOptions::default()).await {
                            errors.push(QemuStorageCreateError::CommandRanError(err));
                        }
                    }
                }
            }

            if !found {
                errors.push(QemuStorageCreateError::FileNotFound(request.filepath.path.clone()));
            }
        }

        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}

/// Supported hardware architectures
#[derive(Clone, Debug, Hash, Ord, PartialOrd, PartialEq, Eq)]
pub enum QemuSupportedArch {
    X86_64(QemuInstanceForX86_64),
    Aarch64(QemuInstanceForAarch64),
}

impl ToCommand for QemuSupportedArch {
    fn command(&self) -> String {
        match self {
            QemuSupportedArch::X86_64(qemu_instance) => qemu_instance.command(),
            QemuSupportedArch::Aarch64(qemu_instance) => qemu_instance.command(),
        }
    }

    fn to_args(&self) -> Vec<String> {
        match self {
            QemuSupportedArch::X86_64(qemu_instance) => qemu_instance.to_args(),
            QemuSupportedArch::Aarch64(qemu_instance) => qemu_instance.to_args(),
        }
    }
}

impl FromStr for QemuSupportedArch {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse::<QemuCommand>()? {
            QemuCommand::X86_64(qemu) => Ok(QemuSupportedArch::X86_64(qemu)),
            QemuCommand::Aarch64(qemu) => Ok(QemuSupportedArch::Aarch64(qemu)),
        }
    }
}
#[derive(Clone, Debug)]
pub enum WorkerEvent {
    Panic,
    Shutdown,
    Suspend,
    Resume,
    Powerdown,
    Reset,
    Stop,
}

/// Commands to send to the QEMU process watcher tasks
#[derive(Clone, Debug)]
pub enum QmpCmd {
    Status,
    SystemPowerdown,
    QueryBlock,
}

#[derive(Clone, Debug)]
pub enum GuestAgentCmd {
    Ping,
    Info,
}

/// Commands to send to the QEMU process watcher tasks
#[derive(Clone, Debug)]
pub enum WorkerCommand {
    // worker commands
    Shutdown,

    // commands to run over QMP
    Qmp(QmpCmd),

    // guest agent commands
    GuestAgent(GuestAgentCmd),
}

#[derive(Error, Debug)]
pub enum CreateError {
    #[error("config: {0}")]
    Config(String),

    #[error("todo {0}")]
    GetProc(String),

    #[error("todo {0}")]
    ParsePid(#[from] ParseIntError),

    #[error("todo {0}")]
    JoinPaths(#[from] JoinPathsError),

    #[error("todo {0}")]
    Which(#[from] which::Error),

    #[error("verify")]
    Verify(#[from] VerifyError),

    #[error("spawn")]
    Spawn(#[from] SpawnError),

    #[error("alloc")]
    Allocate(#[from] AllocateError),
}

#[derive(Error, Debug)]
pub enum CollectError {
    #[error("api")]
    CallApiError(#[from] CallApiError),

    #[error("cpu not found")]
    CpuNotFound(CpuNotFound),

    #[error("regex")]
    Regex(#[from] regex::Error),

    #[error("cpu regex failed")]
    CpuRegexFailed,
}

#[derive(Error, Debug)]
pub enum CallApiError {
    #[error("qemu call failed with error code {0}")]
    Executor(#[from] qapi::ExecuteError),

    #[error("qemu call failed with error code {0}")]
    Timeout(#[from] Elapsed),

    #[error("qemu call failed with error code")]
    NoGaAvailable,
}

#[derive(Error, Debug)]
pub enum SysScanError {
    #[error("io")]
    IO(#[from] std::io::Error),

    #[error("parse {0}")]
    ParsePid(#[from] ParseIntError),
}

#[derive(Error, Debug)]
pub enum VerifyError {
    #[error("io")]
    IO(#[from] std::io::Error),

    #[error("api")]
    CallApiError(#[from] CallApiError),

    #[error("collect")]
    Collect(#[from] CollectError),
}

#[derive(Error, Debug)]
pub enum ArchiveError {
    #[error("io")]
    IO(#[from] std::io::Error),

    #[error("api")]
    CallApiError(#[from] CallApiError),
}

#[derive(Error, Debug)]
pub enum QemuStorageCreateError {
    #[error("io")]
    IO(#[from] std::io::Error),

    #[error("json")]
    JsonParse(#[from] serde_json::error::Error),

    #[error("corrupt image")]
    CorruptImage(PathBuf),

    #[error("cmd")]
    CommandRanError(#[from] CommandRanError),

    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("unsupported storage: {0}")]
    UnsupportedStorage(&'static str),
}

#[derive(Error, Debug)]
pub enum SpawnError {
    #[error("io")]
    Io(#[from] std::io::Error),

    #[error("system command")]
    SystemCommand(#[from] FxSysCommandError),

    #[error("pid not found")]
    PidNotFound,

    #[error("parsing string into a valid pid failed with {0}")]
    ParsePid(ParseIntError),

    #[error("uds timeout")]
    Timeout(Elapsed),

    #[error("db")]
    Db,
}

#[derive(Error, Debug)]
pub enum AllocateError {
    #[error("io")]
    Io(#[from] std::io::Error),

    #[error("corrupt image")]
    StorageNotOk,

    #[error("db")]
    Db,
}
