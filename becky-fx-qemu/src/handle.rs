use becky_utils::Process;
use qapi::futures::{QapiService, QgaStreamTokio, QmpStreamTokio};
use qapi::qga::{GuestAgentCommandInfo, GuestAgentInfo};
use qapi::qmp::{SchemaInfo, VersionInfo};
use tokio::task::JoinHandle;

pub struct QemuHandle {
    pub process: Process,
    /// Unix Domain Socket to control QEMU through QMP
    pub ctl: QapiService<QmpStreamTokio<tokio::io::WriteHalf<tokio::net::UnixStream>>>,
    /// if connected, contains the QEMU version
    pub(crate) version: Option<VersionInfo>,
    /// if connected, contains the accepted JSON schema for QMP
    pub(crate) schema: Vec<SchemaInfo>,
    /// Background reader that forwards QMP events from the socket.
    pub(crate) event_reader: JoinHandle<()>,
    pub ga: Option<QapiService<QgaStreamTokio<tokio::io::WriteHalf<tokio::net::UnixStream>>>>,
    /// Guest-agent version and supported command inventory, when QGA is enabled
    /// and ready.
    pub(crate) ga_info: Option<GuestAgentInfo>,
    /// Background guest-agent service task, when QGA is enabled.
    pub(crate) ga_task: Option<JoinHandle<()>>,
}

impl QemuHandle {
    pub fn version(&self) -> Option<&VersionInfo> {
        self.version.as_ref()
    }

    pub fn supported_command(&self, cmd: &str) -> bool {
        schema_supports_command(&self.schema, cmd)
    }

    pub fn guest_agent_info(&self) -> Option<&GuestAgentInfo> {
        self.ga_info.as_ref()
    }

    pub fn supported_ga_command(&self, cmd: &str) -> bool {
        self.ga_info.as_ref().is_some_and(|info| ga_supports_command(&info.supported_commands, cmd))
    }

    pub async fn stop_event_reader(&mut self) {
        self.event_reader.abort();
        let _ = (&mut self.event_reader).await;
        if let Some(ga_task) = self.ga_task.take() {
            ga_task.abort();
            let _ = ga_task.await;
        }
    }
}

pub(crate) fn schema_supports_command(schema: &[SchemaInfo], cmd: &str) -> bool {
    schema.iter().any(|schema| match schema {
        SchemaInfo::command { base, .. } => base.name.eq_ignore_ascii_case(cmd),
        _ => false,
    })
}

pub(crate) fn ga_supports_command(commands: &[GuestAgentCommandInfo], cmd: &str) -> bool {
    commands.iter().any(|command| command.enabled && command_name_matches(&command.name, cmd))
}

fn command_name_matches(left: &str, right: &str) -> bool {
    normalize_command_name(left) == normalize_command_name(right)
}

fn normalize_command_name(value: &str) -> String {
    value.replace('_', "-").to_ascii_lowercase()
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

    #[test]
    fn ga_supports_command_matches_enabled_commands_only() {
        let commands = vec![
            GuestAgentCommandInfo {
                enabled: true,
                name: "guest-ping".to_string(),
                success_response: true,
            },
            GuestAgentCommandInfo {
                enabled: false,
                name: "guest-shutdown".to_string(),
                success_response: true,
            },
        ];

        assert!(ga_supports_command(&commands, "GUEST_PING"));
        assert!(!ga_supports_command(&commands, "guest-shutdown"));
        assert!(!ga_supports_command(&commands, "guest-info"));
    }
}
