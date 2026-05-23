//! Operating-system image metadata and download helpers.

use clap::ValueEnum;
use reqwest::Client;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

/// Errors returned while downloading an OS image or other file.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("Failed to download file, I/O error:{0}")]
    Io(#[from] std::io::Error),

    #[error("Request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
}

/// Downloads `url` to `filename` using an async HTTP client.
pub async fn download_file(url: &str, filename: &PathBuf) -> Result<(), DownloadError> {
    let client = Client::new();
    let mut response = client.get(url).send().await?;
    let mut file = File::create(filename).await?;

    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }
    Ok(())
}

/// CPU architecture used by OS images.
#[derive(Debug, Clone, Hash, PartialEq, PartialOrd, Eq, Ord, clap::ValueEnum)]
pub enum Arch {
    /// AMD64/x86_64 architecture.
    Amd64,
    /// AArch64/ARM64 architecture.
    Aarch64,
}

//impl_enum_type!(Arch);

impl FromStr for Arch {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "amd64" => Ok(Arch::Amd64),
            "aarch64" => Ok(Arch::Aarch64),
            _ => Err(()),
        }
    }
}

impl Display for Arch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Arch::Amd64 => write!(f, "amd64"),
            Arch::Aarch64 => write!(f, "aarch64"),
        }
    }
}

/// Supported Debian image brands.
#[derive(Debug, Clone, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub enum Debian {
    /// Debian 13 "Trixie".
    Trixie13,
}

/// Supported Alpine image brands.
#[derive(Debug, Clone, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub enum Alpine {
    /// Alpine Linux 3.22.1.
    Version3_22_1,
}

/// Supported Red Hat image brands.
#[derive(Debug, Clone, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub enum RedHat {}

/// Supported OS image file formats.
#[derive(Debug, Clone, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub enum OsImageFileType {
    /// ISO image.
    Iso,
    /// QCOW2 disk image.
    Qcow2,
    /// Raw disk image.
    Raw,
}

impl Display for OsImageFileType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            OsImageFileType::Iso => write!(f, "iso"),
            OsImageFileType::Qcow2 => write!(f, "qcow2"),
            OsImageFileType::Raw => write!(f, "raw"),
        }
    }
}

/// Linux distribution variants supported by Becky.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum LinuxDistro {
    /// Debian Linux.
    Debian { arch: Arch, brand: Debian },
    /// Alpine Linux.
    Alpine { arch: Arch, brand: Alpine },
    /// Red Hat Linux.
    RedHat { arch: Arch, brand: RedHat },
}

/// CLI-friendly Linux distribution selector.
#[derive(Clone, Debug, Hash, PartialOrd, PartialEq, Eq, Ord, ValueEnum)]
pub enum SupportedLinuxDistroType {
    /// Debian Linux.
    Debian,
    /// Alpine Linux.
    Alpine,
    /// Red Hat Linux.
    RedHat,
}

impl Display for SupportedLinuxDistroType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SupportedLinuxDistroType::Debian => write!(f, "debian"),
            SupportedLinuxDistroType::Alpine => write!(f, "alpine"),
            SupportedLinuxDistroType::RedHat => write!(f, "redhat"),
        }
    }
}

/// Operating systems supported by Becky-managed images.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum SupportedOs {
    /// Linux distributions.
    Linux(LinuxDistro),
}

/// CLI-friendly operating-system selector.
#[derive(Clone, Debug, Hash, PartialOrd, PartialEq, Eq, Ord, ValueEnum)]
pub enum SupportedOsType {
    /// Linux operating systems.
    Linux,
}

impl Display for SupportedOsType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SupportedOsType::Linux => write!(f, "linux"),
        }
    }
}
