//! Host lifecycle and registration traits.

use async_trait::async_trait;
use std::error::Error;
use wora::o11y::HostInfo;
use wora::prelude::*;

use crate::host_id::HostId;
use crate::metadata::MetadataManager;

#[async_trait]
/// Host setup hooks run by an engine during startup.
pub trait HostSysInit<Event, Metric> {
    /// Performs early host boot work before full setup.
    async fn host_system_boot(&mut self, exec: &impl AsyncExecutor<Event, Metric>, fs: &(impl WFS + 'static), hi: &HostInfo) -> Result<(), ()>;

    /// Performs host setup using executor, filesystem, and host information.
    async fn host_system_setup(&mut self, exec: &impl AsyncExecutor<Event, Metric>, fs: &(impl WFS + 'static), hi: &HostInfo) -> Result<(), ()>;
}

#[async_trait]
/// Host teardown hooks run by an engine during shutdown.
pub trait HostSysEnd<Event, Metric> {
    /// Performs host cleanup before the engine exits.
    async fn host_system_end(&mut self, exec: impl AsyncExecutor<Event, Metric>, fs: impl WFS + 'static, hi: &HostInfo) -> Result<(), ()>;
}

/// How a host registers and unregisters itself to be included in the set of usable hosts.
#[async_trait]
pub trait RegisterHost: MetadataManager {
    type RegisterError: Sync + Send + Error;

    /// Register a host by its `HostId` into the metadata backing store.
    async fn register(&mut self, host: &HostId) -> Result<(), Self::RegisterError>;
    /// Unregister a host by its `HostId` into the metadata backing store.
    async fn unregister(&mut self, host: &HostId) -> Result<(), Self::RegisterError>;

    /// Determine whether a `HostId` is registered in the metadata backing store.
    async fn is_registered(&self, host: &HostId) -> Result<bool, Self::RegisterError>;

    /// Used for observability, unlikely need to override.
    fn register_type(&self) -> &str {
        std::any::type_name::<Self>()
    }
}
