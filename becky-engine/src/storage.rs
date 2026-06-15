//! Storage management traits and simple storage backends.

use async_trait::async_trait;
use becky_fx_id::FxId;
use bytesize::ByteSize;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::host_id::HostId;
use crate::machine_conf::{FxResourceConstraints, StorageConfigurationDisk};
use crate::metadata::MetadataManager;
use crate::sys_conf::SystemConfiguration;

/// Direction of a storage resize operation.
pub enum StorageResizeRequestDirection {
    /// Reduce storage size.
    Shrink,
    /// Increase storage size.
    Grow,
}

/// Request to resize a configured disk.
pub struct StorageResizeRequest {
    /// Disk to resize.
    pub filepath: StorageConfigurationDisk,
    /// Target size.
    pub new_size: ByteSize,
    /// Resize direction.
    pub dir: StorageResizeRequestDirection,
}

/// Errors returned by the basic filesystem storage backend.
#[derive(Debug, Error)]
pub enum StorageFilesystemError {
    /// Filesystem operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A required directory was not present.
    #[error("directory not found: {0}")]
    DirectoryNotFound(PathBuf),

    /// The operation is not supported by the filesystem backend.
    #[error("unsupported storage operation: {0}")]
    Unsupported(&'static str),
}

async fn ensure_dir(path: &Path) -> Result<(), StorageFilesystemError> {
    let metadata = tokio::fs::metadata(path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            StorageFilesystemError::DirectoryNotFound(path.to_path_buf())
        } else {
            StorageFilesystemError::Io(err)
        }
    })?;

    if metadata.is_dir() {
        Ok(())
    } else {
        Err(StorageFilesystemError::Unsupported("configured storage path exists but is not a directory"))
    }
}

async fn ensure_system_dirs(sys_conf: &SystemConfiguration) -> Result<(), StorageFilesystemError> {
    ensure_dir(&sys_conf.os_cache_root_path).await?;
    ensure_dir(&sys_conf.vm_root_path).await?;
    ensure_dir(&sys_conf.vm_data_root_path).await?;
    ensure_dir(&sys_conf.run_path).await
}

#[async_trait]
/// Storage lifecycle API used by effect providers.
pub trait SysStorage: Send + Sync {
    /// Storage-info result type.
    type SysStorageInfoResult: Debug;
    /// Storage-info error type.
    type SysStorageInfoError: Debug;
    /// Returns storage information for an effect.
    async fn sys_storage_info<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageInfoResult, Self::SysStorageInfoError>;

    /// Storage-check result type.
    type SysStorageCheckResult: Debug;
    /// Storage-check error type.
    type SysStorageCheckError: Debug;
    /// Validates storage for an effect.
    async fn sys_storage_check<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCheckResult, Self::SysStorageCheckError>;

    /// Storage-create result type.
    type SysStorageCreateResult: Debug;
    /// Storage-create error type.
    type SysStorageCreateError: Debug;
    /// Creates storage needed by an effect.
    async fn sys_storage_create<T: MetadataManager + Send + Sync>(
        &mut self,
        sys_conf: &SystemConfiguration,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: &impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCreateResult, Self::SysStorageCreateError>;

    /// Storage-open result type.
    type SysStorageOpenResult;
    /// Storage-open error type.
    type SysStorageOpenError;
    /// Opens or attaches storage for an effect.
    async fn sys_storage_open<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageOpenResult, Self::SysStorageOpenError>;

    /// Storage-close result type.
    type SysStorageCloseResult;
    /// Storage-close error type.
    type SysStorageCloseError;
    /// Closes or detaches storage for an effect.
    async fn sys_storage_close<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCloseResult, Self::SysStorageCloseError>;

    /// Storage-resize result type.
    type SysStorageResizeResult;
    /// Storage-resize error type.
    type SysStorageResizeError;
    /// Resizes storage for an effect.
    async fn sys_storage_resize<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
        fx_id: &FxId,
        rc: impl FxResourceConstraints,
        resize_requests: Vec<StorageResizeRequest>,
    ) -> Result<Self::SysStorageResizeResult, Self::SysStorageResizeError>;
}

// TODO
// provide traits to convert between different storage types
// https://manurevah.com/blah/en/p/Convert-qcow2-to-LVM

/// No-op storage backend for effects that do not need storage.
pub struct Storageless {}

#[async_trait]
impl SysStorage for Storageless {
    type SysStorageInfoResult = ();
    type SysStorageInfoError = ();

    async fn sys_storage_info<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageInfoResult, Self::SysStorageInfoError> {
        Ok(())
    }

    type SysStorageCheckResult = ();
    type SysStorageCheckError = ();

    async fn sys_storage_check<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCheckResult, Self::SysStorageCheckError> {
        Ok(())
    }

    type SysStorageCreateResult = ();
    type SysStorageCreateError = ();

    async fn sys_storage_create<T: MetadataManager + Send + Sync>(
        &mut self,
        _sys_conf: &SystemConfiguration,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: &impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCreateResult, Self::SysStorageCreateError> {
        Ok(())
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
    type SysStorageResizeError = ();

    async fn sys_storage_resize<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
        _resize_requests: Vec<StorageResizeRequest>,
    ) -> Result<Self::SysStorageResizeResult, Self::SysStorageResizeError> {
        Ok(())
    }
}

/// Filesystem-backed storage backend that creates and validates Becky working directories.
#[derive(Clone, Debug, Default)]
pub struct StorageFilesystem {
    /// Optional configuration used by info/check/open/close calls that do not
    /// receive a `SystemConfiguration` argument directly.
    pub system_configuration: Option<SystemConfiguration>,
}

impl StorageFilesystem {
    /// Creates a filesystem storage backend bound to a system configuration.
    pub fn new(system_configuration: SystemConfiguration) -> Self {
        Self {
            system_configuration: Some(system_configuration),
        }
    }

    async fn validate_bound_configuration(&self) -> Result<(), StorageFilesystemError> {
        match &self.system_configuration {
            Some(sys_conf) => ensure_system_dirs(sys_conf).await,
            None => Err(StorageFilesystemError::Unsupported(
                "StorageFilesystem has no bound SystemConfiguration for this lifecycle method",
            )),
        }
    }
}

#[async_trait]
impl SysStorage for StorageFilesystem {
    type SysStorageInfoResult = ();
    type SysStorageInfoError = StorageFilesystemError;

    async fn sys_storage_info<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageInfoResult, Self::SysStorageInfoError> {
        self.validate_bound_configuration().await
    }

    type SysStorageCheckResult = ();
    type SysStorageCheckError = StorageFilesystemError;

    async fn sys_storage_check<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCheckResult, Self::SysStorageCheckError> {
        self.validate_bound_configuration().await
    }

    type SysStorageCreateResult = ();
    type SysStorageCreateError = StorageFilesystemError;

    async fn sys_storage_create<T: MetadataManager + Send + Sync>(
        &mut self,
        sys_conf: &SystemConfiguration,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: &impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCreateResult, Self::SysStorageCreateError> {
        tokio::fs::create_dir_all(&sys_conf.os_cache_root_path).await?;
        tokio::fs::create_dir_all(&sys_conf.vm_root_path).await?;
        tokio::fs::create_dir_all(&sys_conf.vm_data_root_path).await?;
        tokio::fs::create_dir_all(&sys_conf.run_path).await?;
        ensure_system_dirs(sys_conf).await?;
        self.system_configuration = Some(sys_conf.clone());
        Ok(())
    }

    type SysStorageOpenResult = ();
    type SysStorageOpenError = StorageFilesystemError;

    async fn sys_storage_open<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageOpenResult, Self::SysStorageOpenError> {
        self.validate_bound_configuration().await
    }

    type SysStorageCloseResult = ();
    type SysStorageCloseError = StorageFilesystemError;

    async fn sys_storage_close<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
    ) -> Result<Self::SysStorageCloseResult, Self::SysStorageCloseError> {
        self.validate_bound_configuration().await
    }

    type SysStorageResizeResult = ();
    type SysStorageResizeError = StorageFilesystemError;

    async fn sys_storage_resize<T: MetadataManager + Send + Sync>(
        &mut self,
        _host_id: &HostId,
        _mdm: &mut T,
        _fx_id: &FxId,
        _rc: impl FxResourceConstraints,
        resize_requests: Vec<StorageResizeRequest>,
    ) -> Result<Self::SysStorageResizeResult, Self::SysStorageResizeError> {
        if !resize_requests.is_empty() {
            return Err(StorageFilesystemError::Unsupported("StorageFilesystem does not resize provider disks"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::empy_implementations::Metadataless;
    use crate::host_id::HostId;
    use crate::machine_conf::ResourceConstraintless;

    fn unique_storage_root() -> PathBuf {
        std::env::temp_dir().join(format!("becky-storage-test-{}", std::process::id()))
    }

    fn test_system_configuration(root: &Path) -> SystemConfiguration {
        SystemConfiguration {
            emulator_paths: Vec::new(),
            binary_paths: Vec::new(),
            run_path: root.join("run"),
            vm_root_path: root.join("vm"),
            vm_data_root_path: root.join("data"),
            os_cache_root_path: root.join("os-cache"),
        }
    }

    #[tokio::test]
    async fn filesystem_storage_create_and_open_validate_directories() -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_storage_root();
        let sys_conf = test_system_configuration(&root);
        let mut storage = StorageFilesystem::default();
        let mut metadata = Metadataless {};
        let host_id = HostId::String("host".to_string());
        let fx_id = FxId::String("fx".to_string());

        storage
            .sys_storage_create(&sys_conf, &host_id, &mut metadata, &fx_id, &ResourceConstraintless)
            .await?;
        storage.sys_storage_open(&host_id, &mut metadata, &fx_id, ResourceConstraintless).await?;
        storage.sys_storage_close(&host_id, &mut metadata, &fx_id, ResourceConstraintless).await?;

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[tokio::test]
    async fn filesystem_storage_resize_reports_unsupported_requests() {
        let mut storage = StorageFilesystem::default();
        let mut metadata = Metadataless {};
        let request = StorageResizeRequest {
            filepath: StorageConfigurationDisk {
                id: "disk".to_string(),
                path: PathBuf::from("disk.img"),
                size: ByteSize::b(1),
                bootable: false,
            },
            new_size: ByteSize::b(2),
            dir: StorageResizeRequestDirection::Grow,
        };

        let result = storage
            .sys_storage_resize(
                &HostId::String("host".to_string()),
                &mut metadata,
                &FxId::String("fx".to_string()),
                ResourceConstraintless,
                vec![request],
            )
            .await;

        assert!(matches!(result, Err(StorageFilesystemError::Unsupported(_))));
    }
}
