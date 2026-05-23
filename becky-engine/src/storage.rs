//! Storage management traits and simple storage backends.

use async_trait::async_trait;
use becky_fx_id::FxId;
use bytesize::ByteSize;
use std::fmt::Debug;

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

/// Filesystem-backed storage backend that creates Becky working directories.
pub struct StorageFilesystem {}

#[async_trait]
impl SysStorage for StorageFilesystem {
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
    type SysStorageCreateError = std::io::Error;

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
        tokio::fs::create_dir_all(&sys_conf.run_path).await?;
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
