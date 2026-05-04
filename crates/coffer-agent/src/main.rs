//! Coffer Guest Agent — runs inside the MicroVM.
//!
//! Listens on vsock port 1024 and handles host requests:
//! - Exec: run commands with stdout/stderr capture
//! - Upload: write files
//! - Download: read files
//! - Ping: health check

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use coffer_protocol::{AgentRequest, AgentResponse, ErrorCode, ResponseBody, COFFER_VSOCK_PORT};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info, warn};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    info!(version = AGENT_VERSION, port = COFFER_VSOCK_PORT, "Coffer agent starting");

    let start_time = Instant::now();

    // In a real Firecracker guest, vsock uses AF_VSOCK.
    // For development/testing, fall back to a Unix socket if vsock is unavailable.
    let mut listener = match tokio_vsock::VsockListener::bind(
        tokio_vsock::VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, COFFER_VSOCK_PORT),
    ) {
        Ok(l) => {
            info!("Listening on vsock port {}", COFFER_VSOCK_PORT);
            l
        }
        Err(e) => {
            warn!("vsock unavailable ({}), falling back to Unix socket", e);
            let socket_path = "/tmp/coffer-agent.sock";
            let _ = std::fs::remove_file(socket_path);
            let std_listener = std::os::unix::net::UnixListener::bind(socket_path)?;
            std_listener.set_nonblocking(true)?;
            return run_unix_listener(std_listener, start_time).await;
        }
    };

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!("Accepted connection from {:?}", addr);
                let start = start_time;
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, start).await {
                        error!("Connection error: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {}", e);
            }
        }
    }
}

async fn run_unix_listener(
    listener: std::os::unix::net::UnixListener,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let tokio_listener = tokio::net::UnixListener::from_std(listener)?;
    loop {
        match tokio_listener.accept().await {
            Ok((stream, _)) => {
                let start = start_time;
                tokio::spawn(async move {
                    if let Err(e) = handle_unix_connection(stream, start).await {
                        error!("Connection error: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept error: {}", e);
            }
        }
    }
}

async fn handle_connection(
    stream: tokio_vsock::VsockStream,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    // tokio-vsock stream implements AsyncRead + AsyncWrite
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let req: AgentRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = AgentResponse::Error {
                    request_id: "unknown".into(),
                    code: ErrorCode::InvalidRequest,
                    message: format!("JSON parse error: {}", e),
                };
                send_response(&mut write_half, &resp).await?;
                continue;
            }
        };

        let resp = process_request(req, start_time).await;
        send_response(&mut write_half, &resp).await?;
    }

    Ok(())
}

async fn handle_unix_connection(
    stream: tokio::net::UnixStream,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let req: AgentRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = AgentResponse::Error {
                    request_id: "unknown".into(),
                    code: ErrorCode::InvalidRequest,
                    message: format!("JSON parse error: {}", e),
                };
                send_response(&mut write_half, &resp).await?;
                continue;
            }
        };

        let resp = process_request(req, start_time).await;
        send_response(&mut write_half, &resp).await?;
    }

    Ok(())
}

async fn send_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &AgentResponse,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = serde_json::to_vec(resp)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

async fn process_request(req: AgentRequest, start_time: Instant) -> AgentResponse {
    match req {
        AgentRequest::Ping { request_id } => {
            AgentResponse::Ok {
                request_id,
                body: ResponseBody::Pong {
                    agent_version: AGENT_VERSION.into(),
                    uptime_secs: start_time.elapsed().as_secs(),
                },
            }
        }
        AgentRequest::Exec { request_id, cmd, env, working_dir, stdin, timeout_ms } => {
            exec_command(request_id, cmd, env, working_dir, stdin, timeout_ms).await
        }
        AgentRequest::Upload { request_id, guest_path, data, mode } => {
            upload_file(request_id, guest_path, data, mode).await
        }
        AgentRequest::Download { request_id, guest_path } => {
            download_file(request_id, guest_path).await
        }
    }
}

async fn exec_command(
    request_id: String,
    cmd: Vec<String>,
    env: HashMap<String, String>,
    working_dir: Option<String>,
    stdin_data: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
) -> AgentResponse {
    let start = Instant::now();

    if cmd.is_empty() {
        return AgentResponse::Error {
            request_id,
            code: ErrorCode::InvalidRequest,
            message: "Empty command".into(),
        };
    }

    let mut command = tokio::process::Command::new(&cmd[0]);
    if cmd.len() > 1 {
        command.args(&cmd[1..]);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }

    if let Some(data) = stdin_data {
        command.stdin(std::process::Stdio::piped());
        let mut child = match command.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn() {
            Ok(c) => c,
            Err(e) => return AgentResponse::Error {
                request_id,
                code: ErrorCode::ExecFailed,
                message: format!("Failed to spawn: {}", e),
            },
        };

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&data).await;
        }

        let timeout = timeout_ms.map(Duration::from_millis);
        match run_with_timeout(child, timeout).await {
            Ok((exit_code, stdout, stderr)) => AgentResponse::Ok {
                request_id,
                body: ResponseBody::Exec {
                    exit_code,
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    stderr: String::from_utf8_lossy(&stderr).to_string(),
                    duration_ms: start.elapsed().as_millis() as u64,
                },
            },
            Err(e) => AgentResponse::Error {
                request_id,
                code: ErrorCode::ExecFailed,
                message: e,
            },
        }
    } else {
        command.stdin(std::process::Stdio::null());
        let child = match command.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn() {
            Ok(c) => c,
            Err(e) => return AgentResponse::Error {
                request_id,
                code: ErrorCode::ExecFailed,
                message: format!("Failed to spawn: {}", e),
            },
        };

        let timeout = timeout_ms.map(Duration::from_millis);
        match run_with_timeout(child, timeout).await {
            Ok((exit_code, stdout, stderr)) => AgentResponse::Ok {
                request_id,
                body: ResponseBody::Exec {
                    exit_code,
                    stdout: String::from_utf8_lossy(&stdout).to_string(),
                    stderr: String::from_utf8_lossy(&stderr).to_string(),
                    duration_ms: start.elapsed().as_millis() as u64,
                },
            },
            Err(e) => AgentResponse::Error {
                request_id,
                code: ErrorCode::ExecFailed,
                message: e,
            },
        }
    }
}

async fn run_with_timeout(
    child: tokio::process::Child,
    timeout: Option<Duration>,
) -> Result<(i32, Vec<u8>, Vec<u8>), String> {


    let mut child_opt = Some(child);

    let fut = async {
        let c = child_opt.take().unwrap();
        let output = c.wait_with_output().await.map_err(|e| e.to_string())?;
        let code = output.status.code().unwrap_or(-1);
        Ok((code, output.stdout, output.stderr))
    };

    if let Some(t) = timeout {
        match tokio::time::timeout(t, fut).await {
            Ok(r) => r,
            Err(_) => {
                if let Some(mut c) = child_opt.take() {
                    let _ = c.start_kill();
                }
                Err("Execution timed out".into())
            }
        }
    } else {
        fut.await
    }
}

async fn upload_file(
    request_id: String,
    guest_path: String,
    data: Vec<u8>,
    mode: Option<u32>,
) -> AgentResponse {
    let path = std::path::Path::new(&guest_path);
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return AgentResponse::Error {
                request_id,
                code: ErrorCode::IoError,
                message: format!("Create dir failed: {}", e),
            };
        }
    }

    match tokio::fs::write(&guest_path, &data).await {
        Ok(_) => {
            if let Some(m) = mode {
                let _ = tokio::fs::set_permissions(&guest_path, std::fs::Permissions::from_mode(m)).await;
            }
            AgentResponse::Ok {
                request_id,
                body: ResponseBody::Upload { bytes_written: data.len() as u64 },
            }
        }
        Err(e) => AgentResponse::Error {
            request_id,
            code: ErrorCode::IoError,
            message: format!("Write failed: {}", e),
        },
    }
}

async fn download_file(request_id: String, guest_path: String) -> AgentResponse {
    match tokio::fs::read(&guest_path).await {
        Ok(data) => AgentResponse::Ok {
            request_id,
            body: ResponseBody::Download { data },
        },
        Err(e) => AgentResponse::Error {
            request_id,
            code: ErrorCode::IoError,
            message: format!("Read failed: {}", e),
        },
    }
}
