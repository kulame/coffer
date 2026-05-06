//! Firecracker Unix socket REST API client.

use std::path::PathBuf;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::debug;

use crate::error::{CofferError, Result};

/// A lightweight HTTP/1.1 client over Unix domain socket for Firecracker.
pub struct FirecrackerClient {
    socket_path: PathBuf,
}

impl FirecrackerClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub async fn create_vm(&self, config: &MachineConfig) -> Result<()> {
        self.put("/machine-config", config).await
    }

    pub async fn set_kernel(&self, config: &KernelConfig) -> Result<()> {
        self.put("/boot-source", config).await
    }

    pub async fn add_drive(&self, _drive_id: &str, config: &DriveConfig) -> Result<()> {
        self.put("/drives/rootfs", config).await
    }

    pub async fn add_network_interface(&self, config: &NetworkInterfaceConfig) -> Result<()> {
        self.put("/network-interfaces/eth0", config).await
    }

    pub async fn add_vsock(&self, config: &VsockConfig) -> Result<()> {
        self.put("/vsock", config).await
    }

    pub async fn start_microvm(&self) -> Result<()> {
        self.put("/actions", &Action { action_type: "InstanceStart".into() }).await
    }

    pub async fn pause(&self) -> Result<()> {
        self.patch_vm_state("Paused").await
    }

    pub async fn resume(&self) -> Result<()> {
        self.patch_vm_state("Resumed").await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.put("/actions", &Action { action_type: "SendCtrlAltDel".into() }).await
    }

    pub async fn load_snapshot(&self, req: &LoadSnapshotRequest) -> Result<()> {
        self.put("/snapshot/load", req).await
    }

    pub async fn create_snapshot(&self, req: &CreateSnapshotRequest) -> Result<()> {
        self.put("/snapshot/create", req).await
    }

    async fn patch_vm_state(&self, state: &str) -> Result<()> {
        #[derive(Serialize)]
        struct StateBody { state: String }
        self.patch("/vm", &StateBody { state: state.into() }).await
    }

    // ------------------------------------------------------------------
    // Low-level HTTP/1.1 over Unix socket
    // ------------------------------------------------------------------

    async fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        self.request("PUT", path, Some(body)).await
    }

    async fn patch<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        self.request("PATCH", path, Some(body)).await
    }

    async fn request<T: Serialize>(
        &self,
        method: &str,
        path: &str,
        body: Option<&T>,
    ) -> Result<()> {
        let body_json = body.map(|b| serde_json::to_vec(b)).transpose()?;
        let body_len = body_json.as_ref().map(|v| v.len()).unwrap_or(0);

        let req = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            method, path, body_len
        );

        let mut stream = UnixStream::connect(&self.socket_path).await?;
        stream.write_all(req.as_bytes()).await?;
        if let Some(body) = body_json {
            stream.write_all(&body).await?;
        }
        stream.flush().await?;

        // Read response.
        let mut buf = vec![0u8; 8192];
        let mut response = Vec::new();
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            // Simple heuristic: if we have \r\n\r\n and enough bytes for Content-Length.
            if let Some(header_end) = find_crlf2(&response) {
                let headers = std::str::from_utf8(&response[..header_end]).unwrap_or("");
                if let Some(cl) = parse_content_length(headers) {
                    if response.len() >= header_end + 4 + cl {
                        break;
                    }
                } else if response.len() >= header_end + 4 {
                    // No body expected (e.g., 204).
                    break;
                }
            }
        }

        let header_end = find_crlf2(&response).unwrap_or(0);
        let status_line = std::str::from_utf8(&response[..header_end.min(response.len())])
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");

        if !status_line.contains(" 2") && !status_line.contains(" 204") {
            let body_start = header_end + 4;
            let body = if body_start < response.len() {
                String::from_utf8_lossy(&response[body_start..]).to_string()
            } else {
                String::new()
            };
            return Err(CofferError::Firecracker(format!(
                "{} -> {}: {}",
                path, status_line, body
            )));
        }

        debug!(path, method, "Firecracker API call succeeded");
        Ok(())
    }
}

fn find_crlf2(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        if line.to_lowercase().starts_with("content-length:") {
            return line[15..].trim().parse().ok();
        }
    }
    None
}

// ------------------------------------------------------------------
// Firecracker API types
// ------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    pub smt: bool,
    pub track_dirty_pages: bool,
}

#[derive(Serialize, Debug)]
pub struct KernelConfig {
    pub kernel_image_path: PathBuf,
    pub boot_args: String,
}

#[derive(Serialize, Debug)]
pub struct DriveConfig {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Serialize, Debug)]
pub struct NetworkInterfaceConfig {
    pub iface_id: String,
    pub guest_mac: String,
    pub host_dev_name: String,
}

#[derive(Serialize, Debug)]
pub struct VsockConfig {
    pub guest_cid: u32,
    pub uds_path: PathBuf,
}

#[derive(Serialize, Debug)]
struct Action {
    action_type: String,
}

#[derive(Serialize, Debug)]
pub struct LoadSnapshotRequest {
    pub snapshot_path: PathBuf,
    pub mem_backend: MemBackendConfig,
    pub resume_vm: bool,
}

#[derive(Serialize, Debug)]
pub struct MemBackendConfig {
    pub backend_type: String,
    pub backend_path: PathBuf,
}

/// Deprecated: use `MemBackendConfig` directly.
pub type MemoryBackend = MemBackendConfig;

#[derive(Serialize, Debug)]
pub struct CreateSnapshotRequest {
    pub snapshot_type: String, // "Full" or "Diff"
    pub snapshot_path: PathBuf,
    pub mem_file_path: PathBuf,
}
