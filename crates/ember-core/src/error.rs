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

    /// ember-vz failed to start a macOS VM, with diagnostic context preserved.
    ///
    /// Wraps the underlying error variant (so callers can pattern-match on the
    /// real cause — `CommandExec`, `Vm`, `Io`, etc.) while carrying the last
    /// few lines of `ember-vz.log` and the path to the preserved log copy for
    /// human-readable diagnostics. SEC-469 follow-up to SEC-466: prior to
    /// this variant, the multi-line diagnostic was flattened into
    /// `Error::Vm(String)`, losing the original variant for downstream
    /// retry/categorization logic.
    ///
    /// Display renders the source error followed by the log-tail block and
    /// the preserved-log path; pattern-match `source` (and its dereferenced
    /// inner variants) for programmatic decisions.
    #[error("{}", format_ember_vz_start_failed(source, stderr_tail, preserved_log_path.as_deref()))]
    EmberVzStartFailed {
        #[source]
        source: Box<Error>,
        stderr_tail: Vec<String>,
        preserved_log_path: Option<PathBuf>,
    },
}

/// Render the multi-line diagnostic message for `Error::EmberVzStartFailed`.
///
/// Format mirrors what `crates/ember-macos/src/vm.rs` was previously building
/// inline as a `String`, so operator output stays unchanged from SEC-466.
fn format_ember_vz_start_failed(
    source: &Error,
    stderr_tail: &[String],
    preserved_log_path: Option<&std::path::Path>,
) -> String {
    let mut msg = source.to_string();
    if !stderr_tail.is_empty() {
        msg.push_str("\nember-vz.log (last 10 lines):\n");
        for line in stderr_tail {
            msg.push_str(&format!("  {line}\n"));
        }
    }
    if let Some(p) = preserved_log_path {
        msg.push_str(&format!("preserved at: {}", p.display()));
    }
    msg
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ember_vz_start_failed_preserves_source_variant() {
        // The SEC-466 path used to flatten the underlying error to a String;
        // SEC-469 keeps the variant addressable via `#[source]` so retry/
        // categorization logic can match on the real cause.
        let inner = Error::Vm("ready-fd timeout after 30s".to_string());
        let err = Error::EmberVzStartFailed {
            source: Box::new(inner),
            stderr_tail: vec!["panic: bad MAC".to_string()],
            preserved_log_path: Some(PathBuf::from("/var/lib/ember/failed-starts/x.log")),
        };

        // Pattern-match the wrapping variant.
        match &err {
            Error::EmberVzStartFailed { source, .. } => {
                // And further pattern-match on the inner cause.
                match source.as_ref() {
                    Error::Vm(s) => assert!(s.contains("ready-fd timeout")),
                    other => panic!("expected inner Error::Vm, got {other:?}"),
                }
            }
            other => panic!("expected EmberVzStartFailed, got {other:?}"),
        }
    }

    #[test]
    fn ember_vz_start_failed_renders_multiline_diagnostic() {
        let err = Error::EmberVzStartFailed {
            source: Box::new(Error::Vm("ready-fd timeout".to_string())),
            stderr_tail: vec!["line one".to_string(), "line two".to_string()],
            preserved_log_path: Some(PathBuf::from("/tmp/preserved.log")),
        };
        let rendered = err.to_string();
        // Source error first.
        assert!(rendered.contains("ready-fd timeout"));
        // Log-tail block.
        assert!(rendered.contains("ember-vz.log (last 10 lines):"));
        assert!(rendered.contains("  line one"));
        assert!(rendered.contains("  line two"));
        // Preserved-path footer.
        assert!(rendered.contains("preserved at: /tmp/preserved.log"));
    }

    #[test]
    fn ember_vz_start_failed_omits_log_tail_when_empty() {
        let err = Error::EmberVzStartFailed {
            source: Box::new(Error::Vm("ready-fd closed".to_string())),
            stderr_tail: vec![],
            preserved_log_path: None,
        };
        let rendered = err.to_string();
        assert_eq!(rendered, "vm: ready-fd closed");
    }

    #[test]
    fn ember_vz_start_failed_source_is_set() {
        // std::error::Error::source() must be non-None so consumers walking
        // the error chain (anyhow, log frameworks) reach the inner variant
        // rather than seeing only the rendered string.
        use std::error::Error as StdError;
        let err = Error::EmberVzStartFailed {
            source: Box::new(Error::Vm("inner".to_string())),
            stderr_tail: vec![],
            preserved_log_path: None,
        };
        assert!(err.source().is_some());
    }
}
