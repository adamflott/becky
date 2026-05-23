//! Effect lifecycle and engine control traits.
//!
//! Providers implement [`FxControl`] for a single manageable effect type. Engine
//! implementations expose broader health, listing, info, and shutdown endpoints
//! through [`ControlEngine`].

use std::fmt::Debug;
use std::time;

use crate::FxAccounting;
use crate::host_id::HostId;
use crate::machine_conf::FxResourceConstraints;
use crate::metadata::MetadataManager;
use crate::storage::SysStorage;
use async_trait::async_trait;
use becky_fx_id::FxId;

#[async_trait]
/// Lifecycle operations for a managed Becky effect.
pub trait FxControl: Send + Sync + Debug + FxAccounting {
    /// Stable provider-local identifier type for this effect.
    type Id;

    /// Returns the provider-local identifier for this effect.
    fn id(&self) -> Self::Id;

    /// Allocation result type.
    type FxAllocateResult;
    /// Allocation error type.
    type FxAllocateError;
    /// Allocates any host, metadata, or storage resources required before start.
    async fn fx_allocate<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxAllocateResult, Self::FxAllocateError>;

    /// Spawn/start result type, usually a process or provider handle.
    type FxSpawnResult;
    /// Spawn/start error type.
    type FxSpawnError;
    /// Starts the effect or attaches to an existing instance.
    async fn fx_start<T: MetadataManager>(
        &mut self,
        host_id: &HostId,
        fx_id: &FxId,
        mdt: &mut T,
        rc: &impl FxResourceConstraints,
        storage: &mut impl SysStorage,
    ) -> Result<Self::FxSpawnResult, Self::FxSpawnError>;

    /// Status result type.
    type FxStatusResult;
    /// Status error type.
    type FxStatusError;
    /// Returns the current status for a started effect handle.
    async fn fx_status(&mut self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxStatusResult, Self::FxStatusError>;

    /// Graceful stop result type.
    type FxStopResult;
    /// Graceful stop error type.
    type FxStopError;
    /// Requests graceful termination of the effect.
    async fn fx_stop(&mut self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxStopResult, Self::FxStopError>;

    /// Forced destroy result type.
    type FxDestroyResult;
    /// Forced destroy error type.
    type FxDestroyError;
    /// Forcibly destroys the effect.
    async fn fx_destroy(&self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxDestroyResult, Self::FxDestroyError>;

    /// Archive/checkpoint result type.
    type FxArchiveResult;
    /// Archive/checkpoint error type.
    type FxArchiveError;
    /// Archives or checkpoints the effect when the provider supports it.
    async fn fx_archive(&self, fnr: &mut Self::FxSpawnResult) -> Result<Self::FxArchiveResult, Self::FxArchiveError>;
}

/// Policy used when an engine resumes while effects may already exist.
pub enum ControlEngineFxRestartPolicy {
    /// Attach to compatible existing effects.
    AttachFxOnResume,
    /// Kill compatible existing effects and start new ones.
    KillFxThenStartFxOnResume,
    /// Kill effects when the engine exits, optionally after a grace period.
    KillFxOnExit(Option<std::time::Duration>),
    /// Request effect shutdown when the engine exits, optionally after a grace period.
    ShutdownFxOnExit(Option<std::time::Duration>),
    /// TODO figure out if possible
    DestroyState,
}

#[async_trait]
/// External control API for an engine implementation.
pub trait ControlEngine {
    /// Ping error type.
    type FxPingErr: Debug;
    /// Shutdown error type.
    type FxShutdownErr: Debug;
    /// Effect-list result type.
    type FxList: Debug;
    /// Effect-list error type.
    type FxListErr: Debug;
    /// Effect-info result type.
    type FxInfo: Debug;
    /// Effect-info error type.
    type FxInfoErr: Debug;
    /// Checks whether the control endpoint is responsive before `deadline`.
    async fn ctl_ping(&mut self, deadline: time::Duration) -> Result<(), Self::FxPingErr>;

    /// Returns the configured restart policy for effect reconciliation.
    async fn ctl_fx_restart_policy(&self) -> ControlEngineFxRestartPolicy;

    /// Lists known effects before `deadline`.
    async fn ctl_fx_list(&mut self, deadline: time::Duration) -> Result<Self::FxList, Self::FxListErr>;

    /// Returns detailed effect information before `deadline`.
    async fn ctl_fx_info(&mut self, deadline: time::Duration) -> Result<Self::FxInfo, Self::FxInfoErr>;

    /// Requests engine shutdown before `deadline`.
    async fn ctl_fx_shutdown(&mut self, deadline: time::Duration) -> Result<(), Self::FxShutdownErr>;
}
