use becky_utils::Process;
use qapi::futures::{QapiService, QgaStreamTokio, QmpStreamTokio};
use qapi::qmp::{SchemaInfo, VersionInfo};

pub struct QemuHandle {
    pub process: Process,
    /// Unix Domain Socket to control QEMU through QMP
    pub ctl: QapiService<QmpStreamTokio<tokio::io::WriteHalf<tokio::net::UnixStream>>>,
    /// if connected, contains the QEMU version
    pub(crate) version: Option<VersionInfo>,
    /// if connected, contains the accepted JSON schema for QMP
    pub(crate) schema: Vec<SchemaInfo>,
    pub ga: Option<QapiService<QgaStreamTokio<tokio::io::WriteHalf<tokio::net::UnixStream>>>>,
}

impl QemuHandle {
    pub fn supported_command(&self, cmd: &str) -> bool {
        let mut found = false;
        for schema in &self.schema {
            match schema {
                SchemaInfo::command { base, .. } if base.name.eq_ignore_ascii_case(cmd) => {
                    found = true;
                    break;
                }
                _ => {}
            }
        }
        found
    }
}
