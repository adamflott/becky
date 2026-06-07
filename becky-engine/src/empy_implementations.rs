//! Empty and no-op implementations for tests and simple providers.
//!
//! These types are useful when a caller does not need metadata, application
//! events, or metrics but still needs to satisfy engine trait bounds.

use async_trait::async_trait;
use std::fmt::Debug;

use becky_fx_id::FxId;
use serde::{Serialize, de::DeserializeOwned};

use crate::host_id::*;
use crate::metadata::{MetadataInit, MetadataInventory, MetadataJobUpdate, MetadataManager, MetadataSource, MetadataUpdate};
use crate::state::FxExecutionState;

/// Metadata implementation that accepts all operations and stores nothing.
#[derive(Clone, Debug)]
pub struct Metadataless {}

#[async_trait]
impl MetadataInit for Metadataless {
    type MetadataInitError = ();
    async fn metadata_init(&self) -> Result<(), Self::MetadataInitError> {
        Ok(())
    }
}

#[async_trait]
impl MetadataSource for Metadataless {
    type MetadataSourceHandle = ();
    type MetadataConnectError = ();

    async fn reconnect(&mut self) -> Result<Self::MetadataSourceHandle, Self::MetadataConnectError> {
        Ok(())
    }

    async fn disconnect(&mut self) -> () {}
}

#[async_trait]
impl MetadataUpdate for Metadataless {
    type MetadataUpdateResult = ();
    type MetadataUpdateError = ();

    async fn metadata_fx_state_update(
        &mut self,
        _host_id: &HostId,
        _fxid: &FxId,
        _state: FxExecutionState,
    ) -> Result<Self::MetadataUpdateResult, Self::MetadataUpdateError> {
        Ok(())
    }
}

#[async_trait]
impl MetadataJobUpdate for Metadataless {
    async fn metadata_fx_job_update(&mut self, _host_id: &HostId, _state: FxExecutionState, _job_id: Self::MetadataUpdateResult) {}
}

#[async_trait]
impl MetadataInventory for Metadataless {
    type MetadataInventoryResult = ();
    type MetadataInventoryError = ();

    async fn metadata_fx_record_upsert<T>(
        &mut self,
        _host_id: &HostId,
        _provider: &str,
        _fxid: &FxId,
        _record: T,
    ) -> Result<Self::MetadataInventoryResult, Self::MetadataInventoryError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + Debug,
    {
        Ok(())
    }

    async fn metadata_fx_record_get<T>(&mut self, _host_id: &HostId, _provider: &str, _fxid: &FxId) -> Result<Option<T>, Self::MetadataInventoryError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + Debug,
    {
        Ok(None)
    }

    async fn metadata_fx_record_list<T>(&mut self, _host_id: &HostId, _provider: &str) -> Result<Vec<(FxId, T)>, Self::MetadataInventoryError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + Debug,
    {
        Ok(Vec::new())
    }

    async fn metadata_fx_record_delete(
        &mut self,
        _host_id: &HostId,
        _provider: &str,
        _fxid: &FxId,
    ) -> Result<Self::MetadataInventoryResult, Self::MetadataInventoryError> {
        Ok(())
    }
}

impl MetadataManager for Metadataless {}

/// Marker event type for engines that do not emit application events.
pub struct Eventless;
/// Marker metric type for engines that do not emit custom metrics.
pub struct Metricless;
