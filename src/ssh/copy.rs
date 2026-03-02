//! SCP-style file transfer over SSH.
//!
//! Transfers files between the host and a guest VM using SSH exec channels.
//! Upload pipes local file data through `cat > <path>` on the remote;
//! download reads `cat <path>` output and writes it locally.
//!
//! Directory transfers use tar piped through an SSH channel.

use std::path::Path;

use russh::ChannelMsg;
use tokio::process::Command;

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

    tokio::fs::write(local_path, &file_data)
        .await
        .map_err(|e| Error::Io {
            path: local_path.to_path_buf(),
            source: e,
        })?;

    Ok(())
}

/// Copy a local directory to a remote VM via SSH.
///
/// Creates a tar archive of the local directory and extracts it on the
/// remote side.
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

    // Create tar archive from local directory.
    let tar_output = Command::new("tar")
        .args(["-cf", "-", "-C"])
        .arg(parent)
        .arg(basename)
        .output()
        .await
        .map_err(|e| Error::Ssh(format!("failed to run local tar: {e}")))?;

    if !tar_output.status.success() {
        let stderr = String::from_utf8_lossy(&tar_output.stderr);
        return Err(Error::Ssh(format!("local tar failed: {stderr}")));
    }

    // Extract on the remote side.
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

    channel
        .data(&tar_output.stdout[..])
        .await
        .map_err(|e| Error::Ssh(format!("failed to send tar data: {e}")))?;

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
/// Runs tar on the remote side to pack the directory, then extracts
/// it locally.
pub async fn download_dir(
    client: &mut SshClient,
    remote_path: &str,
    local_path: &Path,
) -> Result<(), Error> {
    // Split remote path into parent and basename for tar -C.
    let remote = Path::new(remote_path);
    let parent = remote
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("/");
    let basename = remote
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Ssh("remote directory has no name".to_string()))?;

    // Create tar archive on the remote side.
    let mut channel = client
        .handle_mut()
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("failed to open session channel: {e}")))?;

    let command = format!("tar -cf - -C {} {}", shell_quote(parent), shell_quote(basename));
    channel
        .exec(true, command.as_str())
        .await
        .map_err(|e| Error::Ssh(format!("failed to execute remote command: {e}")))?;

    let mut tar_data = Vec::new();
    let mut exit_code: Option<u32> = None;
    let mut stderr_buf = Vec::new();

    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        match msg {
            ChannelMsg::Data { ref data } => {
                tar_data.extend_from_slice(data);
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

    // Create local destination and extract.
    tokio::fs::create_dir_all(local_path)
        .await
        .map_err(|e| Error::Io {
            path: local_path.to_path_buf(),
            source: e,
        })?;

    let mut child = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(local_path)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Ssh(format!("failed to run local tar: {e}")))?;

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin.write_all(&tar_data).await.map_err(|e| {
            Error::Ssh(format!("failed to write tar data to local tar: {e}"))
        })?;
        // Drop stdin to close the pipe so tar can finish.
    }

    let output = child
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
