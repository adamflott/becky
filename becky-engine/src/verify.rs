//! Verification and reconciliation traits.

use async_trait::async_trait;

use crate::control::FxControl;
use crate::host_id::HostId;
use crate::metadata::MetadataManager;
use crate::state::StateCollect;

/// Verification result comparing metadata, requested state, and observed state.
#[derive(Debug)]
pub enum Verification {
    /// Observed state matches expected state.
    Match,
    /// UUID or unique identifier does not match.
    MismatchUuid,
    /// Name does not match.
    MismatchName,
    /// Expected name is missing.
    MismatchNoName,
    /// Target type does not match.
    MismatchTargetType,
    /// CPU requirements do not match observed state.
    MismatchCpu,
    /// Verification could not determine a result.
    Unknown,
}

// TODO super trait metadata to compare against state
#[async_trait]
/// Verifies that a running effect matches requested metadata and state.
pub trait FxVerify: FxControl + StateCollect {
    /// Verification error type.
    type FxOpVerifyError;
    /// Verifies the given running effect handle.
    async fn fx_op_verify(&mut self, handle: &mut Self::FxSpawnResult) -> Result<Verification, Self::FxOpVerifyError>;
}

#[async_trait]
/// Reconciles metadata and host state.
pub trait Reconcile: MetadataManager {
    // : SysScanCollect + MetadataManager + FnControl {
    /// Reconciles state for the given host.
    async fn reconcile(&self, host_id: &HostId);
}
