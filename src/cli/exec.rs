use std::path::Path;

use clap::Args;

use crate::ssh;
use crate::state::store::StateStore;
use crate::state::vm;
use crate::state::vm::VmStatus;

#[derive(Args)]
pub struct ExecArgs {
    /// VM name
    pub vm_name: String,

    /// User to run the command as (default: from VM metadata)
    #[arg(long)]
    pub user: Option<String>,

    /// Command to execute (everything after --)
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub fn run(args: &ExecArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.vm_name)?;

    if metadata.status != VmStatus::Running {
        anyhow::bail!(
            "vm '{}' is {} — start it first with: ember vm start {}",
            args.vm_name,
            metadata.status,
            args.vm_name
        );
    }

    let network = metadata.network.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "vm '{}' has no network configured — cannot connect via SSH",
            args.vm_name
        )
    })?;

    let guest_ip = &network.guest_ip;
    let key_path = &metadata.ssh.key;
    let user = args.user.as_deref().unwrap_or(&metadata.ssh.user);

    // Build the remote command string from the argument vector.
    let command = shell_escape_join(&args.command);

    let rt = tokio::runtime::Runtime::new()?;
    let exit_code = rt.block_on(async {
        let mut client = ssh::client::connect(guest_ip, user, key_path).await?;
        let code = ssh::exec::exec(&mut client, &command).await?;
        let _ = client.close().await;
        Ok::<u32, anyhow::Error>(code)
    })?;

    if exit_code != 0 {
        std::process::exit(exit_code as i32);
    }

    Ok(())
}

/// Join command arguments into a single shell command string.
///
/// Arguments containing spaces, quotes, or shell metacharacters are
/// single-quoted. This matches the behavior expected by remote shells.
fn shell_escape_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || "\"'\\$`!#&|;(){}[]<>?*~".contains(c)) {
                // Single-quote the argument, escaping any embedded single quotes.
                format!("'{}'", arg.replace('\'', "'\\''"))
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
        let args = vec!["bash".to_string(), "-c".to_string(), "echo $HOME".to_string()];
        assert_eq!(shell_escape_join(&args), "bash -c 'echo $HOME'");
    }

    #[test]
    fn empty_argument() {
        let args = vec!["cmd".to_string(), "".to_string()];
        assert_eq!(shell_escape_join(&args), "cmd ''");
    }
}
