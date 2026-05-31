//! Boot method configuration for virtualized effects.

use bon::Builder;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// TODO put in in someplace for vms
// pub emulator_method_options: BootMethodOptions,
#[derive(Builder, Debug, Clone, Default)]
/// Paths used by firmware-backed boot methods.
pub struct BootMethodOptions {
    /// Firmware image path.
    pub firmware: PathBuf,
    /// Non-volatile firmware state path.
    pub nv: PathBuf,
}

/// Supported machine firmware boot methods.
#[derive(Clone, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
pub enum BootMethod {
    /// BIOS or legacy boot.
    Bios,
    /// UEFI firmware boot.
    Uefi,
}
