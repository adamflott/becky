//! Resource and machine configuration types for managed effects.

use crate::empy_implementations::Metadataless;
use crate::metadata::MetadataManager;
use crate::os::{OsImageFileType, SupportedOs};
use bon::Builder;
use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fmt::Debug;
use std::path::PathBuf;

/// Disk image or block-device configuration for an effect.
#[derive(Builder, Debug, Clone, Hash, Ord, PartialEq, Eq, PartialOrd, Serialize, Deserialize)]
pub struct StorageConfigurationDisk {
    /// Provider-local storage identifier.
    pub id: String,
    /// Host path to the disk.
    pub path: PathBuf,
    /// Requested or observed disk size.
    pub size: ByteSize,
    /// Whether this disk is bootable.
    pub bootable: bool,
}

/// ISO image configuration for an effect.
#[derive(Builder, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConfigurationIso {
    /// Provider-local storage identifier.
    pub id: String,
    /// Host path to the ISO.
    pub path: PathBuf,
    /// Whether this ISO should be attached as bootable media.
    pub bootable: bool,
}

/// Cloud image configuration for an effect.
#[derive(Builder, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConfigurationCloudImage {
    /// Provider-local image identifier.
    pub id: String,
    /// Host path to the cloud image.
    pub path: PathBuf,
    /// Operating system represented by this image.
    pub os: SupportedOs,
    /// Image file format.
    pub os_type: OsImageFileType,
}

/// Network configuration for an effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkingConfiguration {
    /// User-mode networking.
    User,
}

/// Guest bootstrap mechanism.
#[derive(Debug, Clone, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
pub enum BootStrapMethod {
    /// Use cloud-init metadata.
    CloudInit,
    /// Do not run a bootstrap mechanism.
    None,
}

/// Resource constraints needed to translate metadata into provider-specific
/// configuration.
///
/// Implementations usually describe storage, network, CPU, and memory
/// requirements for a concrete provider.
pub trait FxResourceConstraints: Send + Sync + Debug {
    /// Metadata input type used to build provider configuration.
    type Metadata: Send + Sync + Debug + MetadataManager;

    /// Provider-specific storage configuration type.
    type FxStorageConfiguration: Send + Sync + Debug;
    /// Provider-specific full configuration type.
    type FxConfiguration: Send + Sync + Debug;
    /// Error returned when metadata cannot be converted into provider config.
    type FxConfigurationError: Send + Sync + Debug;

    /// Converts metadata into provider-specific effect configuration.
    fn convert_from_metadata_to_fx_configuration(&self, mdt: Self::Metadata) -> Result<Self::FxConfiguration, Self::FxConfigurationError>;

    /// Returns provider-specific storage configuration.
    fn storage_configurations(&self) -> Self::FxStorageConfiguration;
}

/// Empty resource-constraint implementation.
#[derive(Debug)]
pub struct ResourceConstraintless;

impl FxResourceConstraints for ResourceConstraintless {
    type Metadata = Metadataless;
    type FxStorageConfiguration = ();
    type FxConfiguration = ();
    type FxConfigurationError = Infallible;

    fn convert_from_metadata_to_fx_configuration(&self, _mdt: Self::Metadata) -> Result<Self::FxConfiguration, Self::FxConfigurationError> {
        Ok(())
    }

    fn storage_configurations(&self) -> Self::FxStorageConfiguration {}
}
