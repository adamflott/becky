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
    pub children: Vec<Children>,
    #[serde(rename = "virtual-size")]
    pub virtual_size: i64,
    pub filename: String,
    #[serde(rename = "cluster-size")]
    pub cluster_size: i64,
    pub format: String,
    #[serde(rename = "actual-size")]
    pub actual_size: i64,
    #[serde(rename = "format-specific")]
    pub format_specific: FormatSpecificRoot,
    #[serde(rename = "dirty-flag")]
    pub dirty_flag: bool,
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
    pub children: Vec<Value>,
    #[serde(rename = "virtual-size")]
    pub virtual_size: i64,
    pub filename: String,
    pub format: String,
    #[serde(rename = "actual-size")]
    pub actual_size: i64,
    #[serde(rename = "format-specific")]
    pub format_specific: FormatSpecific,
    #[serde(rename = "dirty-flag")]
    pub dirty_flag: bool,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatSpecific {
    #[serde(rename = "type")]
    pub type_field: String,
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
    pub data: Data2,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Data2 {
    pub compat: String,
    #[serde(rename = "compression-type")]
    pub compression_type: String,
    #[serde(rename = "lazy-refcounts")]
    pub lazy_refcounts: bool,
    #[serde(rename = "refcount-bits")]
    pub refcount_bits: i64,
    pub corrupt: bool,
    #[serde(rename = "extended-l2")]
    pub extended_l2: bool,
}
