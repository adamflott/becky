//! Metadata source, update, and image synchronization traits.

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use std::path::{Path, PathBuf};

use becky_fx_id::FxId;

use crate::host_id::*;
use crate::os::SupportedOs;
use crate::state::FxExecutionState;

/// Initialize the metadata source.
#[async_trait]
pub trait MetadataInit {
    /// Initialization error type.
    type MetadataInitError: Debug;
    /// Initializes the metadata source before use.
    async fn metadata_init(&self) -> Result<(), Self::MetadataInitError>;
}

/// Re-establish connection, and disconnect from metadata source.
#[async_trait]
pub trait MetadataSource {
    /// Connected metadata handle type.
    type MetadataSourceHandle: Send + Sync + Debug;
    /// Metadata connection error type.
    type MetadataConnectError: Send + Sync + Debug;
    /// Reconnects to the metadata source.
    async fn reconnect(&mut self) -> Result<Self::MetadataSourceHandle, Self::MetadataConnectError>;
    /// Disconnects from the metadata source.
    async fn disconnect(&mut self) -> ();
}

/// Update a function's state in the metadata.
#[async_trait]
pub trait MetadataUpdate: Send + Sync {
    /// Metadata update result type, often a job or revision identifier.
    type MetadataUpdateResult: Send + Sync + Debug;
    /// Metadata update error type.
    type MetadataUpdateError: Send + Sync + Debug;

    /// Updates the execution state for an effect on a host.
    async fn metadata_fx_state_update(
        &mut self,
        host_id: &HostId,
        fxid: &FxId,
        state: FxExecutionState,
    ) -> Result<Self::MetadataUpdateResult, Self::MetadataUpdateError>;
}

#[async_trait]
/// Updates asynchronous metadata jobs created by state updates.
pub trait MetadataJobUpdate: MetadataUpdate {
    /// Updates the state of a metadata update job.
    async fn metadata_fx_job_update(&mut self, host_id: &HostId, state: FxExecutionState, job_id: Self::MetadataUpdateResult);
}

/// Persists provider-specific effect inventory records.
///
/// This trait is intentionally generic over the record payload so metadata
/// backends can store QEMU VMs, containers, process functions, or other effect
/// kinds without the engine knowing their schemas. SQL implementations can
/// serialize `record` into a JSON/BLOB column keyed by host, provider, and
/// [`FxId`]; file implementations can write the same payload to provider
/// directories.
#[async_trait]
pub trait MetadataInventory: Send + Sync {
    /// Metadata inventory operation result type, often a revision identifier.
    type MetadataInventoryResult: Send + Sync + Debug;
    /// Metadata inventory operation error type.
    type MetadataInventoryError: Send + Sync + Debug;

    /// Inserts or replaces the provider-specific record for an effect.
    async fn metadata_fx_record_upsert<T>(
        &mut self,
        host_id: &HostId,
        provider: &str,
        fxid: &FxId,
        record: T,
    ) -> Result<Self::MetadataInventoryResult, Self::MetadataInventoryError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + Debug;

    /// Loads the provider-specific record for an effect.
    async fn metadata_fx_record_get<T>(&mut self, host_id: &HostId, provider: &str, fxid: &FxId) -> Result<Option<T>, Self::MetadataInventoryError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + Debug;

    /// Lists provider-specific records for a host/provider pair.
    async fn metadata_fx_record_list<T>(&mut self, host_id: &HostId, provider: &str) -> Result<Vec<(FxId, T)>, Self::MetadataInventoryError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + Debug;

    /// Deletes the provider-specific record for an effect.
    async fn metadata_fx_record_delete(
        &mut self,
        host_id: &HostId,
        provider: &str,
        fxid: &FxId,
    ) -> Result<Self::MetadataInventoryResult, Self::MetadataInventoryError>;
}

#[async_trait]
/// Synchronizes operating-system images into a local cache.
pub trait OsImage: Send + Sync {
    /// Provider-specific image definition type.
    type ImageDef;
    /// Image synchronization error type.
    type SyncError: Debug;
    // async fn sync_image(&self, cache_root_dir: &PathBuf, image: Self::ImageDef) -> Result<(), ()>;
    /// Synchronizes all configured images into `cache_root_dir`.
    async fn sync_images(&mut self, cache_root_dir: &PathBuf) -> Result<(), Self::SyncError>;

    /// Returns the cache filename for a supported OS.
    fn get_filename(&self, image: &SupportedOs) -> PathBuf;
}

/// Composite trait for complete metadata backends.
pub trait MetadataManager: MetadataSource + MetadataUpdate + MetadataJobUpdate + MetadataInventory + Sync + Send {}
