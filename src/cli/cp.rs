use std::path::Path;

use clap::Args;

use crate::ssh;
use crate::state::store::StateStore;
use crate::state::vm;

#[derive(Args)]
pub struct CpArgs {
    /// Source path (prefix with <vm-name>: for remote)
    pub src: String,

    /// Destination path (prefix with <vm-name>: for remote)
    pub dst: String,
}

pub fn run(args: &CpArgs, state_dir: &Path) -> anyhow::Result<()> {
    let (src_vm, src_path) = parse_path(&args.src);
    let (dst_vm, dst_path) = parse_path(&args.dst);

    let (vm_name, local_path, remote_path, uploading) = match (src_vm, dst_vm) {
        (None, Some(name)) => (name, src_path, dst_path, true),
        (Some(name), None) => (name, dst_path, src_path, false),
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "VM-to-VM copy is not supported — copy to local first, then to the other VM"
            )
        }
        (None, None) => {
            anyhow::bail!("neither path specifies a VM — prefix with <vm-name>: for remote paths")
        }
    };

    if remote_path.is_empty() {
        anyhow::bail!("remote path cannot be empty — use <vm-name>:<path>");
    }

    let store = StateStore::new(state_dir.to_path_buf());
    let (metadata, network) = vm::load_running_with_network(&store, vm_name)?;

    let guest_ip = &network.guest_ip;
    let key_path = &metadata.ssh.key;
    let user = &metadata.ssh.user;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut client = ssh::client::connect(guest_ip, user, key_path).await?;
        if uploading {
            let local = Path::new(local_path);
            if local.is_dir() {
                ssh::copy::upload_dir(&mut client, local, remote_path).await?;
            } else {
                ssh::copy::upload(&mut client, local, remote_path).await?;
            }
        } else {
            let local = Path::new(local_path);
            if ssh::copy::is_remote_dir(&mut client, remote_path).await? {
                ssh::copy::download_dir(&mut client, remote_path, local).await?;
            } else {
                ssh::copy::download(&mut client, remote_path, local).await?;
            }
        }
        let _ = client.close().await;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Parse a cp path argument into an optional VM name and the path portion.
///
/// If the argument contains a `:`, everything before the first `:` is the
/// VM name and everything after is the path. Otherwise the entire argument
/// is treated as a local path.
fn parse_path(arg: &str) -> (Option<&str>, &str) {
    if let Some(colon) = arg.find(':') {
        let name = &arg[..colon];
        let path = &arg[colon + 1..];
        if name.is_empty() {
            // Leading colon — treat as a local path.
            (None, arg)
        } else {
            (Some(name), path)
        }
    } else {
        (None, arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_path() {
        assert_eq!(parse_path("myvm:/etc/hosts"), (Some("myvm"), "/etc/hosts"));
    }

    #[test]
    fn parse_local_path() {
        assert_eq!(parse_path("/tmp/file.txt"), (None, "/tmp/file.txt"));
    }

    #[test]
    fn parse_relative_local_path() {
        assert_eq!(parse_path("file.txt"), (None, "file.txt"));
    }

    #[test]
    fn parse_leading_colon() {
        assert_eq!(parse_path(":/weird"), (None, ":/weird"));
    }

    #[test]
    fn parse_remote_empty_path() {
        assert_eq!(parse_path("myvm:"), (Some("myvm"), ""));
    }
}
