use std::path::Path;

use anyhow::{Context as _, Result};
use collections::HashMap;
use serde::Deserialize;

use crate::shell::ShellKind;

fn parse_env_map_from_noisy_output(output: &str) -> Result<collections::HashMap<String, String>> {
    for (position, _) in output.match_indices('{') {
        let candidate = &output[position..];
        let mut deserializer = serde_json::Deserializer::from_str(candidate);
        if let Ok(env_map) = HashMap::<String, String>::deserialize(&mut deserializer) {
            return Ok(env_map);
        }
    }
    anyhow::bail!("Failed to find JSON in shell output: {output}")
}

pub fn print_env() {
    let env_vars: HashMap<String, String> = std::env::vars().collect();
    let json = serde_json::to_string_pretty(&env_vars).unwrap_or_else(|err| {
        eprintln!("Error serializing environment variables: {}", err);
        std::process::exit(1);
    });
    println!("{}", json);
}

/// Capture all environment variables from the login shell in the given directory.
pub async fn capture(
    shell_path: impl AsRef<Path>,
    args: &[String],
    directory: impl AsRef<Path>,
) -> Result<collections::HashMap<String, String>> {
    return capture_unix(shell_path.as_ref(), args, directory.as_ref()).await;
}

/// Try to parse the environment output before checking the exit status.
/// The user's shell rc files may contain commands that fail (e.g. editor
/// integrations that call posix_spawnp outside a real PTY), causing a
/// non-zero exit status even though `zed --printenv` ran successfully and
/// produced valid output on its separate fd.
fn parse_env_output(
    env_output: &str,
    status: &std::process::ExitStatus,
    successful_capture_warning: impl FnOnce() -> String,
    failed_capture_error: impl FnOnce() -> String,
) -> Result<collections::HashMap<String, String>> {
    match parse_env_map_from_noisy_output(env_output) {
        Ok(env_map) => {
            if !status.success() {
                log::warn!("{}", successful_capture_warning());
            }
            Ok(env_map)
        }
        Err(parse_error) => {
            if !status.success() {
                anyhow::bail!(
                    "{}. Failed to deserialize environment variables from json: {parse_error}. output: {env_output}",
                    failed_capture_error(),
                );
            }

            anyhow::bail!(
                "Failed to deserialize environment variables from json: {parse_error}. output: {env_output}"
            );
        }
    }
}

async fn capture_unix(
    shell_path: &Path,
    args: &[String],
    directory: &Path,
) -> Result<collections::HashMap<String, String>> {


    use crate::command::new_std_command;

    let shell_kind = ShellKind::new(shell_path);
    let quoted_zed_path = super::get_shell_safe_zed_path(shell_kind)?;

    let mut command_string = String::new();
    let mut command = new_std_command(shell_path);
    command.args(args);

    match shell_kind {
        ShellKind::Fish => {
            // in fish, asdf, direnv attach to the `fish_prompt` event
            command_string.push_str("emit fish_prompt;");
            command.arg("-l");
        }
        _ => {
            command.arg("-l");
        }
    }

    command.args(["-i", "-c"]);

    // Prefix with "./" if the path starts with "-" to prevent cd from interpreting it as a flag
    let dir_str = directory.to_string_lossy();
    let dir_str = if dir_str.starts_with('-') {
        format!("./{dir_str}").into()
    } else {
        dir_str
    };
    let quoted_dir = shell_kind
        .try_quote(&dir_str)
        .context("unexpected null in directory name")?;

    // cd into the directory, triggering directory specific side-effects (asdf, direnv, etc)
    command_string.push_str(&format!("cd {};", quoted_dir));

    command_string.push_str(&format!("{} --printenv {}", quoted_zed_path, format!(">&0")));

    command.arg(&command_string);

    super::set_pre_exec_to_start_new_session(&mut command);

    let (env_output, process_output) = spawn_and_read_fd(command, 0).await?;
    let env_output = String::from_utf8_lossy(&env_output);

    parse_env_output(
        &env_output,
        &process_output.status,
        || {
            format!(
                "login shell exited with {} but environment was captured successfully. stderr: {:?}",
                process_output.status,
                String::from_utf8_lossy(&process_output.stderr),
            )
        },
        || {
            format!(
                "login shell exited with {}. stdout: {:?}, stderr: {:?}",
                process_output.status,
                String::from_utf8_lossy(&process_output.stdout),
                String::from_utf8_lossy(&process_output.stderr),
            )
        },
    )
}

async fn spawn_and_read_fd(
    mut command: std::process::Command,
    child_fd: std::os::fd::RawFd,
) -> anyhow::Result<(Vec<u8>, std::process::Output)> {
    use command_fds::{CommandFdExt, FdMapping};
    use std::{io::Read, process::Stdio};

    let (mut reader, writer) = std::io::pipe()?;

    command.fd_mappings(vec![FdMapping {
        parent_fd: writer.into(),
        child_fd,
    }])?;

    let process = smol::process::Command::from(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;

    Ok((buffer, process.output().await?))
}
