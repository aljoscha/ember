//! SSH client connection with retry/backoff.
//!
//! Establishes SSH connections to guest VMs using russh. After a VM boots,
//! SSH may not be immediately available, so [`connect`] retries with
//! exponential backoff up to a configurable timeout (~30s by default).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::keys::key::PublicKey;
use russh_keys::load_secret_key;
use tokio::time::sleep;

use crate::error::Error;

/// Default timeout waiting for SSH to become available after VM boot.
const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Initial retry delay (doubles each attempt).
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Maximum delay between retries.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// SSH port on the guest.
const SSH_PORT: u16 = 22;

/// A connected SSH session to a guest VM.
///
/// Wraps a russh `Handle` and provides methods for command execution
/// and file transfer. Created via [`connect`].
pub struct SshClient {
    handle: client::Handle<SshHandler>,
}

impl SshClient {
    /// Return a reference to the underlying russh handle.
    ///
    /// Used by `ssh::exec` and `ssh::copy` to open channels.
    pub fn handle(&self) -> &client::Handle<SshHandler> {
        &self.handle
    }

    /// Return a mutable reference to the underlying russh handle.
    pub fn handle_mut(&mut self) -> &mut client::Handle<SshHandler> {
        &mut self.handle
    }

    /// Close the SSH connection.
    pub async fn close(self) -> Result<(), Error> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await
            .map_err(|e| Error::Ssh(format!("disconnect failed: {e}")))?;
        Ok(())
    }
}

/// Connect to a guest VM over SSH with exponential-backoff retry.
///
/// Loads the private key from `key_path`, then attempts to connect to
/// `guest_ip:22` as `user`. Retries on connection failure with exponential
/// backoff (250ms → 500ms → 1s → 2s → 2s → ...) up to ~30 seconds.
///
/// Returns an [`SshClient`] on success, or [`Error::Ssh`] if all attempts
/// are exhausted or authentication fails.
pub async fn connect(guest_ip: &str, user: &str, key_path: &Path) -> Result<SshClient, Error> {
    connect_with_timeout(guest_ip, user, key_path, SSH_CONNECT_TIMEOUT).await
}

/// Connect to a guest VM over SSH with a custom timeout.
///
/// Same as [`connect`] but allows overriding the default 30-second timeout.
pub async fn connect_with_timeout(
    guest_ip: &str,
    user: &str,
    key_path: &Path,
    timeout: Duration,
) -> Result<SshClient, Error> {
    let key_pair = load_secret_key(key_path, None).map_err(|e| {
        Error::Ssh(format!(
            "failed to load SSH key {}: {e}",
            key_path.display()
        ))
    })?;

    let config = Arc::new(client::Config::default());

    let addr = format!("{guest_ip}:{SSH_PORT}");
    let start = Instant::now();
    let mut backoff = INITIAL_BACKOFF;
    let mut last_err = String::from("connection not attempted");

    while start.elapsed() < timeout {
        match client::connect(config.clone(), &addr, SshHandler).await {
            Ok(mut handle) => {
                let auth_ok = handle
                    .authenticate_publickey(user, Arc::new(key_pair))
                    .await
                    .map_err(|e| Error::Ssh(format!("authentication failed: {e}")))?;

                if auth_ok {
                    return Ok(SshClient { handle });
                } else {
                    return Err(Error::Ssh(format!(
                        "SSH authentication rejected for user '{user}' with key {}",
                        key_path.display()
                    )));
                }
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }

        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        sleep(backoff.min(remaining)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }

    Err(Error::Ssh(format!(
        "SSH connection to {addr} timed out after {timeout:?}: {last_err}"
    )))
}

/// Minimal SSH client handler.
///
/// Accepts all server host keys (appropriate for local VM connections where
/// the host key is ephemeral and not pre-known). In a public-network
/// context you'd want known_hosts verification here.
pub struct SshHandler;

#[async_trait]
impl Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // Always accept — these are local VMs with ephemeral host keys.
        Ok(true)
    }
}
