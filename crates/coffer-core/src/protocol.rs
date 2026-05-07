//! vsock protocol client for host-guest communication.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;

use coffer_protocol::{AgentRequest, AgentResponse, ExecEvent, ResponseBody};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{CofferError, Result};

pub struct VsockClient {
    stream: StdUnixStream,
    read_buf: Vec<u8>,
}

impl VsockClient {
    pub fn connect(vsock_uds_path: &Path, port: u32) -> Result<Self> {
        let mut stream = StdUnixStream::connect(vsock_uds_path)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
        write!(stream, "CONNECT {}\n", port)?;
        stream.flush()?;
        let mut ack = String::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte)?;
            if byte[0] == b'\n' { break; }
            ack.push(byte[0] as char);
        }
        if !ack.starts_with("OK ") {
            return Err(CofferError::AgentCommunication(format!(
                "Firecracker vsock handshake failed: {}", ack
            )));
        }
        Ok(Self { stream, read_buf: Vec::with_capacity(4096) })
    }

    pub fn call(&mut self, req: &AgentRequest) -> Result<AgentResponse> {
        let frame = coffer_protocol::encode_frame(req)?;
        self.stream.write_all(&frame)?;
        self.stream.flush()?;
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

    pub fn exec(
        &mut self,
        cmd: Vec<String>,
        env: HashMap<String, String>,
        working_dir: Option<String>,
        stdin: Option<Vec<u8>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput> {
        let req = AgentRequest::Exec {
            request_id: uuid::Uuid::new_v4().to_string(),
            cmd, env, working_dir, stdin, timeout_ms,
            interactive: false,
            tty: false,
            window_size: None,
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

pub async fn exec_interactive(
    vsock_uds_path: &Path,
    port: u32,
    cmd: Vec<String>,
    env: HashMap<String, String>,
    working_dir: Option<String>,
    tty: bool,
    window_size: Option<coffer_protocol::WindowSize>,
) -> Result<i32> {
    eprintln!("[host] exec_interactive: connecting to {:?}:{}", vsock_uds_path, port);
    let stream = tokio::net::UnixStream::connect(vsock_uds_path)
        .await
        .map_err(|e| CofferError::AgentCommunication(format!("Connect failed: {}", e)))?;
    eprintln!("[host] connected");

    let (mut read_stream, mut write_stream) = stream.into_split();

    write_stream.write_all(format!("CONNECT {}\n", port).as_bytes()).await?;
    write_stream.flush().await?;
    eprintln!("[host] sent CONNECT");

    let mut ack = String::new();
    let mut byte = [0u8; 1];
    loop {
        read_stream.read_exact(&mut byte).await?;
        if byte[0] == b'\n' { break; }
        ack.push(byte[0] as char);
    }
    eprintln!("[host] handshake: {}", ack.trim());
    if !ack.starts_with("OK ") {
        return Err(CofferError::AgentCommunication(format!(
            "Firecracker vsock handshake failed: {}", ack
        )));
    }

    let req_cmd = cmd.clone();
    let req = AgentRequest::Exec {
        request_id: uuid::Uuid::new_v4().to_string(),
        cmd, env, working_dir,
        stdin: None, timeout_ms: None,
        interactive: true,
        tty,
        window_size,
    };
    let frame = coffer_protocol::encode_frame(&req)?;
    write_stream.write_all(&frame).await?;
    write_stream.flush().await?;
    eprintln!("[host] sent exec request: cmd={:?}, tty={:?}", req_cmd, tty);

    let mut read_buf = Vec::with_capacity(4096);
    let mut temp_buf = [0u8; 4096];

    // Channel for forwarding stdin data from a blocking thread.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    // Spawn a blocking thread to read from stdin synchronously.
    let _stdin_thread = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdin_closed = false;
    loop {
        if let Some(pos) = read_buf.iter().position(|&b| b == b'\n') {
            let line = read_buf.drain(..=pos).collect::<Vec<_>>();
            let resp: AgentResponse = match serde_json::from_slice(&line) {
                Ok(r) => r,
                Err(e) => {
                    return Err(CofferError::AgentCommunication(format!(
                        "JSON parse error: {}", e
                    )));
                }
            };
            match resp {
                AgentResponse::Event { event: ExecEvent::Stdout { chunk }, .. } => {
                    eprintln!("[host] stdout: {:?}", &chunk[..chunk.len().min(80)]);
                    print!("{}", chunk);
                    let _ = std::io::stdout().flush();
                }
                AgentResponse::Event { event: ExecEvent::Stderr { chunk }, .. } => {
                    eprintln!("[host] stderr: {:?}", &chunk[..chunk.len().min(80)]);
                    eprint!("{}", chunk);
                    let _ = std::io::stderr().flush();
                }
                AgentResponse::Event { event: ExecEvent::Exited { exit_code }, .. } => {
                    eprintln!("[host] event: Exited {}", exit_code);
                }
                AgentResponse::Ok { body: ResponseBody::Exec { exit_code, .. }, .. } => {
                    eprintln!("[host] response: Ok, exit_code={}", exit_code);
                    return Ok(exit_code);
                }
                AgentResponse::Error { message, code, .. } => {
                    eprintln!("[host] response: Error {:?}: {}", code, message);
                    return Err(CofferError::AgentExec {
                        message: format!("{:?}: {}", code, message),
                        exit_code: None,
                    });
                }
                _ => {
                    eprintln!("[host] response: other");
                }
            }
            continue;
        }

        tokio::select! {
            result = read_stream.read(&mut temp_buf) => {
                match result {
                    Ok(0) => {
                        eprintln!("[host] read_stream EOF");
                        return Err(CofferError::AgentCommunication(
                            "Connection closed".into(),
                        ));
                    }
                    Ok(n) => {
                        read_buf.extend_from_slice(&temp_buf[..n]);
                    }
                    Err(e) => {
                        return Err(CofferError::AgentCommunication(format!(
                            "Read error: {}", e
                        )));
                    }
                }
            }
            data = stdin_rx.recv(), if !stdin_closed => {
                match data {
                    Some(bytes) => {
                        if write_stream.write_all(&bytes).await.is_err() {
                            return Err(CofferError::AgentCommunication(
                                "Failed to write stdin to vsock".into(),
                            ));
                        }
                        if write_stream.flush().await.is_err() {
                            return Err(CofferError::AgentCommunication(
                                "Failed to flush stdin to vsock".into(),
                            ));
                        }
                    }
                    None => {
                        // stdin thread exited (EOF on host stdin).
                        // Do NOT call shutdown() — Firecracker vsock does not
                        // support half-close and will tear down the whole
                        // connection, preventing us from reading the response.
                        stdin_closed = true;
                    }
                }
            }
        }
    }
}
