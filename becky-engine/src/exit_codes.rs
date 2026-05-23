//! Process exit codes returned by Becky binaries.

use std::process::ExitCode;
use tracing::error;

/// Structured process termination reasons.
#[derive(Clone, Debug)]
pub enum BeckyExitCode {
    /// Command-line arguments were invalid.
    InvalidArgs,
    /// Metadata initialization failed.
    MetadataInitFailure,
    /// A required remote service was unavailable.
    RemoteServiceUnavailable,
    /// The process ended because of a Unix signal.
    UnixSignalEnd,
    /// The process failed while waiting for OS image synchronization.
    SyncingImages,
}

impl std::process::Termination for BeckyExitCode {
    fn report(self) -> ExitCode {
        match self {
            BeckyExitCode::InvalidArgs => ExitCode::from(1),
            BeckyExitCode::MetadataInitFailure => ExitCode::from(2),
            BeckyExitCode::RemoteServiceUnavailable => {
                error!("Service unavailable");
                ExitCode::from(3)
            }

            BeckyExitCode::UnixSignalEnd => ExitCode::from(4),
            BeckyExitCode::SyncingImages => ExitCode::from(5),
        }
    }
}
