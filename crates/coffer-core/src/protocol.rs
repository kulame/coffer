//! vsock protocol client for host-guest communication.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;

use coffer_protocol::{AgentRequest, AgentResponse, ResponseBody};

use crate::error::{CofferError, Result};

/// A synchronous vsock client (used within async tasks).
///
/// Firecracker vsock uses a Unix domain socket on the host side.
/// The socket path is `vsock_uds_path` configured in the VM.
pub struct VsockClient {
    stream: StdUnixStream,
    read_buf: Vec<u8>,
}

impl VsockClient {
    /// Connect to the guest agent via the host-side vsock Unix socket.
    pub fn connect(vsock_uds_path: &Path) -> Result<Self> {
        let stream = StdUnixStream::connect(vsock_uds_path)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
        Ok(Self {
            stream,
            read_buf: Vec::with_capacity(4096),
        })
    }

    /// Send a request and wait for the response.
    pub fn call(&mut self, req: &AgentRequest) -> Result<AgentResponse> {
        let frame = coffer_protocol::encode_frame(req)?;
        self.stream.write_all(&frame)?;
        self.stream.flush()?;

        // Read until we get a complete JSON line.
        let mut temp_buf = [0u8; 4096];
        loop {
            if let Some(pos) = self.read_buf.iter().position(|&b| b == b'\n') {
                let line = self.read_buf.drain(..=pos).collect::<Vec<_>>();
                let resp: AgentResponse = serde_json::from_slice(&line)?;
                return Ok(resp);
            }
            let n = self.stream.read(&mut temp_buf)?;
            if n == 0 {
                return Err(CofferError::AgentCommunication(
                    "vsock closed before response".into(),
                ));
            }
            self.read_buf.extend_from_slice(&temp_buf[..n]);
        }
    }

    /// Execute a command and return output.
    pub fn exec(
        &mut self,
        cmd: Vec<String>,
        env: std::collections::HashMap<String, String>,
        working_dir: Option<String>,
        stdin: Option<Vec<u8>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput> {
        let req = AgentRequest::Exec {
            request_id: uuid::Uuid::new_v4().to_string(),
            cmd,
            env,
            working_dir,
            stdin,
            timeout_ms,
        };

        match self.call(&req)? {
            AgentResponse::Ok { body: ResponseBody::Exec { exit_code, stdout, stderr, duration_ms }, .. } => {
                Ok(ExecOutput { exit_code, stdout, stderr, duration_ms })
            }
            AgentResponse::Error { message, code, .. } => {
                Err(CofferError::AgentExec {
                    message: format!("{:?}: {}", code, message),
                    exit_code: None,
                })
            }
            _ => Err(CofferError::AgentCommunication("Unexpected response type".into())),
        }
    }

    /// Health check.
    pub fn ping(&mut self) -> Result<(String, u64)> {
        let req = AgentRequest::Ping {
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        match self.call(&req)? {
            AgentResponse::Ok { body: ResponseBody::Pong { agent_version, uptime_secs }, .. } => {
                Ok((agent_version, uptime_secs))
            }
            _ => Err(CofferError::AgentCommunication("Unexpected ping response".into())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}
