//! TUI terminal emulator for coffer shell using ratatui + vt100.
//!
//! Note: This module is fully implemented but not yet wired into the CLI.
//! The `#[allow(dead_code)]` attribute suppresses warnings while the feature
//! is pending integration.
#![allow(dead_code)]

//! Architecture:
//!   - Blocking thread reads raw bytes from stdin and forwards to vsock.
//!   - Async task manages the vsock connection (read guest output, write host input).
//!   - Blocking task (spawn_blocking) runs the ratatui event loop, rendering the vt100 screen
//!     and a status bar at the bottom.
//!   - Resize events are forwarded to the guest via a separate short-lived vsock connection.

use std::io;
use std::path::Path;
use std::time::Instant;

use crossterm::event::{self, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

// ===================================================================
// vt100 → ratatui colour mapping
// ===================================================================

fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

// ===================================================================
// Terminal widget: renders a vt100::Parser screen into ratatui
// ===================================================================

struct TerminalWidget<'a> {
    parser: &'a vt100::Parser,
}

impl<'a> Widget for TerminalWidget<'a> {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let screen = self.parser.screen();
        let (screen_rows, screen_cols) = screen.size();
        let rows = (screen_rows as u16).min(area.height);
        let cols = (screen_cols as u16).min(area.width);

        for row in 0..rows {
            for col in 0..cols {
                let x = area.x + col;
                let y = area.y + row;
                if x >= buf.area.right() || y >= buf.area.bottom() {
                    continue;
                }

                if let Some(cell) = screen.cell(row, col) {
                    let mut modifiers = Modifier::empty();
                    if cell.bold() {
                        modifiers |= Modifier::BOLD;
                    }
                    if cell.italic() {
                        modifiers |= Modifier::ITALIC;
                    }
                    if cell.underline() {
                        modifiers |= Modifier::UNDERLINED;
                    }
                    if cell.inverse() {
                        modifiers |= Modifier::REVERSED;
                    }

                    let style = Style::default()
                        .fg(map_color(cell.fgcolor()))
                        .bg(map_color(cell.bgcolor()))
                        .add_modifier(modifiers);

                    let symbol = if cell.contents().is_empty() {
                        " "
                    } else {
                        cell.contents()
                    };
                    buf[(x, y)].set_symbol(symbol).set_style(style);
                }
            }
        }

        // Highlight cursor by swapping fg/bg
        let (cursor_row, cursor_col) = screen.cursor_position();
        if cursor_row < rows && cursor_col < cols {
            let cx = area.x + cursor_col;
            let cy = area.y + cursor_row;
            if cx < buf.area.right() && cy < buf.area.bottom() {
                let cell = &buf[(cx, cy)];
                let current = cell.style();
                let inverted = Style::default()
                    .fg(current.bg.unwrap_or(Color::Reset))
                    .bg(current.fg.unwrap_or(Color::Reset))
                    .add_modifier(current.add_modifier);
                buf[(cx, cy)].set_style(inverted);
            }
        }
    }
}

// ===================================================================
// App state
// ===================================================================

struct AppState {
    parser: vt100::Parser,
    vm_id: String,
    template_id: String,
    shell: String,
    start_time: Instant,
    status: String,
    exit_code: Option<i32>,
}

impl AppState {
    fn new(rows: u16, cols: u16, vm_id: String, template_id: String, shell: String) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, rows as usize * 10),
            vm_id,
            template_id,
            shell,
            start_time: Instant::now(),
            status: "Connected".into(),
            exit_code: None,
        }
    }

    fn update_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }
}

// ===================================================================
// Status bar
// ===================================================================

fn build_status_bar(app: &AppState, width: u16) -> Paragraph<'static> {
    let elapsed = app.start_time.elapsed();
    let duration = format!(
        "{:02}:{:02}:{:02}",
        elapsed.as_secs() / 3600,
        (elapsed.as_secs() % 3600) / 60,
        elapsed.as_secs() % 60
    );

    let left = format!(" {} | {} | Shell: {} ", app.vm_id, app.template_id, app.shell);
    let right = format!(" {} | {} ", app.status, duration);

    let left_len = left.chars().count();
    let right_len = right.chars().count();
    let total = left_len + right_len;
    let pad = (width as usize).saturating_sub(total);

    let text = format!("{}{:>pad$}", left, right, pad = pad + right_len);
    let truncated: String = text.chars().take(width as usize).collect();

    Paragraph::new(Line::from(truncated))
        .style(Style::default().bg(Color::Blue).fg(Color::White))
}

// ===================================================================
// Render full UI
// ===================================================================

fn render_ui(f: &mut ratatui::Frame, app: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    // Main terminal area
    let terminal_widget = TerminalWidget { parser: &app.parser };
    f.render_widget(terminal_widget, chunks[0]);

    // Status bar
    let status = build_status_bar(app, chunks[1].width);
    f.render_widget(status, chunks[1]);
}

// ===================================================================
// Key event → ANSI bytes
// ===================================================================

fn key_event_to_bytes(key: event::KeyEvent) -> Vec<u8> {
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

// ===================================================================
// Vsock helpers
// ===================================================================

async fn send_resize_pty(
    vsock_uds_path: &Path,
    port: u32,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::connect(vsock_uds_path).await?;
    let (mut read_stream, mut write_stream) = stream.into_split();

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
        anyhow::bail!("resize handshake failed: {}", ack);
    }

    let req = coffer_protocol::AgentRequest::ResizePty { cols, rows };
    let frame = coffer_protocol::encode_frame(&req)?;
    write_stream.write_all(&frame).await?;
    write_stream.flush().await?;

    // Drain any response (ignore)
    let mut drain = [0u8; 256];
    let _ = tokio::time::timeout(tokio::time::Duration::from_millis(500), read_stream.read(&mut drain)).await;

    Ok(())
}

#[derive(Debug)]
enum VsockMessage {
    Output(Vec<u8>),
    Exited(i32),
}

async fn vsock_io_task(
    vsock_uds_path: std::path::PathBuf,
    port: u32,
    cmd: Vec<String>,
    env: std::collections::HashMap<String, String>,
    working_dir: Option<String>,
    window_size: Option<coffer_protocol::WindowSize>,
    mut stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    output_tx: std::sync::mpsc::Sender<VsockMessage>,
    mut resize_rx: tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>,
) -> anyhow::Result<i32> {
    eprintln!("[vsock] connecting to {:?}", vsock_uds_path);
    let stream = tokio::net::UnixStream::connect(&vsock_uds_path)
        .await
        .map_err(|e| anyhow::anyhow!("vsock connect failed: {}", e))?;
    let (mut read_stream, mut write_stream) = stream.into_split();
    eprintln!("[vsock] connected");

    // Firecracker vsock handshake
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
        anyhow::bail!("vsock handshake failed: {}", ack);
    }
    eprintln!("[vsock] handshake ok: {}", ack.trim());

    // Send Exec request
    let req_cmd = cmd.clone();
    let req = coffer_protocol::AgentRequest::Exec {
        request_id: uuid::Uuid::new_v4().to_string(),
        cmd,
        env,
        working_dir,
        stdin: None,
        timeout_ms: None,
        interactive: true,
        tty: false,
        window_size,
    };
    let frame = coffer_protocol::encode_frame(&req)?;
    write_stream.write_all(&frame).await?;
    write_stream.flush().await?;
    eprintln!("[vsock] sent exec request: cmd={:?}, tty=false", req_cmd);

    // Spawn stdout reader
    let mut reader_handle = {
        let output_tx = output_tx.clone();
        tokio::spawn(async move {
            let mut read_buf = Vec::with_capacity(4096);
            let mut temp_buf = [0u8; 4096];
            loop {
                match read_stream.read(&mut temp_buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        read_buf.extend_from_slice(&temp_buf[..n]);
                        while let Some(pos) = read_buf.iter().position(|&b| b == b'\n') {
                            let line = read_buf.drain(..=pos).collect::<Vec<_>>();
                            let resp: coffer_protocol::AgentResponse = match serde_json::from_slice(&line) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };
                            match resp {
                                coffer_protocol::AgentResponse::Event {
                                    event: coffer_protocol::ExecEvent::Stdout { chunk },
                                    ..
                                } => {
                                    eprintln!("[vsock] stdout: {:?}", &chunk[..chunk.len().min(100)]);
                                    if output_tx.send(VsockMessage::Output(chunk.into_bytes())).is_err() {
                                        break;
                                    }
                                }
                                coffer_protocol::AgentResponse::Event {
                                    event: coffer_protocol::ExecEvent::Stderr { chunk },
                                    ..
                                } => {
                                    eprintln!("[vsock] stderr: {:?}", &chunk[..chunk.len().min(100)]);
                                    if output_tx.send(VsockMessage::Output(chunk.into_bytes())).is_err() {
                                        break;
                                    }
                                }
                                coffer_protocol::AgentResponse::Event {
                                    event: coffer_protocol::ExecEvent::Exited { exit_code },
                                    ..
                                } => {
                                    eprintln!("[vsock] exited: {}", exit_code);
                                    let _ = output_tx.send(VsockMessage::Exited(exit_code));
                                    return Some(exit_code);
                                }
                                coffer_protocol::AgentResponse::Ok {
                                    body: coffer_protocol::ResponseBody::Exec { exit_code, .. },
                                    ..
                                } => {
                                    eprintln!("[vsock] ok exec: exit_code={}", exit_code);
                                    let _ = output_tx.send(VsockMessage::Exited(exit_code));
                                    return Some(exit_code);
                                }
                                coffer_protocol::AgentResponse::Error { message, .. } => {
                                    eprintln!("[vsock] error: {}", message);
                                    let _ = output_tx.send(VsockMessage::Output(
                                        format!("Agent error: {}\r\n", message).into_bytes(),
                                    ));
                                    let _ = output_tx.send(VsockMessage::Exited(-1));
                                    return Some(-1);
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        let _ = output_tx.send(VsockMessage::Output(
                            format!("Vsock read error: {}\r\n", e).into_bytes(),
                        ));
                        break;
                    }
                }
            }
            None
        })
    };

    let mut stdin_closed = false;
    let mut reader_done = false;
    let mut exit_code = None;

    eprintln!("[vsock] entering io loop");
    loop {
        tokio::select! {
            data = stdin_rx.recv(), if !stdin_closed => {
                match data {
                    Some(bytes) => {
                        if write_stream.write_all(&bytes).await.is_err() { break; }
                        if write_stream.flush().await.is_err() { break; }
                    }
                    None => {
                        stdin_closed = true;
                    }
                }
            }
            size = resize_rx.recv() => {
                if let Some((cols, rows)) = size {
                    let _ = send_resize_pty(&vsock_uds_path, port, cols, rows).await;
                }
            }
            result = &mut reader_handle, if !reader_done => {
                reader_done = true;
                exit_code = result.unwrap_or(None);
            }
        }
        if reader_done {
            eprintln!("[vsock] reader done, breaking io loop");
            break;
        }
    }

    // Allow output channel to drain briefly before dropping
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    eprintln!("[vsock] returning exit_code={:?}", exit_code);
    Ok(exit_code.unwrap_or(-1))
}

// ===================================================================
// Main entry point
// ===================================================================

pub async fn run_tui_shell(
    vsock_uds_path: std::path::PathBuf,
    port: u32,
    cmd: Vec<String>,
    env: std::collections::HashMap<String, String>,
    working_dir: Option<String>,
    window_size: Option<coffer_protocol::WindowSize>,
    vm_id: String,
    template_id: String,
    shell: String,
) -> anyhow::Result<i32> {
    // Channels
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (output_tx, output_rx) = std::sync::mpsc::channel::<VsockMessage>();
    let (resize_tx, resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();

    // Vsock async task
    let vsock_handle = tokio::spawn(vsock_io_task(
        vsock_uds_path,
        port,
        cmd,
        env,
        working_dir,
        window_size,
        stdin_rx,
        output_tx.clone(),
        resize_rx,
    ));

    // TUI blocking task
    let tui_handle = tokio::task::spawn_blocking(move || -> anyhow::Result<i32> {
        enable_raw_mode().map_err(|e| anyhow::anyhow!("enable_raw_mode: {}", e))?;
        let mut stdout = io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
        )
        .map_err(|e| anyhow::anyhow!("enter alternate screen: {}", e))?;

        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = ratatui::Terminal::new(backend)
            .map_err(|e| anyhow::anyhow!("create terminal: {}", e))?;

        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let mut app = AppState::new(rows.saturating_sub(1), cols, vm_id, template_id, shell);

        loop {
            // Poll output from vsock
            match output_rx.try_recv() {
                Ok(VsockMessage::Output(bytes)) => {
                    app.parser.process(&bytes);
                }
                Ok(VsockMessage::Exited(code)) => {
                    eprintln!("[tui] received Exited({})", code);
                    app.exit_code = Some(code);
                    app.status = format!("Exited ({})", code);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[tui] output disconnected, exit_code={:?}", app.exit_code);
                    if app.exit_code.is_none() {
                        app.status = "Disconnected".into();
                    }
                    break;
                }
            }

            // Poll crossterm events
            if event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
                match event::read() {
                    Ok(event::Event::Resize(cols, rows)) => {
                        app.update_size(rows.saturating_sub(1), cols);
                        terminal
                            .resize(ratatui::layout::Rect::new(0, 0, cols, rows))
                            .ok();
                        let _ = resize_tx.send((cols, rows.saturating_sub(1)));
                    }
                    Ok(event::Event::Key(key)) => {
                        let bytes = key_event_to_bytes(key);
                        if !bytes.is_empty() {
                            let _ = stdin_tx.send(bytes);
                        }
                    }
                    _ => {}
                }
            }

            // Render
            terminal.draw(|f| render_ui(f, &app)).ok();

            if app.exit_code.is_some() && output_rx.try_recv().is_err() {
                // All output drained and session ended
                eprintln!("[tui] all output drained, breaking");
                std::thread::sleep(std::time::Duration::from_millis(300));
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(16));
        }

        // Cleanup
        disable_raw_mode().ok();
        let _ = crossterm::execute!(
            terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
        );

        eprintln!("[tui] loop break, returning {:?}", app.exit_code);
        Ok(app.exit_code.unwrap_or(-1))
    });

    // Wait for both tasks
    let (vsock_result, tui_result) = tokio::join!(vsock_handle, tui_handle);

    let vsock_code = vsock_result.map_err(|e| anyhow::anyhow!("vsock task panicked: {}", e))??;
    let tui_code = tui_result.map_err(|e| anyhow::anyhow!("TUI task panicked: {}", e))??;

    eprintln!("[tui] run_tui_shell done: tui_code={}, vsock_code={}", tui_code, vsock_code);
    Ok(tui_code)
}
