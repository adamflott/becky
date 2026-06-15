//! Core traits and shared types for the Becky orchestration engine.
//!
//! `becky-engine` defines the contracts used by concrete providers and
//! controllers. It intentionally keeps storage, metadata, host registration,
//! state collection, and effect lifecycle operations as traits so provider
//! crates can plug in different backends.

pub mod boot_methods;
pub mod control;
pub mod cpu;
pub mod empy_implementations;
pub mod exit_codes;
pub mod host;
pub mod host_id;
pub mod machine_conf;
pub mod metadata;
pub mod os;
pub mod state;
pub mod storage;
pub mod sys;
pub mod sys_conf;
pub mod verify;

use async_trait::async_trait;
use std::fmt::Debug;
use sysinfo::DiskUsage;
use tokio::sync::mpsc::Sender;
use wora::prelude::*;

use crate::control::{ControlEngine, FxControl};
use crate::host::{HostSysEnd, HostSysInit, RegisterHost};
use crate::metadata::{MetadataInit, MetadataUpdate};
use crate::state::{StateCollect, StateUpdate};
use crate::sys::{SysScanCollect, SysScanUpdate};
use crate::verify::FxVerify;

#[async_trait]
/// Runs the engine's long-lived reconciliation or event loop.
pub trait MainLoop<Event: Send + 'static, Metric>: MetadataUpdate + ControlEngine {
    /// Executes the main engine loop and returns the retry action requested by
    /// the runtime.
    async fn mainloop(
        &mut self,
        wora: &mut Wora<Event, Metric>,
        _exec: impl AsyncExecutor<Event, Metric>,
        _fs: impl WFS + 'static,
        _metrics: Sender<O11yEvent<Metric>>,
    ) -> MainRetryAction;
}

/// Composite trait for a full Becky engine implementation.
///
/// Implementors are expected to provide host lifecycle hooks, metadata
/// management, system scanning, state collection, effect control, live movement,
/// and verification.
pub trait FxEngine<Event: Send + 'static, Metric, Metadata>:
    Send
    + Sync
    + HostSysInit<Event, Metric>
    + HostSysEnd<Event, Metric>
    + RegisterHost
    + MainLoop<Event, Metric>
    + MetadataInit
    + MetadataUpdate
    + SysScanCollect
    + SysScanUpdate
    + StateCollect
    + StateUpdate
    + FxControl
    + FxVerify
{
}

/// Events emitted by the Becky engine into an embedding application.
#[derive(Debug)]
pub enum EngineEvent<AppEv> {
    /// Application-defined event.
    App(AppEv),
    /// Request to start an effect.
    FxStart,
    /// Request to update an effect.
    FxUpdate,
    /// Request to stop an effect.
    FxStop,
    /// Request to delete an effect.
    FxDelete,
    /// Request to rescan host or provider state.
    Rescan,
}

#[async_trait]
/// Collects accounting metrics for a running effect instance.
pub trait FxAccounting {
    /// Provider-specific handle or instance identifier used for accounting.
    type Instance;
    /// Returns accumulated CPU time for the instance.
    async fn accumulated_cpu_time(&self, i: &Self::Instance) -> u64;
    /// Returns disk I/O usage for the instance.
    async fn disk_usage(&self, i: &Self::Instance) -> DiskUsage;
    /// Returns resident memory for the instance.
    async fn memory(&self, i: &Self::Instance) -> u64;
    /// Returns virtual memory for the instance.
    async fn virtual_memory(&self, i: &Self::Instance) -> u64;
    /// Returns elapsed run time for the instance.
    async fn run_time(&self, i: &Self::Instance) -> u64;
}
