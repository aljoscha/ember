//! Thin HTTP-over-Unix-socket client for the Firecracker API.
//!
//! Firecracker exposes a REST API on a Unix socket. This module wraps
//! each endpoint in a typed async method using hyper + hyperlocal.

use std::path::{Path, PathBuf};

use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{header, Method, Request};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Firecracker API request/response types
// ---------------------------------------------------------------------------

/// Machine configuration (vcpu count, memory).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
}

/// Kernel boot source configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
}

/// Block device (drive) attached to the VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

/// Network interface attached to the VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
}

/// Instance action (start, stop, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceAction {
    pub action_type: ActionType,
}

/// Action types supported by the Firecracker API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionType {
    InstanceStart,
    SendCtrlAltDel,
    FlushMetrics,
}

/// VM state update for pause/resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmStateUpdate {
    pub state: VmState,
}

/// VM states that can be set via PATCH /vm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VmState {
    Paused,
    Resumed,
}

/// Vsock device attached to the VM.
///
/// Firecracker creates a Unix domain socket at `uds_path` on the host.
/// Guest programs connect via `AF_VSOCK` to CID 2 (host); host programs
/// connect to the UDS and specify the guest CID + port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vsock {
    pub vsock_id: String,
    pub guest_cid: u32,
    pub uds_path: String,
}

/// Error body returned by Firecracker on failure.
#[derive(Debug, Deserialize)]
struct FaultResponse {
    fault_message: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Async client for the Firecracker REST API over a Unix socket.
pub struct FirecrackerClient {
    socket_path: PathBuf,
    client: Client<UnixConnector, Full<Bytes>>,
}

impl FirecrackerClient {
    /// Create a new client targeting the given Unix socket path.
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            client: Client::unix(),
        }
    }

    /// `PUT /machine-config`
    pub async fn put_machine_config(&self, config: &MachineConfig) -> anyhow::Result<()> {
        self.put("/machine-config", config).await
    }

    /// `PUT /boot-source`
    pub async fn put_boot_source(&self, boot: &BootSource) -> anyhow::Result<()> {
        self.put("/boot-source", boot).await
    }

    /// `PUT /drives/{drive_id}`
    pub async fn put_drive(&self, drive: &Drive) -> anyhow::Result<()> {
        let path = format!("/drives/{}", drive.drive_id);
        self.put(&path, drive).await
    }

    /// `PUT /network-interfaces/{iface_id}`
    pub async fn put_network_interface(&self, iface: &NetworkInterface) -> anyhow::Result<()> {
        let path = format!("/network-interfaces/{}", iface.iface_id);
        self.put(&path, iface).await
    }

    /// `PUT /vsock` — attach a vsock device to the VM.
    pub async fn put_vsock(&self, vsock: &Vsock) -> anyhow::Result<()> {
        self.put("/vsock", vsock).await
    }

    /// `PUT /actions` — start the VM, send Ctrl+Alt+Del, etc.
    pub async fn put_action(&self, action: &InstanceAction) -> anyhow::Result<()> {
        self.put("/actions", action).await
    }

    /// `PATCH /vm` — pause or resume the VM.
    pub async fn patch_vm(&self, update: &VmStateUpdate) -> anyhow::Result<()> {
        self.send(Method::PATCH, "/vm", Some(update)).await?;
        Ok(())
    }

    /// `GET /machine-config` — retrieve current machine configuration.
    pub async fn get_machine_config(&self) -> anyhow::Result<MachineConfig> {
        let bytes = self
            .send::<()>(Method::GET, "/machine-config", None)
            .await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Send a PUT request with a JSON body and check for success.
    async fn put<S: Serialize>(&self, path: &str, body: &S) -> anyhow::Result<()> {
        self.send(Method::PUT, path, Some(body)).await?;
        Ok(())
    }

    /// Send an HTTP request to Firecracker and return the raw response bytes.
    ///
    /// Returns an error if the response status is not 2xx, extracting the
    /// `fault_message` from the Firecracker error body when available.
    async fn send<S: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&S>,
    ) -> anyhow::Result<Bytes> {
        let uri: hyper::Uri = Uri::new(&self.socket_path, path).into();

        let req_body = match body {
            Some(b) => Full::new(Bytes::from(serde_json::to_vec(b)?)),
            None => Full::new(Bytes::new()),
        };

        let mut builder = Request::builder().method(&method).uri(uri);

        if body.is_some() {
            builder = builder
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json");
        }

        let req = builder.body(req_body)?;

        let response = self
            .client
            .request(req)
            .await
            .context("firecracker API request failed")?;

        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .context("failed to read firecracker response")?
            .to_bytes();

        if !status.is_success() {
            let message = serde_json::from_slice::<FaultResponse>(&bytes)
                .map(|f| f.fault_message)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
            anyhow::bail!("firecracker {method} {path} returned {status}: {message}");
        }

        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Convenience constructors for action types
// ---------------------------------------------------------------------------

impl InstanceAction {
    pub fn instance_start() -> Self {
        Self {
            action_type: ActionType::InstanceStart,
        }
    }

    pub fn send_ctrl_alt_del() -> Self {
        Self {
            action_type: ActionType::SendCtrlAltDel,
        }
    }
}

impl VmStateUpdate {
    pub fn pause() -> Self {
        Self {
            state: VmState::Paused,
        }
    }

    pub fn resume() -> Self {
        Self {
            state: VmState::Resumed,
        }
    }
}
