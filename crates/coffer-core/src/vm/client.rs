//! Firecracker REST API client over Unix domain socket.

use hyper::{body::Bytes, Method, Request, StatusCode};
use hyper_util::client::legacy::connect::Connected;
use hyper_util::rt::TokioIo;
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;
use tokio::net::UnixStream;

use crate::error::{CofferError, Result};

/// A Unix domain socket connector for hyper.
#[derive(Clone)]
pub struct UnixConnector;

impl tower_service::Service<hyperlocal::Uri> for UnixConnector {
    type Response = TokioIo<UnixStream>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: hyperlocal::Uri) -> Self::Future {
        let path = uri.path().to_owned();
        Box::pin(async move {
            let stream = UnixStream::connect(path).await?;
            Ok(TokioIo::new(stream))
        })
    }
}

/// Firecracker HTTP client.
#[derive(Clone)]
pub struct FirecrackerClient {
    client: hyper_util::client::legacy::Client<UnixConnector, hyper::body::Incoming>,
    socket_path: std::path::PathBuf,
}

impl FirecrackerClient {
    pub fn new(socket_path: &Path) -> Result<Self> {
        let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(UnixConnector);
        Ok(Self {
            client,
            socket_path: socket_path.to_path_buf(),
        })
    }

    /// Send a JSON request and parse the JSON response.
    pub async fn request<Req, Resp>(&self, method: Method, path: &str, body: Option<Req>) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let uri = hyperlocal::Uri::new(&self.socket_path, path);
        let mut builder = Request::builder().method(method).uri(uri);

        let req = if let Some(body) = body {
            let json = serde_json::to_vec(&body)?;
            builder = builder.header("Content-Type", "application/json");
            builder.body(hyper::body::Incoming::from(Bytes::from(json)))?
        } else {
            builder.body(hyper::body::Incoming::from(Bytes::new()))?
        };

        let resp = self.client.request(req).await.map_err(|e| {
            CofferError::Internal(format!("HTTP request failed: {}", e))
        })?;

        let status = resp.status();
        let body_bytes = resp.collect().await.map_err(|e| {
            CofferError::Internal(format!("Body read failed: {}", e))
        })?.to_bytes();

        if status.is_success() {
            if body_bytes.is_empty() {
                // For empty responses, try to parse as () or let caller handle
                serde_json::from_slice(&body_bytes)
                    .map_err(|e| CofferError::Serialization(e))
            } else {
                serde_json::from_slice(&body_bytes)
                    .map_err(|e| CofferError::Serialization(e))
            }
        } else {
            let message = String::from_utf8_lossy(&body_bytes).to_string();
            Err(CofferError::Firecracker {
                status: status.as_u16(),
                message,
            })
        }
    }

    /// PUT /boot-source
    pub async fn put_boot_source(&self, config: &BootSource) -> Result<()> {
        self.request(Method::PUT, "/boot-source", Some(config)).await
    }

    /// PUT /drives/{drive_id}
    pub async fn put_drive(&self, drive_id: &str, config: &Drive) -> Result<()> {
        let path = format!("/drives/{}", drive_id);
        self.request(Method::PUT, &path, Some(config)).await
    }

    /// PUT /machine-config
    pub async fn put_machine_config(&self, config: &MachineConfig) -> Result<()> {
        self.request(Method::PUT, "/machine-config", Some(config)).await
    }

    /// PUT /network-interfaces/{iface_id}
    pub async fn put_network_interface(&self, iface_id: &str, config: &NetworkInterface) -> Result<()> {
        let path = format!("/network-interfaces/{}", iface_id);
        self.request(Method::PUT, &path, Some(config)).await
    }

    /// PUT /vsock
    pub async fn put_vsock(&self, config: &Vsock) -> Result<()> {
        self.request(Method::PUT, "/vsock", Some(config)).await
    }

    /// PUT /actions
    pub async fn put_action(&self, action: &Action) -> Result<()> {
        self.request(Method::PUT, "/actions", Some(action)).await
    }

    /// PUT /snapshot/create
    pub async fn create_snapshot(&self, req: &SnapshotCreate) -> Result<()> {
        self.request(Method::PUT, "/snapshot/create", Some(req)).await
    }

    /// PUT /snapshot/load
    pub async fn load_snapshot(&self, req: &SnapshotLoad) -> Result<()> {
        self.request(Method::PUT, "/snapshot/load", Some(req)).await
    }

    /// PATCH /vm
    pub async fn patch_vm_state(&self, state: &VmState) -> Result<()> {
        self.request(Method::PATCH, "/vm", Some(state)).await
    }

    /// GET /machine-config
    pub async fn get_machine_config(&self) -> Result<MachineConfig> {
        self.request(Method::GET, "/machine-config", None::<()>).await
    }
}

// ===================================================================
// Firecracker API Types
// ===================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BootSource {
    pub kernel_image_path: String,
    pub boot_args: Option<String>,
    pub initrd_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partuuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MachineConfig {
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub huge_pages: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    pub guest_mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Vsock {
    pub guest_cid: u32,
    pub uds_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Action {
    pub action_type: ActionType,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "SnakeCase")]
pub enum ActionType {
    InstanceStart,
    SendCtrlAltDel,
    FlushMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotCreate {
    pub snapshot_type: SnapshotType,
    pub snapshot_path: String,
    pub mem_file_path: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotType {
    Full,
    Diff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotLoad {
    pub snapshot_path: String,
    pub mem_backend: MemBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_vm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_diff_snapshots: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MemBackend {
    pub backend_type: MemBackendType,
    pub backend_path: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemBackendType {
    File,
    Uffd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VmState {
    pub state: VmStateEnum,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VmStateEnum {
    Resumed,
    Paused,
}
