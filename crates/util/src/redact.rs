use std::sync::LazyLock;

static REDACT_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"([A-Z_][A-Z0-9_]*)=("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\S+)"#).unwrap()
});

/// Whether a given environment variable name should have its value redacted
pub fn should_redact(env_var_name: &str) -> bool {
    const REDACTED_SUFFIXES: &[&str] = &[
        "KEY",
        "TOKEN",
        "PASSWORD",
        "SECRET",
        "PASS",
        "CREDENTIALS",
        "LICENSE",
    ];
    REDACTED_SUFFIXES
        .iter()
        .any(|suffix| env_var_name.ends_with(suffix))
}

pub fn is_valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character != '=' && !character.is_control())
}

/// Redact a string which could include a command with environment variables
pub fn redact_command(command: &str) -> String {
    REDACT_REGEX
        .replace_all(command, |caps: &regex::Captures| {
            let var_name = &caps[1];
            let value = &caps[2];
            if should_redact(var_name) {
                format!(r#"{}="[REDACTED]""#, var_name)
            } else {
                format!("{}={}", var_name, value)
            }
        })
        .to_string()
}

