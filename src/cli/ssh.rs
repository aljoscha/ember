use std::path::Path;
use std::process::Command;

use clap::Args;

use crate::state::store::StateStore;
use crate::state::vm;

#[derive(Args)]
pub struct SshArgs {
    /// VM name
    pub name: String,

    /// Command to run (everything after --)
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// Open an SSH session to a VM.
///
/// Invokes the system `ssh` command for full interactive terminal support
/// (PTY, resize, escape sequences). If a command is given after `--`, it
/// is passed to ssh for non-interactive execution.
pub fn run(args: &SshArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let (metadata, network) = vm::load_running_with_network(&store, &args.name)?;

    let guest_ip = &network.guest_ip;
    let user = &metadata.ssh.user;
    let key_path = &metadata.ssh.key;

    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-i", &key_path.to_string_lossy(),
        &format!("{user}@{guest_ip}"),
    ]);

    if !args.command.is_empty() {
        cmd.args(&args.command);
    }

    let status = cmd.status().map_err(|e| {
        anyhow::anyhow!(
            "failed to execute ssh: {e}\n\
             Hint: is the 'ssh' client installed and in PATH?"
        )
    })?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
