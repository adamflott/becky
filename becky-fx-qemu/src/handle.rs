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
    pub fn version(&self) -> Option<&VersionInfo> {
        self.version.as_ref()
    }

    pub fn supported_command(&self, cmd: &str) -> bool {
        schema_supports_command(&self.schema, cmd)
    }
}

pub(crate) fn schema_supports_command(schema: &[SchemaInfo], cmd: &str) -> bool {
    schema.iter().any(|schema| match schema {
        SchemaInfo::command { base, .. } => base.name.eq_ignore_ascii_case(cmd),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qapi::qmp::{SchemaInfoBase, SchemaInfoCommand};

    #[test]
    fn schema_supports_command_matches_case_insensitively() {
        let schema = vec![SchemaInfo::command {
            base: SchemaInfoBase {
                features: None,
                name: "query-status".to_string(),
            },
            command: SchemaInfoCommand {
                allow_oob: None,
                arg_type: "q_empty".to_string(),
                ret_type: "StatusInfo".to_string(),
            },
        }];

        assert!(schema_supports_command(&schema, "QUERY-STATUS"));
        assert!(!schema_supports_command(&schema, "query-block"));
    }
}
