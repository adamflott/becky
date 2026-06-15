use crate::QemuSupportedArch;
use std::str::FromStr;

fn shell_escape_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | ',' | '=' | '+'))
    {
        return arg.to_string();
    }

    let escaped = arg.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

pub fn convert_cmd_line_arg_to_qemu_instance(args: Vec<String>) -> Result<QemuSupportedArch, String> {
    if args.is_empty() {
        return Err("no QEMU command arguments supplied".to_string());
    }

    convert_cmd_line_to_qemu_instance(&args.iter().map(|arg| shell_escape_arg(arg)).collect::<Vec<_>>().join(" "))
}

pub fn convert_cmd_line_to_qemu_instance(command_line: &str) -> Result<QemuSupportedArch, String> {
    QemuSupportedArch::from_str(command_line)
}

#[cfg(test)]
mod tests {
    use super::convert_cmd_line_arg_to_qemu_instance;

    #[test]
    fn rejects_empty_command_args() {
        assert!(convert_cmd_line_arg_to_qemu_instance(Vec::new()).is_err());
    }
}
