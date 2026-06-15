pub mod comm;
pub mod handle;
pub mod img_json;
pub mod instance;
pub mod manager;
pub mod storage_qcow;
pub mod utils;

use crate::img_json::{FormatSpecificRoot, QemuImgInfo};
use crate::storage_qcow::{QEMU_IMG_FILE_EXIT_RAW, QEMU_IMG_FILE_EXT_QCOW2, QEMU_IMG_FORMAT_QCOW2, QEMU_IMG_FORMAT_RAW, QcowOptions, is_qcow_image_corrupt};
use async_trait::async_trait;
use becky_engine::boot_methods::BootMethod;
use becky_engine::empy_implementations::Metadataless;
use becky_engine::host_id::HostId;
use becky_engine::machine_conf::{
    BootStrapMethod, FxResourceConstraints, NetworkingConfiguration, StorageConfigurationCloudImage, StorageConfigurationDisk, StorageConfigurationIso,
};
use becky_engine::metadata::MetadataManager;
use becky_engine::os::OsImageFileType;
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
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::env::JoinPathsError;
use std::fmt::Debug;
use std::num::ParseIntError;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;
use tokio::time::error::Elapsed;
use tracing::{error, info};

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

/// QEMU binary name for image utility
const QEMU_BIN_IMG: &str = "qemu-img";
pub const QEMU_METADATA_PROVIDER: &str = "becky-fx-qemu";

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
    pub qmp_connect_timeout_secs: u64,
    pub qga_connect_timeout_secs: u64,
    pub uds_retry_interval_millis: u64,
    pub qga_command_timeout_secs: u64,
    pub status_poll_interval_secs: u64,
    pub archive_policy: QemuArchivePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QemuArchivePolicy {
    StateOnly,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QemuDesiredArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuDesiredCommonOptions {
    pub memory: String,
    pub enable_guest_agent: bool,
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub extra_options: Vec<String>,
    pub snapshot: Option<bool>,
    pub boot_kernel: bool,
    pub accel_type: String,
    pub bootstrap_method: BootStrapMethod,
    pub qmp_connect_timeout_secs: u64,
    pub qga_connect_timeout_secs: u64,
    pub uds_retry_interval_millis: u64,
    pub qga_command_timeout_secs: u64,
    pub status_poll_interval_secs: u64,
    pub archive_policy: QemuArchivePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuDesiredMetadataRecord {
    pub name: String,
    pub arch: QemuDesiredArchitecture,
    pub common: QemuDesiredCommonOptions,
    pub cpus: u64,
    pub boot_method: Option<BootMethod>,
    pub storage: Vec<QemuStorageType>,
    pub networking: Option<NetworkingConfiguration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuMetadataRecord {
    pub name: String,
    pub command_line: String,
    #[serde(default)]
    pub desired: Option<QemuDesiredMetadataRecord>,
    pub runtime_pid: Option<u32>,
    pub guest_agent_enabled: bool,
    pub pidfile: PathBuf,
    pub qmp_socket: PathBuf,
    pub qga_socket: Option<PathBuf>,
    pub log_dir: PathBuf,
    pub data_dir: PathBuf,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QemuQcowFormat {
    Raw,
    Qcow2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QemuStorageType {
    // file backed
    Qcow2(Vec<(StorageConfigurationDisk, QemuQcowFormat, QcowOptions)>),
    // host block dev
    HostBlock(Vec<StorageConfigurationDisk>),
    //Network,
    // TODO
    //CopyOnWrite,
    //InMemory,
    //DevicePassthrough,
    Iso(Vec<StorageConfigurationIso>),
    CloudImage(Vec<(StorageConfigurationCloudImage, QcowOptions)>),
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

fn qemu_disk_format_arg(format: &QemuQcowFormat) -> &'static str {
    match format {
        QemuQcowFormat::Raw => QEMU_IMG_FORMAT_RAW,
        QemuQcowFormat::Qcow2 => QEMU_IMG_FORMAT_QCOW2,
    }
}

fn os_image_format_arg(format: &OsImageFileType) -> Option<&'static str> {
    match format {
        OsImageFileType::Iso => None,
        OsImageFileType::Qcow2 => Some(QEMU_IMG_FORMAT_QCOW2),
        OsImageFileType::Raw => Some(QEMU_IMG_FORMAT_RAW),
    }
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

fn cloud_image_stem(image: &StorageConfigurationCloudImage) -> String {
    if !image.id.is_empty() {
        sanitize_storage_name(&image.id)
    } else {
        image
            .path
            .file_stem()
            .map(|name| sanitize_storage_name(&name.to_string_lossy()))
            .unwrap_or_else(|| "cloud-image".to_string())
    }
}

fn cloud_image_overlay_path(system_configuration: &SystemConfiguration, fx_id: &FxId, image: &StorageConfigurationCloudImage) -> PathBuf {
    let mut dest = vm_data_dir(system_configuration, fx_id);
    dest.push(format!("{}.{}", cloud_image_stem(image), QEMU_IMG_FILE_EXT_QCOW2));
    dest
}

async fn inspect_qemu_image(filename: &Path) -> Result<QemuImgInfo, QemuStorageCreateError> {
    let filename_arg = filename.display().to_string();
    let cmd = run_system_command(
        QEMU_BIN_IMG,
        vec!["info", "--force-share", "--output", "json", filename_arg.as_str()],
        CommandOptions::default(),
    )
    .await?;

    Ok(serde_json::from_slice::<QemuImgInfo>(cmd.output.stdout.as_slice())?)
}

async fn qemu_img_create(fmt: &str, filename: &Path, size: Option<u64>, opts: &QcowOptions) -> Result<(), QemuStorageCreateError> {
    let filename_arg = filename.display().to_string();
    let size_arg = size.map(|size| size.to_string());
    let create_options = opts.create_options();
    let create_options_arg = create_options.join(",");

    let mut args = vec!["create".to_string(), "--format".to_string(), fmt.to_string()];
    if !create_options_arg.is_empty() {
        args.push("-o".to_string());
        args.push(create_options_arg);
    }
    args.push(filename_arg);
    if let Some(size_arg) = &size_arg {
        args.push(size_arg.clone());
    }

    let borrowed_args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_system_command(QEMU_BIN_IMG, borrowed_args, CommandOptions::default()).await?;
    Ok(())
}

async fn validate_host_block_device(path: &Path) -> Result<(), QemuStorageCreateError> {
    let metadata = tokio::fs::metadata(path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            QemuStorageCreateError::FileNotFound(path.to_path_buf())
        } else {
            QemuStorageCreateError::IO(err)
        }
    })?;

    #[cfg(unix)]
    {
        if metadata.file_type().is_block_device() {
            Ok(())
        } else {
            Err(QemuStorageCreateError::UnsupportedStorage("host block storage path is not a block device"))
        }
    }

    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(QemuStorageCreateError::UnsupportedStorage(
            "host block storage validation is only implemented on Unix hosts",
        ))
    }
}

async fn qemu_img_resize(filename: &Path, size: u64, shrink: bool) -> Result<(), QemuStorageCreateError> {
    let filename_arg = filename.display().to_string();
    let size_arg = size.to_string();
    let mut args = vec!["resize"];
    if shrink {
        args.push("--shrink");
    }
    args.push(filename_arg.as_str());
    args.push(size_arg.as_str());

    run_system_command(QEMU_BIN_IMG, args, CommandOptions::default()).await?;
    Ok(())
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
    type FxConfigurationError = ();

    fn convert_from_metadata_to_fx_configuration(&self, _mdt: Self::Metadata) -> Result<Self::FxConfiguration, Self::FxConfigurationError> {
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
        qmp_connect_timeout_secs: 3,
        qga_connect_timeout_secs: 3,
        uds_retry_interval_millis: 100,
        qga_command_timeout_secs: 3,
        status_poll_interval_secs: 10,
        archive_policy: QemuArchivePolicy::StateOnly,
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

                        match inspect_qemu_image(&filename).await {
                            Ok(img) => {
                                if img.is_corrupt() {
                                    errors.push(QemuStorageCreateError::CorruptImage(filename.clone()));
                                }
                            }
                            Err(err) => errors.push(err),
                        }
                    }
                }
                QemuStorageType::HostBlock(devices) => {
                    for device in devices {
                        if let Err(err) = validate_host_block_device(&device.path).await {
                            errors.push(err);
                        }
                    }
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
                QemuStorageType::CloudImage(images) => {
                    for (image, _opts) in images {
                        let filename = cloud_image_overlay_path(&self.system_configuration, fx_id, image);
                        let inspect_path = match tokio::fs::try_exists(&filename).await {
                            Ok(true) => filename,
                            Ok(false) => image.path.clone(),
                            Err(err) => {
                                errors.push(QemuStorageCreateError::IO(err));
                                continue;
                            }
                        };

                        if os_image_format_arg(&image.os_type).is_none() {
                            errors.push(QemuStorageCreateError::UnsupportedStorage("ISO cloud images cannot be used as writable disks"));
                            continue;
                        }

                        match inspect_qemu_image(&inspect_path).await {
                            Ok(img) => {
                                if img.is_corrupt() {
                                    errors.push(QemuStorageCreateError::CorruptImage(inspect_path));
                                }
                            }
                            Err(err) => errors.push(err),
                        }
                    }
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
                    for (props, format, opts) in files {
                        let data_dir = vm_data_dir(&self.system_configuration, fx_id);

                        if let Err(err) = tokio::fs::create_dir_all(&data_dir).await {
                            errors.push(QemuStorageCreateError::IO(err));
                            continue;
                        }

                        let fmt = qemu_disk_format_arg(format);
                        let filename = qemu_disk_path(&self.system_configuration, fx_id, props, format);

                        match tokio::fs::try_exists(&filename).await {
                            Ok(true) => {
                                if let Err(corrupt_err) = is_qcow_image_corrupt(&filename).await {
                                    errors.push(corrupt_err);
                                }
                            }
                            Ok(false) => match qemu_img_create(fmt, &filename, Some(props.size.as_u64()), opts).await {
                                Ok(_created) => {}
                                Err(cmd_err) => {
                                    errors.push(cmd_err);
                                }
                            },
                            Err(err) => errors.push(QemuStorageCreateError::IO(err)),
                        }
                    }
                }
                QemuStorageType::HostBlock(devices) => {
                    for device in devices {
                        if let Err(err) = validate_host_block_device(&device.path).await {
                            errors.push(err);
                        }
                    }
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
                QemuStorageType::CloudImage(images) => {
                    for (image, opts) in images {
                        let Some(backing_format) = os_image_format_arg(&image.os_type) else {
                            errors.push(QemuStorageCreateError::UnsupportedStorage("ISO cloud images cannot be used as writable disks"));
                            continue;
                        };

                        match tokio::fs::try_exists(&image.path).await {
                            Ok(true) => {}
                            Ok(false) => {
                                errors.push(QemuStorageCreateError::FileNotFound(image.path.clone()));
                                continue;
                            }
                            Err(err) => {
                                errors.push(QemuStorageCreateError::IO(err));
                                continue;
                            }
                        }

                        let data_dir = vm_data_dir(&self.system_configuration, fx_id);
                        if let Err(err) = tokio::fs::create_dir_all(&data_dir).await {
                            errors.push(QemuStorageCreateError::IO(err));
                            continue;
                        }

                        let filename = cloud_image_overlay_path(&self.system_configuration, fx_id, image);
                        match tokio::fs::try_exists(&filename).await {
                            Ok(true) => {
                                if let Err(corrupt_err) = is_qcow_image_corrupt(&filename).await {
                                    errors.push(corrupt_err);
                                }
                            }
                            Ok(false) => {
                                let mut create_opts = opts.clone();
                                if create_opts.backing_file.is_none() {
                                    create_opts.backing_file = Some(image.path.clone());
                                }
                                if create_opts.backing_format.is_none() {
                                    create_opts.backing_format = Some(backing_format.to_string());
                                }

                                if let Err(err) = qemu_img_create(QEMU_IMG_FORMAT_QCOW2, &filename, None, &create_opts).await {
                                    errors.push(err);
                                }
                            }
                            Err(err) => errors.push(QemuStorageCreateError::IO(err)),
                        }
                    }
                }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    type SysStorageOpenResult = ();
    type SysStorageOpenError = Vec<QemuStorageCreateError>;

    async fn sys_storage_open<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageOpenResult, Self::SysStorageOpenError> {
        self.sys_storage_check(host_id, mdm, fx_id, rc).await
    }

    type SysStorageCloseResult = ();
    type SysStorageCloseError = Vec<QemuStorageCreateError>;

    async fn sys_storage_close<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCloseResult, Self::SysStorageCloseError> {
        self.sys_storage_check(host_id, mdm, fx_id, rc).await
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
                match storage {
                    QemuStorageType::Qcow2(files) => {
                        for (props, format, _opts) in files {
                            if props.id != request.filepath.id {
                                continue;
                            }

                            found = true;
                            if let Err(err) = qemu_img_resize(
                                &qemu_disk_path(&self.system_configuration, fx_id, props, format),
                                request.new_size.as_u64(),
                                matches!(request.dir, StorageResizeRequestDirection::Shrink),
                            )
                            .await
                            {
                                errors.push(err);
                            }
                        }
                    }
                    QemuStorageType::HostBlock(devices) => {
                        if devices.iter().any(|device| device.id == request.filepath.id) {
                            found = true;
                            errors.push(QemuStorageCreateError::UnsupportedStorage("host block storage cannot be resized by qemu-img"));
                        }
                    }
                    QemuStorageType::Iso(isos) => {
                        if isos.iter().any(|iso| iso.id == request.filepath.id) {
                            found = true;
                            errors.push(QemuStorageCreateError::UnsupportedStorage("ISO storage cannot be resized"));
                        }
                    }
                    QemuStorageType::CloudImage(images) => {
                        for (image, _opts) in images {
                            if image.id != request.filepath.id {
                                continue;
                            }

                            found = true;
                            if let Err(err) = qemu_img_resize(
                                &cloud_image_overlay_path(&self.system_configuration, fx_id, image),
                                request.new_size.as_u64(),
                                matches!(request.dir, StorageResizeRequestDirection::Shrink),
                            )
                            .await
                            {
                                errors.push(err);
                            }
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
    Watchdog,
    BlockIoError,
    BlockImageCorrupted,
    DeviceDeleted,
    DeviceUnplugGuestError,
    Migration,
    MemoryFailure,
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
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("metadata: {0}")]
    Metadata(String),

    #[error("qemu command parse: {0}")]
    ParseQemuCommand(String),

    #[error("process lookup failed: {0}")]
    GetProc(String),

    #[error("pid parse failed: {0}")]
    ParsePid(#[from] ParseIntError),

    #[error("path list join failed: {0}")]
    JoinPaths(#[from] JoinPathsError),

    #[error("qemu binary lookup failed: {0}")]
    Which(#[from] which::Error),

    #[error("verify")]
    Verify(#[from] VerifyError),

    #[error("spawn")]
    Spawn(#[from] SpawnError),

    #[error("alloc")]
    Allocate(#[from] AllocateError),
}

#[derive(Error, Debug)]
pub enum QemuManagerStopError {
    #[error("qemu vm is not managed: {0}")]
    NotFound(FxId),

    #[error("worker command send failed: {0}")]
    Send(String),

    #[error("qemu vm {fx_id} pid {pid} did not exit within {timeout_secs} seconds")]
    Timeout { fx_id: FxId, pid: u32, timeout_secs: u64 },

    #[error("worker join failed: {0}")]
    Join(String),

    #[error("metadata update failed: {0}")]
    Metadata(String),
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

    #[error("unsupported qmp command: {0}")]
    UnsupportedCommand(&'static str),

    #[error("unsupported qga command: {0}")]
    UnsupportedGuestAgentCommand(&'static str),

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

    #[error("unsupported archive policy: {0}")]
    UnsupportedPolicy(&'static str),
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

    #[error("qmp call failed with error code {0}")]
    Qmp(#[from] qapi::ExecuteError),

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

#[cfg(test)]
mod tests {
    use super::*;
    use becky_engine::empy_implementations::Metadataless;
    use becky_engine::host_id::HostId;
    use becky_engine::machine_conf::StorageConfigurationDisk;
    use becky_engine::storage::{StorageResizeRequest, StorageResizeRequestDirection, SysStorage};
    use bytesize::ByteSize;

    #[test]
    fn default_common_options_include_runtime_timeouts() {
        let opts = default_common_options();

        assert_eq!(opts.qmp_connect_timeout_secs, 3);
        assert_eq!(opts.qga_connect_timeout_secs, 3);
        assert_eq!(opts.uds_retry_interval_millis, 100);
        assert_eq!(opts.qga_command_timeout_secs, 3);
        assert_eq!(opts.status_poll_interval_secs, 10);
    }

    #[tokio::test]
    async fn host_block_resize_reports_unsupported_storage() {
        let disk = StorageConfigurationDisk {
            id: "host-disk".to_string(),
            path: PathBuf::from("/dev/not-real"),
            size: ByteSize::b(0),
            bootable: false,
        };
        let mut storage = QemuMachineConfiguration {
            name: "test".to_string(),
            system_configuration: SystemConfiguration::default(),
            conf: default_machine_configuration_by_host(default_common_options()),
            storage: vec![QemuStorageType::HostBlock(vec![disk.clone()])],
            networking: None,
        };
        let mut metadata = Metadataless {};

        let result = storage
            .sys_storage_resize(
                &HostId::String("host".to_string()),
                &mut metadata,
                &FxId::String("fx".to_string()),
                QemuMachineRequest::default_for_host(),
                vec![StorageResizeRequest {
                    filepath: disk,
                    new_size: ByteSize::b(1024),
                    dir: StorageResizeRequestDirection::Grow,
                }],
            )
            .await;

        let errors = match result {
            Ok(()) => panic!("host block resize should be unsupported"),
            Err(errors) => errors,
        };
        assert!(matches!(errors.first(), Some(QemuStorageCreateError::UnsupportedStorage(message)) if message.contains("host block")));
    }
}
