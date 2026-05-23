//! System scan and destruction traits.

use async_trait::async_trait;

use crate::host_id::HostId;
use crate::metadata::MetadataManager;

#[async_trait]
/// Collects host/system scan data into metadata.
pub trait SysScanCollect {
    /// System-scan result type.
    type SysScanCollectResult;
    /// System-scan error type.
    type SysScanCollectError;
    /// Scans host state and records it through the metadata manager.
    async fn sys_scan_collect<T: MetadataManager + Send + Sync>(
        &mut self,
        host_id: &HostId,
        mdm: &mut T,
    ) -> Result<Self::SysScanCollectResult, Self::SysScanCollectError>;
}
#[async_trait]
/// Updates system scan state after collection.
pub trait SysScanUpdate {}

#[async_trait]
/// Destroys provider-managed system resources.
pub trait SysDestroy {}
