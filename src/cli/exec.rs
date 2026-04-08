use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use clap::Args;

use crate::cli::vm::load_running_with_ip;
use ember_core::ssh;
use ember_core::state::store::StateStore;
use ember_core::state::vm;

#[derive(Args)]
pub struct ExecArgs {
    /// VM name
    pub vm_name: String,

    /// User to run the command as (default: from VM metadata)
    #[arg(long)]
    pub user: Option<String>,

    /// Wait up to N seconds for SSH to become available (default: 30)
    #[arg(long, default_value = "30")]
    pub wait: u64,

    /// Force SSH transport (skip vsock even if available)
    #[arg(long)]
    pub ssh: bool,

    /// Command to execute (everything after --)
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub fn run(args: &ExecArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.vm_name)?;
    let command = shell_escape_join(&args.command);

    // Try vsock first (faster, no SSH dependency)
    if !args.ssh {
        if let Some(ref vsock) = metadata.vsock {
            match exec_vsock(&vsock.uds_path, &command) {
                Ok(exit_code) => {
                    if exit_code != 0 {
                        std::process::exit(exit_code);
                    }
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("vsock: {e} — falling back to SSH");
                }
            }
        }
    }

    // SSH fallback
    let (_metadata, network) = load_running_with_ip(&store, &args.vm_name)?;
    let guest_ip = &network.guest_ip;
    let key_path = &metadata.ssh.key;
    let user = args.user.as_deref().unwrap_or(&metadata.ssh.user);
    let timeout = Duration::from_secs(args.wait);

    let rt = tokio::runtime::Runtime::new()?;
    let exit_code = rt.block_on(async {
        let mut client =
            ssh::client::connect_with_timeout(guest_ip, user, key_path, timeout).await?;
        let code = ssh::exec::exec(&mut client, &command).await?;
        let _ = client.close().await;
        Ok::<u32, anyhow::Error>(code)
    })?;

    if exit_code != 0 {
        std::process::exit(exit_code as i32);
    }

    Ok(())
}

/// Execute a command via the emberd vsock daemon.
///
/// Connects to the VM's vsock UDS, sends a JSON-lines exec request,
/// reads the response. Returns the exit code.
fn exec_vsock(uds_path: &Path, command: &str) -> anyhow::Result<i32> {
    let mut stream = UnixStream::connect(uds_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // Send exec request
    let req = serde_json::json!({"op": "exec", "command": command});
    serde_json::to_writer(&mut stream, &req)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    // Read response
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: serde_json::Value = serde_json::from_str(line.trim())?;

    // Print stdout/stderr
    if let Some(stdout) = resp.get("stdout").and_then(|v| v.as_str()) {
        if !stdout.is_empty() {
            print!("{stdout}");
        }
    }
    if let Some(stderr_out) = resp.get("stderr").and_then(|v| v.as_str()) {
        if !stderr_out.is_empty() {
            eprint!("{stderr_out}");
        }
    }

    // Check for errors
    if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("emberd error: {err}");
    }

    Ok(resp
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(1) as i32)
}

/// Join command arguments into a single shell command string.
///
/// If there's a single argument, pass it verbatim — the user composed
/// a shell command (e.g., `ember exec vm -- "echo hi | tee /tmp/out"`).
///
/// If there are multiple arguments, quote any that contain shell
/// metacharacters so they're treated as literal arguments.
fn shell_escape_join(args: &[String]) -> String {
    if args.len() == 1 {
        return args[0].clone();
    }
    args.iter()
        .map(|arg| {
            if arg.is_empty()
                || arg
                    .contains(|c: char| c.is_whitespace() || "\"'\\$`!#&|;(){}[]<>?*~".contains(c))
            {
                ember_core::ssh::copy::shell_quote(arg)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let args = vec!["ls".to_string(), "-la".to_string()];
        assert_eq!(shell_escape_join(&args), "ls -la");
    }

    #[test]
    fn command_with_spaces() {
        let args = vec!["echo".to_string(), "hello world".to_string()];
        assert_eq!(shell_escape_join(&args), "echo 'hello world'");
    }

    #[test]
    fn command_with_single_quotes() {
        let args = vec!["echo".to_string(), "it's".to_string()];
        assert_eq!(shell_escape_join(&args), "echo 'it'\\''s'");
    }

    #[test]
    fn command_with_special_chars() {
        let args = vec![
            "bash".to_string(),
            "-c".to_string(),
            "echo $HOME".to_string(),
        ];
        assert_eq!(shell_escape_join(&args), "bash -c 'echo $HOME'");
    }

    #[test]
    fn empty_argument() {
        let args = vec!["cmd".to_string(), "".to_string()];
        assert_eq!(shell_escape_join(&args), "cmd ''");
    }
}
