//! Coffer Guest Agent — runs inside the MicroVM.
//!
//! Listens on vsock port 1024 and handles host requests:
//! - Exec: run commands with stdout/stderr capture (batch or streaming)
//! - Upload: write files
//! - Download: read files
//! - Ping: health check

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use coffer_protocol::{AgentRequest, AgentResponse, ErrorCode, ExecEvent, ResponseBody, COFFER_VSOCK_PORT};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info, warn};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, write_half) = tokio::io::split(stream);
    handle_stream(BufReader::new(read_half), write_half, start_time).await
}

async fn handle_unix_connection(
    stream: tokio::net::UnixStream,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, write_half) = tokio::io::split(stream);
    handle_stream(BufReader::new(read_half), write_half, start_time).await
}

async fn handle_stream<R, W>(
    mut reader: BufReader<R>,
    mut writer: W,
    start_time: Instant,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
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
                send_response(&mut writer, &resp).await?;
                continue;
            }
        };

        match req {
            AgentRequest::Exec {
                interactive: true,
                request_id,
                cmd,
                env,
                working_dir,
                timeout_ms,
                ..
            } => {
                // Interactive exec takes over the whole connection; extract the
                // underlying stream so we can use a fresh BufReader.
                let inner = reader.into_inner();
                if let Err(e) = handle_interactive_exec(
                    request_id, cmd, env, working_dir, timeout_ms,
                    inner, &mut writer,
                ).await {
                    let resp = AgentResponse::Error {
                        request_id: "unknown".into(),
                        code: ErrorCode::InternalError,
                        message: format!("Interactive exec failed: {}", e),
                    };
                    send_response(&mut writer, &resp).await?;
                }
                // After interactive exec the connection is typically closed by
                // the host; stop reading further requests on this stream.
                break;
            }
            _ => {
                let resp = process_request(req, start_time).await;
                send_response(&mut writer, &resp).await?;
            }
        }
    }

    Ok(())
}

async fn handle_interactive_exec<R, W>(
    request_id: String,
    cmd: Vec<String>,
    env: HashMap<String, String>,
    working_dir: Option<String>,
    _timeout_ms: Option<u64>,
    reader: R,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    if cmd.is_empty() {
        let resp = AgentResponse::Error {
            request_id,
            code: ErrorCode::InvalidRequest,
            message: "Empty command".into(),
        };
        send_response(writer, &resp).await?;
        return Ok(());
    }

    let mut command = tokio::process::Command::new(&cmd[0]);
    if cmd.len() > 1 {
        command.args(&cmd[1..]);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    command.current_dir(working_dir.unwrap_or_else(|| "/".into()));
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command.spawn()?;
    let child_id = child.id().unwrap_or(0);
    info!(%child_id, cmd = ?cmd, "Spawned interactive child process");

    // Debug: immediately write a newline to child's stdin and check if child stays alive.
    if let Some(ref mut child_stdin) = child.stdin {
        let _ = child_stdin.write_all(b"\n").await;
        let _ = child_stdin.flush().await;
        info!(%child_id, "Wrote initial newline to child stdin");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    match child.try_wait() {
        Ok(Some(status)) => {
            warn!(%child_id, ?status, "Child exited immediately after spawn!");
        }
        Ok(None) => {
            info!(%child_id, "Child still running after 100ms");
        }
        Err(e) => {
            warn!(%child_id, ?e, "Failed to check child status");
        }
    }

    let mut child_stdin_opt = Some(child.stdin.take().unwrap());
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ExecEvent>(64);

    // stdout reader task
    let stdout_tx = event_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    if stdout_tx.send(ExecEvent::Stdout { chunk }).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // stderr reader task
    let stderr_tx = event_tx;
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stderr);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    if stderr_tx.send(ExecEvent::Stderr { chunk }).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Use a fresh BufReader for stdin forwarding so that any leftover state
    // from the JSON request parsing does not affect stdin reads.
    let mut stdin_reader = BufReader::new(reader);
    let mut stdin_buf = [0u8; 4096];

    let mut child_fut = std::pin::pin!(child.wait());

    let exit_code = loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                let resp = AgentResponse::Event {
                    request_id: request_id.clone(),
                    event,
                };
                send_response(writer, &resp).await?;
            }
            result = &mut child_fut => {
                let code = result?.code().unwrap_or(-1);
                info!(%child_id, %code, "Child process exited");
                break code;
            }
            result = stdin_reader.read(&mut stdin_buf) => {
                match result {
                    Ok(0) => {
                        // EOF on connection — close child's stdin
                        info!(%child_id, "VSock stdin EOF received, closing child stdin");
                        drop(child_stdin_opt.take());
                    }
                    Ok(n) => {
                        if let Some(ref mut child_stdin) = child_stdin_opt {
                            child_stdin.write_all(&stdin_buf[..n]).await?;
                            child_stdin.flush().await?;
                        }
                    }
                    Err(_) => {
                        drop(child_stdin_opt.take());
                    }
                }
            }
        }
    };

    // Wait for stdout/stderr tasks to finish draining
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    // Forward any remaining buffered events
    while let Some(event) = event_rx.recv().await {
        let resp = AgentResponse::Event {
            request_id: request_id.clone(),
            event,
        };
        send_response(writer, &resp).await?;
    }

    // Notify host that child exited
    let resp = AgentResponse::Event {
        request_id: request_id.clone(),
        event: ExecEvent::Exited { exit_code },
    };
    send_response(writer, &resp).await?;

    // Final response
    let resp = AgentResponse::Ok {
        request_id: request_id.clone(),
        body: ResponseBody::Exec {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
        },
    };
    send_response(writer, &resp).await?;

    Ok(())
}

async fn send_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &AgentResponse,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        AgentRequest::Exec {
            request_id,
            cmd,
            env,
            working_dir,
            stdin,
            timeout_ms,
            interactive: false,
        } => {
            exec_command(request_id, cmd, env, working_dir, stdin, timeout_ms).await
        }
        AgentRequest::Exec { interactive: true, request_id, .. } => {
            AgentResponse::Error {
                request_id,
                code: ErrorCode::InternalError,
                message: "Interactive exec should be handled by handle_stream".into(),
            }
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
