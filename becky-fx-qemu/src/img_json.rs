use serde;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QemuImgCheck {
    #[serde(rename = "image-end-offset")]
    pub image_end_offset: i64,
    #[serde(rename = "total-clusters")]
    pub total_clusters: i64,
    #[serde(rename = "check-errors")]
    pub check_errors: i64,
    pub filename: String,
    pub format: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QemuImgInfo {
    #[serde(default)]
    pub children: Vec<Children>,
    #[serde(rename = "virtual-size")]
    pub virtual_size: i64,
    pub filename: String,
    #[serde(rename = "cluster-size")]
    pub cluster_size: Option<i64>,
    pub format: String,
    #[serde(rename = "actual-size")]
    pub actual_size: Option<i64>,
    #[serde(rename = "format-specific")]
    pub format_specific: Option<FormatSpecificRoot>,
    #[serde(rename = "dirty-flag")]
    pub dirty_flag: Option<bool>,
}

impl QemuImgInfo {
    /// Returns whether `qemu-img info` reported the image as corrupt.
    ///
    /// QEMU only reports this field for some formats, notably qcow2. Raw images
    /// and older QEMU versions may omit the whole `format-specific` subtree.
    pub fn is_corrupt(&self) -> bool {
        self.format_specific
            .as_ref()
            .map(|format_specific| format_specific.data.corrupt)
            .unwrap_or(false)
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Children {
    pub name: String,
    pub info: Info,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    #[serde(default)]
    pub children: Vec<Value>,
    #[serde(rename = "virtual-size")]
    pub virtual_size: i64,
    pub filename: String,
    pub format: String,
    #[serde(rename = "actual-size")]
    pub actual_size: Option<i64>,
    #[serde(rename = "format-specific")]
    pub format_specific: Option<FormatSpecific>,
    #[serde(rename = "dirty-flag")]
    pub dirty_flag: Option<bool>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatSpecific {
    #[serde(rename = "type")]
    pub type_field: String,
    #[serde(default)]
    pub data: Data,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Data {}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatSpecificRoot {
    #[serde(rename = "type")]
    pub type_field: String,
    #[serde(default)]
    pub data: Data2,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Data2 {
    #[serde(default)]
    pub compat: String,
    #[serde(rename = "compression-type")]
    #[serde(default)]
    pub compression_type: String,
    #[serde(rename = "lazy-refcounts")]
    #[serde(default)]
    pub lazy_refcounts: bool,
    #[serde(rename = "refcount-bits")]
    pub refcount_bits: Option<i64>,
    #[serde(default)]
    pub corrupt: bool,
    #[serde(rename = "extended-l2")]
    #[serde(default)]
    pub extended_l2: bool,
}

#[cfg(test)]
mod tests {
    use super::QemuImgInfo;

    #[test]
    fn parses_raw_image_info_without_format_specific() -> Result<(), serde_json::Error> {
        let info = serde_json::from_str::<QemuImgInfo>(
            r#"{
                "virtual-size": 1048576,
                "filename": "/tmp/disk.img",
                "format": "raw",
                "actual-size": 4096,
                "dirty-flag": false
            }"#,
        )?;

        assert_eq!(info.format, "raw");
        assert!(!info.is_corrupt());
        Ok(())
    }

    #[test]
    fn parses_qcow2_corruption_flag() -> Result<(), serde_json::Error> {
        let info = serde_json::from_str::<QemuImgInfo>(
            r#"{
                "virtual-size": 1048576,
                "filename": "/tmp/disk.qcow2",
                "cluster-size": 65536,
                "format": "qcow2",
                "actual-size": 196616,
                "format-specific": {
                    "type": "qcow2",
                    "data": {
                        "compat": "1.1",
                        "compression-type": "zlib",
                        "lazy-refcounts": false,
                        "refcount-bits": 16,
                        "corrupt": true,
                        "extended-l2": false
                    }
                },
                "dirty-flag": false
            }"#,
        )?;

        assert!(info.is_corrupt());
        Ok(())
    }
}
