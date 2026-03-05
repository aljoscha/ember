//! SCP-style file transfer over SSH.
//!
//! Transfers files between the host and a guest VM using SSH exec channels.
//! Upload pipes local file data through `cat > <path>` on the remote;
//! download reads `cat <path>` output and writes it locally.
//!
//! Directory transfers use tar piped through an SSH channel.
//!
//! All transfers stream data in chunks to avoid buffering entire files in
//! memory.

use std::path::Path;
use std::process::Stdio;

use russh::ChannelMsg;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::error::Error;
use crate::ssh::client::SshClient;

/// Chunk size for streaming data through SSH channels (64 KiB).
const CHUNK_SIZE: usize = 64 * 1024;

/// Copy a local file to a remote VM via SSH.
///
/// Opens an SSH session channel, executes `cat > <remote_path>` on the
/// guest, and streams the local file data through the channel's stdin
/// in [`CHUNK_SIZE`] chunks.
pub async fn upload(
    client: &mut SshClient,
    local_path: &Path,
    remote_path: &str,
) -> Result<(), Error> {
    let mut file = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| Error::Io {
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

    // Stream file data through channel stdin in chunks.
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| Error::Io {
            path: local_path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        channel
            .data(&buf[..n])
            .await
            .map_err(|e| Error::Ssh(format!("failed to send file data: {e}")))?;
    }

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
            ChannelMsg::ExtendedData { ref data, ext: 1 } => {
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
/// guest, and streams each data chunk directly to the local file.
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

    let mut file = tokio::fs::File::create(local_path)
        .await
        .map_err(|e| Error::Io {
            path: local_path.to_path_buf(),
            source: e,
        })?;

    let mut exit_code: Option<u32> = None;
    let mut stderr_buf = Vec::new();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::Data { ref data } => {
                file.write_all(data).await.map_err(|e| Error::Io {
                    path: local_path.to_path_buf(),
                    source: e,
                })?;
            }
            ChannelMsg::ExtendedData { ref data, ext: 1 } => {
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

    Ok(())
}

/// Copy a local directory to a remote VM via SSH.
///
/// Creates a tar archive of the local directory and streams it to the
/// remote side for extraction, without buffering the entire archive.
pub async fn upload_dir(
    client: &mut SshClient,
    local_path: &Path,
    remote_path: &str,
) -> Result<(), Error> {
    let parent = local_path
        .parent()
        .ok_or_else(|| Error::Ssh("local directory has no parent".to_string()))?;
    let basename = local_path
        .file_name()
        .ok_or_else(|| Error::Ssh("local directory has no name".to_string()))?;

    // Spawn tar and stream its stdout to the SSH channel.
    let mut tar_child = Command::new("tar")
        .args(["-cf", "-", "-C"])
        .arg(parent)
        .arg(basename)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Ssh(format!("failed to run local tar: {e}")))?;

    let mut tar_stdout = tar_child.stdout.take().expect("stdout piped");

    // Open remote extraction channel.
    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    let command = format!(
        "mkdir -p {} && tar -xf - -C {}",
        shell_quote(remote_path),
        shell_quote(remote_path)
    );
    channel
        .exec(true, command.as_str())
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute remote command: {e}")))?;

    // Stream tar output through the SSH channel in chunks.
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = tar_stdout
            .read(&mut buf)
            .await
            .map_err(|e| Error::Ssh(format!("failed to read local tar output: {e}")))?;
        if n == 0 {
            break;
        }
        channel
            .data(&buf[..n])
            .await
            .map_err(|e| Error::Ssh(format!("failed to send tar data: {e}")))?;
    }

    // Wait for local tar to finish and check its exit status.
    let tar_output = tar_child
        .wait_with_output()
        .await
        .map_err(|e| Error::Ssh(format!("failed to wait for local tar: {e}")))?;
    if !tar_output.status.success() {
        let stderr = String::from_utf8_lossy(&tar_output.stderr);
        return Err(Error::Ssh(format!("local tar failed: {stderr}")));
    }

    channel
        .eof()
        .await
        .map_err(|e| Error::Ssh(format!("failed to send EOF: {e}")))?;

    let mut exit_code: Option<u32> = None;
    let mut stderr_buf = Vec::new();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::ExtendedData { ref data, ext: 1 } => {
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
            "remote tar extract to '{remote_path}' failed (exit code {code}): {stderr}"
        )));
    }

    Ok(())
}

/// Copy a directory from a remote VM to the local host via SSH.
///
/// Runs tar on the remote side to pack the directory, then streams
/// each data chunk directly into a local tar extraction process.
pub async fn download_dir(
    client: &mut SshClient,
    remote_path: &str,
    local_path: &Path,
) -> Result<(), Error> {
    // Split remote path into parent and basename for tar -C.
    let remote = Path::new(remote_path);
    let parent = remote.parent().and_then(|p| p.to_str()).unwrap_or("/");
    let basename = remote
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Ssh("remote directory has no name".to_string()))?;

    // Create local destination.
    tokio::fs::create_dir_all(local_path)
        .await
        .map_err(|e| Error::Io {
            path: local_path.to_path_buf(),
            source: e,
        })?;

    // Spawn local tar extraction process before reading remote data.
    let mut tar_child = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(local_path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Ssh(format!("failed to run local tar: {e}")))?;

    let mut tar_stdin = tar_child.stdin.take().expect("stdin piped");

    // Create tar archive on the remote side.
    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    let command = format!(
        "tar -cf - -C {} {}",
        shell_quote(parent),
        shell_quote(basename)
    );
    channel
        .exec(true, command.as_str())
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute remote command: {e}")))?;

    let mut exit_code: Option<u32> = None;
    let mut stderr_buf = Vec::new();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::Data { ref data } => {
                tar_stdin.write_all(data).await.map_err(|e| {
                    Error::Ssh(format!("failed to write tar data to local tar: {e}"))
                })?;
            }
            ChannelMsg::ExtendedData { ref data, ext: 1 } => {
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
            "remote tar of '{remote_path}' failed (exit code {code}): {stderr}"
        )));
    }

    // Close stdin so tar can finish, then wait for it.
    drop(tar_stdin);
    let output = tar_child
        .wait_with_output()
        .await
        .map_err(|e| Error::Ssh(format!("failed to wait for local tar: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Ssh(format!(
            "local tar extract to '{}' failed: {stderr}",
            local_path.display()
        )));
    }

    Ok(())
}

/// Check whether a remote path is a directory.
///
/// Returns `true` if the remote `test -d` exits with 0.
pub async fn is_remote_dir(client: &mut SshClient, remote_path: &str) -> Result<bool, Error> {
    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    let command = format!("test -d {}", shell_quote(remote_path));
    channel
        .exec(true, command.as_str())
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute remote command: {e}")))?;

    let mut exit_code: Option<u32> = None;

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        if let ChannelMsg::ExitStatus { exit_status } = msg {
            exit_code = Some(exit_status);
        }
    }

    Ok(exit_code == Some(0))
}

/// Shell-quote a single argument for use in a remote command.
///
/// Wraps the argument in single quotes and escapes any embedded single
/// quotes with the `'\''` idiom.
pub(crate) fn shell_quote(s: &str) -> String {
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
