//! SCP-style file transfer over SSH.
//!
//! Transfers files between the host and a guest VM using SSH exec channels.
//! Upload pipes local file data through `cat > <path>` on the remote;
//! download reads `cat <path>` output and writes it locally.

use std::path::Path;

use russh::ChannelMsg;

use crate::error::Error;
use crate::ssh::client::SshClient;

/// Copy a local file to a remote VM via SSH.
///
/// Opens an SSH session channel, executes `cat > <remote_path>` on the
/// guest, and pipes the local file data through the channel's stdin.
pub async fn upload(
    client: &mut SshClient,
    local_path: &Path,
    remote_path: &str,
) -> Result<(), Error> {
    let data = tokio::fs::read(local_path).await.map_err(|e| Error::Io {
        path: local_path.to_path_buf(),
        source: e,
    })?;

    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    let command = format!("cat > {}", shell_quote(remote_path));
    channel
        .exec(true, command.as_str())
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute remote command: {e}")))?;

    // Send file data through channel stdin.
    channel
        .data(&data[..])
        .await
        .map_err(|e| Error::Ssh(format!("failed to send file data: {e}")))?;

    // Signal end of input so `cat` writes and exits.
    channel
        .eof()
        .await
        .map_err(|e| Error::Ssh(format!("failed to send EOF: {e}")))?;

    // Wait for the remote command to finish.
    let mut exit_code: Option<u32> = None;
    let mut stderr_buf = Vec::new();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::ExtendedData { ref data, ext } if ext == 1 => {
                stderr_buf.extend_from_slice(data);
            }
            ChannelMsg::ExitStatus { exit_status } => {
                exit_code = Some(exit_status);
            }
            _ => {}
        }
    }

    let code = exit_code.unwrap_or(1);
    if code != 0 {
        let stderr = String::from_utf8_lossy(&stderr_buf);
        return Err(Error::Ssh(format!(
            "remote write to '{remote_path}' failed (exit code {code}): {stderr}"
        )));
    }

    Ok(())
}

/// Copy a file from a remote VM to the local host via SSH.
///
/// Opens an SSH session channel, executes `cat <remote_path>` on the
/// guest, collects the stdout data, and writes it to the local path.
pub async fn download(
    client: &mut SshClient,
    remote_path: &str,
    local_path: &Path,
) -> Result<(), Error> {
    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    let command = format!("cat {}", shell_quote(remote_path));
    channel
        .exec(true, command.as_str())
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute remote command: {e}")))?;

    let mut file_data = Vec::new();
    let mut exit_code: Option<u32> = None;
    let mut stderr_buf = Vec::new();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::Data { ref data } => {
                file_data.extend_from_slice(data);
            }
            ChannelMsg::ExtendedData { ref data, ext } if ext == 1 => {
                stderr_buf.extend_from_slice(data);
            }
            ChannelMsg::ExitStatus { exit_status } => {
                exit_code = Some(exit_status);
            }
            _ => {}
        }
    }

    let code = exit_code.unwrap_or(1);
    if code != 0 {
        let stderr = String::from_utf8_lossy(&stderr_buf);
        return Err(Error::Ssh(format!(
            "remote read of '{remote_path}' failed (exit code {code}): {stderr}"
        )));
    }

    tokio::fs::write(local_path, &file_data)
        .await
        .map_err(|e| Error::Io {
            path: local_path.to_path_buf(),
            source: e,
        })?;

    Ok(())
}

/// Shell-quote a single argument for use in a remote command.
///
/// Wraps the argument in single quotes and escapes any embedded single
/// quotes with the `'\''` idiom.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_simple_path() {
        assert_eq!(shell_quote("/tmp/file.txt"), "'/tmp/file.txt'");
    }

    #[test]
    fn quote_path_with_spaces() {
        assert_eq!(shell_quote("/tmp/my file.txt"), "'/tmp/my file.txt'");
    }

    #[test]
    fn quote_path_with_single_quotes() {
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }
}
