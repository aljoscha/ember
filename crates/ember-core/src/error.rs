use std::path::PathBuf;

use thiserror::Error;

/// Unified error type for ember.
#[derive(Debug, Error)]
pub enum Error {
    /// A shelled-out command exited with a non-zero status.
    #[error("{command} failed (exit code {exit_code}): {stderr}")]
    Command {
        command: String,
        exit_code: i32,
        stderr: String,
    },

    /// A shelled-out command could not be spawned.
    #[error("failed to execute '{command}' — is it installed and in PATH?")]
    CommandExec {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// ZFS operation failed.
    #[error("zfs: {0}")]
    Zfs(String),

    /// Firecracker API or process error.
    #[error("firecracker: {0}")]
    Firecracker(String),

    /// Networking error (TAP, IP allocation, NAT).
    #[error("network: {0}")]
    Network(String),

    /// VM hypervisor lifecycle error.
    #[error("vm: {0}")]
    Vm(String),

    /// Image pull or unpack error.
    #[error("image: {0}")]
    Image(String),

    /// Vsock CID allocation error.
    #[error("vsock: {0}")]
    Vsock(String),

    /// SSH connection or command error.
    #[error("ssh: {0}")]
    Ssh(String),

    /// State store error.
    #[error("state: {0}")]
    State(String),

    /// Config parsing or validation error.
    #[error("config: {0}")]
    Config(String),

    /// VM not found.
    #[error("vm '{name}' not found — run 'ember vm list' to see available VMs")]
    VmNotFound { name: String },

    /// Image not found locally.
    #[error("image '{name}' not found")]
    ImageNotFound { name: String },

    /// VM is in the wrong state for this operation.
    #[error("vm '{name}' is {actual}, expected {expected}")]
    VmWrongState {
        name: String,
        actual: String,
        expected: String,
    },

    /// Root privileges required.
    #[error("this operation requires root privileges")]
    RootRequired,

    /// File I/O error with path context.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// JSON serialization or deserialization error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// YAML parsing error.
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// SQLite error from the embedded allocator state DB.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Convenience alias used throughout ember.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Create a `Command` error from a finished `std::process::Output`.
    ///
    /// Returns `Ok(output)` if the command succeeded.
    pub fn check_command(
        command: &str,
        output: std::process::Output,
    ) -> Result<std::process::Output> {
        if output.status.success() {
            return Ok(output);
        }
        Err(Error::Command {
            command: command.to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}
