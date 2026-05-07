//! Coffer Guest Agent — runs inside the MicroVM.
//!
//! Listens on vsock port 1024 and handles host requests:
//! - Exec: run commands with stdout/stderr capture (batch or streaming)
//! - Upload: write files
//! - Download: read files
//! - Ping: health check

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── Global active PTY fd for ResizePty requests ──
static ACTIVE_PTY_FD: OnceLock<Mutex<Option<i32>>> = OnceLock::new();

fn get_active_pty_fd() -> &'static Mutex<Option<i32>> {
    ACTIVE_PTY_FD.get_or_init(|| Mutex::new(None))
}

struct ActivePtyGuard;

impl ActivePtyGuard {
    fn new(fd: i32) -> Self {
        if let Ok(mut active) = get_active_pty_fd().lock() {
            *active = Some(fd);
        }
        Self
    }
}

impl Drop for ActivePtyGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = get_active_pty_fd().lock() {
            *active = None;
        }
    }
}

use coffer_protocol::{AgentRequest, AgentResponse, ErrorCode, ExecEvent, ResponseBody, COFFER_VSOCK_PORT};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info, warn};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // If we are PID 1, fork a child to run the actual agent logic.
    // This avoids issues with signal handling and child reaping in PID 1 processes.
    if unsafe { libc::getpid() } == 1 {
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            eprintln!("fork failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        if pid > 0 {
            // Parent process (PID 1) waits for all children and reaps them.
            loop {
                let mut status: libc::c_int = 0;
                let result = unsafe { libc::waitpid(-1, &mut status, 0) };
                if result < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ECHILD) {
                        break;
                    }
                    eprintln!("waitpid failed: {}", err);
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        // Child process continues with the actual agent logic.
    }

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
                tty,
                window_size,
                ..
            } => {
                // Interactive exec takes over the whole connection; extract the
                // underlying stream so we can use a fresh BufReader.
                let inner = reader.into_inner();
                if let Err(e) = handle_interactive_exec(
                    request_id, cmd, env, working_dir, timeout_ms,
                    tty, window_size,
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
    tty: bool,
    window_size: Option<coffer_protocol::WindowSize>,
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

    let mut child: tokio::process::Child;
    let mut child_input: Option<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>;

    let (event_tx, event_rx_holder) = tokio::sync::mpsc::channel::<ExecEvent>(64);
    let mut event_rx_opt = Some(event_rx_holder);
    let emit_log = |_tx: &tokio::sync::mpsc::Sender<ExecEvent>, msg: &str| {
        info!("[agent] {}", msg);
    };

    if tty {
        // ── PTY mode: allocate a pseudo-terminal so the shell enters true REPL ──
        emit_log(&event_tx, "PTY mode: openpty");
        let pty = nix::pty::openpty(None, None)
            .map_err(|e| format!("openpty failed: {}", e))?;
        let slave_fd = pty.slave.as_raw_fd();
        emit_log(&event_tx, &format!("openpty ok: master_fd={}, slave_fd={}", pty.master.as_raw_fd(), slave_fd));

        if let Some(ws) = window_size {
            let winsize = libc::winsize {
                ws_row: ws.rows,
                ws_col: ws.cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::ioctl(pty.master.as_raw_fd(), libc::TIOCSWINSZ, &winsize);
            }
            emit_log(&event_tx, &format!("TIOCSWINSZ: {}x{}", ws.rows, ws.cols));
        }

        let _pty_guard = ActivePtyGuard::new(pty.master.as_raw_fd());

        let slave_stdin = unsafe { std::os::fd::OwnedFd::from_raw_fd(
            nix::unistd::dup(slave_fd)
                .map_err(|e| format!("dup slave stdin failed: {}", e))?
        ) };
        let slave_stdout = unsafe { std::os::fd::OwnedFd::from_raw_fd(
            nix::unistd::dup(slave_fd)
                .map_err(|e| format!("dup slave stdout failed: {}", e))?
        ) };
        let slave_stderr = pty.slave;

        command.stdin(std::process::Stdio::from(slave_stdin));
        command.stdout(std::process::Stdio::from(slave_stdout));
        command.stderr(std::process::Stdio::from(slave_stderr));

        // Make the child a session leader and acquire the PTY as the
        // controlling terminal so /bin/sh gets full job-control support.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut event_rx = event_rx_opt.take().unwrap();

        emit_log(&event_tx, &format!("spawning cmd={:?}", cmd));
        child = command.spawn()?;
        let child_id = child.id().unwrap_or(0);
        emit_log(&event_tx, &format!("spawned child pid={}", child_id));

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        match child.try_wait() {
            Ok(Some(status)) => {
                emit_log(&event_tx, &format!("WARNING: child exited immediately: {:?}", status));
            }
            Ok(None) => {
                emit_log(&event_tx, "child still running after 100ms");
            }
            Err(e) => {
                emit_log(&event_tx, &format!("try_wait failed: {}", e));
            }
        }

        let master_std = std::fs::File::from(pty.master);
        let master_std2 = master_std
            .try_clone()
            .map_err(|e| format!("clone pty master failed: {}", e))?;
        let master_read = tokio::fs::File::from_std(master_std);
        let master_write = tokio::fs::File::from_std(master_std2);
        child_input = Some(Box::new(master_write));

        info!(%child_id, cmd = ?cmd, "Spawned interactive child process with PTY");

        let stdout_tx = event_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut reader = BufReader::new(master_read);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        break;
                    }
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                        if stdout_tx.send(ExecEvent::Stdout { chunk }).await.is_err() {
                            break;
                        }
                    }
                    Err(_e) => {
                        break;
                    }
                }
            }
        });

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
                    emit_log(&event_tx, &format!("child.wait() returned: {}", code));
                    info!(%child_id, %code, "Child process exited");
                    break code;
                }
                result = stdin_reader.read(&mut stdin_buf) => {
                    match result {
                        Ok(0) => {
                            emit_log(&event_tx, "stdin_reader: EOF");
                            info!(%child_id, "VSock stdin EOF received, closing PTY");
                            drop(child_input.take());
                        }
                        Ok(n) => {
                            emit_log(&event_tx, &format!("stdin_reader: read {} bytes", n));
                            if let Some(ref mut child_stdin) = child_input {
                                child_stdin.write_all(&stdin_buf[..n]).await?;
                                child_stdin.flush().await?;
                            }
                        }
                        Err(e) => {
                            emit_log(&event_tx, &format!("stdin_reader: error {}", e));
                            drop(child_input.take());
                        }
                    }
                }
            }
        };

        emit_log(&event_tx, &format!("main loop break, exit_code={}", exit_code));
        let _ = stdout_task.await;

        while let Some(event) = event_rx.recv().await {
            let resp = AgentResponse::Event {
                request_id: request_id.clone(),
                event,
            };
            send_response(writer, &resp).await?;
        }

        let resp = AgentResponse::Event {
            request_id: request_id.clone(),
            event: ExecEvent::Exited { exit_code },
        };
        send_response(writer, &resp).await?;

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
        emit_log(&event_tx, "handle_interactive_exec done");
        return Ok(());
    }

    // ── Legacy pipe mode (no PTY) ──
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command.spawn()?;
    let child_id = child.id().unwrap_or(0);
    emit_log(&event_tx, &format!("pipe mode: spawned child pid={}", child_id));
    info!(%child_id, cmd = ?cmd, "Spawned interactive child process");

    if let Some(ref mut child_stdin) = child.stdin {
        let _ = child_stdin.write_all(b"\n").await;
        let _ = child_stdin.flush().await;
        emit_log(&event_tx, "wrote initial newline to child stdin");
        info!(%child_id, "Wrote initial newline to child stdin");
    }

    let mut event_rx = event_rx_opt.take().unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    match child.try_wait() {
        Ok(Some(status)) => {
            emit_log(&event_tx, &format!("WARNING: child exited immediately: {:?}", status));
            warn!(%child_id, ?status, "Child exited immediately after spawn!");
        }
        Ok(None) => {
            emit_log(&event_tx, "child still running after 100ms");
            info!(%child_id, "Child still running after 100ms");
        }
        Err(e) => {
            emit_log(&event_tx, &format!("try_wait failed: {}", e));
            warn!(%child_id, ?e, "Failed to check child status");
        }
    }

    child_input = Some(Box::new(child.stdin.take().unwrap()));
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    let stdout_tx = event_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    eprintln!("[agent] stdout_task: EOF");
                    break;
                }
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    eprintln!("[agent] stdout_task: read {} bytes", n);
                    if stdout_tx.send(ExecEvent::Stdout { chunk }).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("[agent] stdout_task: error {}", e);
                    break;
                }
            }
        }
    });

    let stderr_tx = event_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stderr);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => {
                    break;
                }
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    if stderr_tx.send(ExecEvent::Stderr { chunk }).await.is_err() {
                        break;
                    }
                }
                Err(_e) => {
                    break;
                }
            }
        }
    });

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
                        info!(%child_id, "VSock stdin EOF received, closing child stdin");
                        drop(child_input.take());
                    }
                    Ok(n) => {
                        if let Some(ref mut child_stdin) = child_input {
                            child_stdin.write_all(&stdin_buf[..n]).await?;
                            child_stdin.flush().await?;
                        }
                    }
                    Err(_e) => {
                        drop(child_input.take());
                    }
                }
            }
        }
    };
    emit_log(&event_tx, &format!("pipe mode main loop break, exit_code={}", exit_code));

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
            ..
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
        AgentRequest::ResizePty { cols, rows } => {
            let fd_opt = get_active_pty_fd().lock().unwrap();
            if let Some(fd) = *fd_opt {
                let winsize = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(fd, libc::TIOCSWINSZ, &winsize);
                }
                AgentResponse::Ok {
                    request_id: "resize".into(),
                    body: ResponseBody::Pong {
                        agent_version: AGENT_VERSION.into(),
                        uptime_secs: start_time.elapsed().as_secs(),
                    },
                }
            } else {
                AgentResponse::Error {
                    request_id: "resize".into(),
                    code: ErrorCode::InvalidRequest,
                    message: "No active PTY session".into(),
                }
            }
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
