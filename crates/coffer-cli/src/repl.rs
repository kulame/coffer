//! Interactive REPL client for coffer shell.
//!
//! Provides a bash-like interactive shell experience over vsock:
//! - Puts the host terminal in raw mode so every keystroke is forwarded
//!   immediately to the guest PTY.
//! - Converts crossterm key events into ANSI escape sequences.
//! - Streams stdout/stderr from the guest directly to the host terminal.
//! - Forwards terminal resize events to the guest agent.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use coffer_protocol::{AgentRequest, AgentResponse, ExecEvent, ResponseBody, WindowSize};

/// Guard that ensures raw mode is disabled when dropped.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Run an interactive shell REPL over vsock.
///
/// The host terminal is switched to raw mode. Keystrokes are forwarded to the
/// guest PTY in real time, and guest output is printed directly to the host
/// terminal. The loop continues until the shell inside the guest exits.
pub async fn run_shell_repl(
    vsock_path: &Path,
    port: u32,
    shell: &str,
    env: HashMap<String, String>,
    working_dir: Option<String>,
) -> anyhow::Result<i32> {
    let stream = UnixStream::connect(vsock_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to vsock: {}", e))?;
    let (mut read_stream, mut write_stream) = stream.into_split();

    // ── Firecracker vsock handshake ──
    write_stream
        .write_all(format!("CONNECT {}\n", port).as_bytes())
        .await?;
    write_stream.flush().await?;

    let mut ack = String::new();
    let mut byte = [0u8; 1];
    loop {
        read_stream.read_exact(&mut byte).await?;
        if byte[0] == b'\n' {
            break;
        }
        ack.push(byte[0] as char);
    }
    if !ack.starts_with("OK ") {
        anyhow::bail!("Firecracker vsock handshake failed: {}", ack);
    }

    // ── Send exec request for interactive shell with PTY ──
    let window_size = crossterm::terminal::size()
        .ok()
        .map(|(cols, rows)| WindowSize { rows, cols });

    let req = AgentRequest::Exec {
        request_id: uuid::Uuid::new_v4().to_string(),
        cmd: vec![shell.into()],
        env,
        working_dir,
        stdin: None,
        timeout_ms: None,
        interactive: true,
        tty: true,
        window_size,
    };
    let frame = coffer_protocol::encode_frame(&req)?;
    write_stream.write_all(&frame).await?;
    write_stream.flush().await?;

    // ── Enable raw mode ──
    crossterm::terminal::enable_raw_mode()
        .map_err(|e| anyhow::anyhow!("Failed to enable raw mode: {}", e))?;
    let _raw_guard = RawModeGuard;

    eprint!("\r\n=== Coffer Shell ===\r\nType 'exit' or press Ctrl+] to quit\r\n\r\n");

    // ── Channels for cross-thread communication ──
    let (_stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (_resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
    let (_quit_tx, mut quit_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // ── Spawn blocking thread for terminal input ──
    let _input_thread = std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event::Event::Key(key)) => {
                    // Ctrl+] to detach from the REPL immediately.
                    if key.code == KeyCode::Char(']')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let _ = _quit_tx.send(());
                        break;
                    }
                    let bytes = key_event_to_bytes(key);
                    if _stdin_tx.send(bytes).is_err() {
                        break;
                    }
                }
                Ok(event::Event::Resize(cols, rows)) => {
                    let _ = _resize_tx.send((cols, rows));
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    let mut read_buf = Vec::with_capacity(4096);
    let mut temp_buf = [0u8; 4096];
    let exit_code = -1;

    let result = 'repl: loop {
        tokio::select! {
            _ = quit_rx.recv() => {
                eprint!("\r\n[detached from coffer shell]\r\n");
                break 'repl Ok(exit_code);
            }
            result = read_stream.read(&mut temp_buf) => {
                match result {
                    Ok(0) => break 'repl Ok(exit_code),
                    Ok(n) => {
                        read_buf.extend_from_slice(&temp_buf[..n]);
                        while let Some(pos) = read_buf.iter().position(|&b| b == b'\n') {
                            let line = read_buf.drain(..=pos).collect::<Vec<_>>();
                            let resp: AgentResponse = match serde_json::from_slice(&line) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };
                            match resp {
                                AgentResponse::Event {
                                    event: ExecEvent::Stdout { chunk }, ..
                                } => {
                                    print!("{}", chunk);
                                    let _ = std::io::stdout().flush();
                                }
                                AgentResponse::Event {
                                    event: ExecEvent::Stderr { chunk }, ..
                                } => {
                                    eprint!("{}", chunk);
                                    let _ = std::io::stderr().flush();
                                }
                                AgentResponse::Event {
                                    event: ExecEvent::Exited { exit_code: code }, ..
                                } => {
                                    break 'repl Ok(code);
                                }
                                AgentResponse::Ok {
                                    body: ResponseBody::Exec { exit_code: code, .. },
                                    ..
                                } => {
                                    break 'repl Ok(code);
                                }
                                AgentResponse::Error { message, .. } => {
                                    break 'repl Err(anyhow::anyhow!("Agent error: {}", message));
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => break 'repl Err(anyhow::anyhow!("Read error: {}", e)),
                }
            }
            data = stdin_rx.recv() => {
                match data {
                    Some(bytes) => {
                        if write_stream.write_all(&bytes).await.is_err() {
                            break Ok(exit_code);
                        }
                        if write_stream.flush().await.is_err() {
                            break Ok(exit_code);
                        }
                    }
                    None => break Ok(exit_code),
                }
            }
            size = resize_rx.recv() => {
                if let Some((cols, rows)) = size {
                    let req = AgentRequest::ResizePty { cols, rows };
                    if let Ok(frame) = coffer_protocol::encode_frame(&req) {
                        let _ = write_stream.write_all(&frame).await;
                        let _ = write_stream.flush().await;
                    }
                }
            }
        }
    };

    result
}

/// Convert a crossterm key event into ANSI escape sequence bytes.
fn key_event_to_bytes(key: KeyEvent) -> Vec<u8> {
    let mut bytes = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let b = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    'A'..='Z' => c as u8 - b'A' + 1,
                    ' ' => 0,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' => 0x1f,
                    '?' => 0x7f,
                    '2' => 0,
                    '3'..='7' => c as u8 - b'0' + 0x1f,
                    '8' => 0x7f,
                    _ => c as u8,
                };
                bytes.push(b);
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
                bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            } else {
                bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(1) => bytes.extend_from_slice(b"\x1bOP"),
        KeyCode::F(2) => bytes.extend_from_slice(b"\x1bOQ"),
        KeyCode::F(3) => bytes.extend_from_slice(b"\x1bOR"),
        KeyCode::F(4) => bytes.extend_from_slice(b"\x1bOS"),
        KeyCode::F(n) if n >= 5 && n <= 12 => {
            bytes.extend_from_slice(format!("\x1b[{}~", n + 10).as_bytes());
        }
        _ => {}
    }
    bytes
}
