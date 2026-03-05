//! Remote command execution over SSH.
//!
//! Opens an SSH session channel, runs the given command, and streams
//! stdout/stderr back to the host in real time. Returns the remote
//! process exit code.

use russh::ChannelMsg;
use tokio::io::{stderr, stdout, AsyncWriteExt};

use crate::error::Error;
use crate::ssh::client::SshClient;

/// Execute a command on a remote VM via an established SSH connection.
///
/// Streams stdout and stderr to the host's respective streams as data
/// arrives. Returns the remote process exit code (0 for success).
pub async fn exec(client: &mut SshClient, command: &str) -> Result<u32, Error> {
    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute command: {e}")))?;

    let mut exit_code: Option<u32> = None;
    let mut out = stdout();
    let mut err = stderr();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::Data { ref data } => {
                out.write_all(data)
                    .await
                    .map_err(|e| Error::Ssh(format!("failed to write stdout: {e}")))?;
                out.flush()
                    .await
                    .map_err(|e| Error::Ssh(format!("failed to flush stdout: {e}")))?;
            }
            ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                err.write_all(data)
                    .await
                    .map_err(|e| Error::Ssh(format!("failed to write stderr: {e}")))?;
                err.flush()
                    .await
                    .map_err(|e| Error::Ssh(format!("failed to flush stderr: {e}")))?;
            }
            ChannelMsg::ExitStatus { exit_status } => {
                exit_code = Some(exit_status);
            }
            _ => {}
        }
    }

    Ok(exit_code.unwrap_or(1))
}
