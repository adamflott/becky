use crate::img_json::QemuImgInfo;
use crate::{QEMU_BIN_IMG, QemuStorageCreateError};
use becky_utils::{CommandOptions, run_system_command};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `qemu-img` format identifier for qcow2-backed disks.
pub const QEMU_IMG_FORMAT_QCOW2: &str = "qcow2";
/// File extension used for qcow2 images managed by this backend.
pub const QEMU_IMG_FILE_EXT_QCOW2: &str = QEMU_IMG_FORMAT_QCOW2;
/// `qemu-img` format identifier for raw disk images.
pub const QEMU_IMG_FORMAT_RAW: &str = "raw";
/// File extension used for raw disk images managed by this backend.
pub const QEMU_IMG_FILE_EXIT_RAW: &str = "img";

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct QcowOptions {
    pub backing_file: Option<PathBuf>,
    pub backing_format: Option<String>,
    pub preallocation: Option<QcowPreallocation>,
    pub compat: Option<String>,
    pub cluster_size: Option<u64>,
    pub lazy_refcounts: Option<bool>,
    pub extended_l2: Option<bool>,
}

impl QcowOptions {
    pub(crate) fn create_options(&self) -> Vec<String> {
        let mut opts = Vec::new();

        if let Some(backing_file) = &self.backing_file {
            opts.push(format!("backing_file={}", backing_file.display()));
        }
        if let Some(backing_format) = &self.backing_format {
            opts.push(format!("backing_fmt={backing_format}"));
        }
        if let Some(preallocation) = &self.preallocation {
            opts.push(format!("preallocation={}", preallocation.as_qemu_arg()));
        }
        if let Some(compat) = &self.compat {
            opts.push(format!("compat={compat}"));
        }
        if let Some(cluster_size) = self.cluster_size {
            opts.push(format!("cluster_size={cluster_size}"));
        }
        if let Some(lazy_refcounts) = self.lazy_refcounts {
            opts.push(format!("lazy_refcounts={}", if lazy_refcounts { "on" } else { "off" }));
        }
        if let Some(extended_l2) = self.extended_l2 {
            opts.push(format!("extended_l2={}", if extended_l2 { "on" } else { "off" }));
        }

        opts
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum QcowPreallocation {
    Off,
    Metadata,
    Falloc,
    Full,
}

impl QcowPreallocation {
    fn as_qemu_arg(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Metadata => "metadata",
            Self::Falloc => "falloc",
            Self::Full => "full",
        }
    }
}

#[derive(Debug)]
/// Result of inspecting a storage artifact with `qemu-img`.
///
/// Callers currently only rely on the error variant to determine whether storage is usable.
pub enum QemuImgResult {
    /// Parsed metadata returned by `qemu-img info`.
    Info(QemuImgInfo),
    /// Failure encountered while inspecting or validating the image.
    Err(QemuStorageCreateError),
}

pub enum QemuResizeType {
    /// allow operation when the new size is smaller than the original
    Shrink,
    /// specify FMT-specific preallocation type for the new areas
    Preallocation,
}

// TODO add utility function to generate filename. or add to interface?

pub(crate) async fn is_qcow_image_corrupt(filename: &Path) -> Result<(), QemuStorageCreateError> {
    let cmd = run_system_command(
        QEMU_BIN_IMG,
        vec!["info", "--force-share", "--output", "json", filename.display().to_string().as_str()],
        CommandOptions::default(),
    )
    .await?;

    let img = serde_json::from_slice::<QemuImgInfo>(cmd.output.stdout.as_slice())?;
    if img.is_corrupt() {
        Err(QemuStorageCreateError::CorruptImage(filename.to_path_buf()))
    } else {
        Ok(())
    }
}
