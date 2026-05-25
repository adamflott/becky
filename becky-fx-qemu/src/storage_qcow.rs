use crate::img_json::QemuImgInfo;
use crate::{QEMU_BIN_IMG, QemuStorageCreateError};
use becky_utils::{CommandOptions, run_system_command};
use std::path::Path;

/// `qemu-img` format identifier for qcow2-backed disks.
pub const QEMU_IMG_FORMAT_QCOW2: &str = "qcow2";
/// File extension used for qcow2 images managed by this backend.
pub const QEMU_IMG_FILE_EXT_QCOW2: &str = QEMU_IMG_FORMAT_QCOW2;
/// `qemu-img` format identifier for raw disk images.
pub const QEMU_IMG_FORMAT_RAW: &str = "raw";
/// File extension used for raw disk images managed by this backend.
pub const QEMU_IMG_FILE_EXIT_RAW: &str = "img";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum QcowOptions {}

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
    if img.format_specific.data.corrupt {
        Err(QemuStorageCreateError::CorruptImage(filename.to_path_buf()))
    } else {
        Ok(())
    }
}
