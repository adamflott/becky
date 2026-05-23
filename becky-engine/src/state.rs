//! Execution state and state/stat collection traits.

use async_trait::async_trait;
use std::process::ExitStatus;

use crate::control::FxControl;

/// Current observed execution state for a managed effect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FxExecutionState {
    /// The effect has not started yet.
    #[default]
    NotStarted,
    /// Not available to be provisioned or run anywhere
    NotAvailable,
    /// The effect is stopped.
    Stopped,
    /// The effect is paused at the given provider-specific id or pid.
    Paused(u32),
    /// The effect is running at the given provider-specific id or pid.
    Running(u32),
    /// Available hardware not sufficient
    Unrunnable,
    /// The effect cannot make progress until an external condition clears.
    Blocked,
    /// The effect cannot run because billing or quota is insufficient.
    InsufficientFunds,
    // case where try_wait() returns no pid
    /// The provider cannot determine the current state.
    Unknown,
    /// The process exited with an OS exit status.
    Exited(ExitStatus),
    /// The provider reported an error string.
    Error(String),
}

/// Desired execution state that the engine should converge toward.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum FxDesiredExecutionState {
    /// The effect should be running.
    #[default]
    Running,
    /// The effect should be stopped.
    Stopped,
    /// The effect should be paused.
    Paused,
}

/// Collects the stats from the fx
#[async_trait]
pub trait StatsCollect: FxControl {
    /// Stats collection result type.
    type FxStatCollectResult;
    /// Stats collection error type.
    type FxStatCollectError;
    /// Collects provider-specific statistics for a started effect handle.
    async fn stat_collect(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStatCollectResult, Self::FxStatCollectError>;
}

/// Assembles the state of the fx
#[async_trait]
pub trait StateCollect: FxControl {
    /// State collection result type.
    type FxStateCollectResult;
    /// State collection error type.
    type FxStateCollectError;
    /// Collects provider-specific state for a started effect handle.
    async fn state_collect(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Self::FxStateCollectResult, Self::FxStateCollectError>;
}

#[async_trait]
/// Updates engine or metadata state after state collection.
pub trait StateUpdate {}
