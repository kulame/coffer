//! coffer-protocol — vsock JSON Lines protocol for host-guest communication.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const COFFER_VSOCK_PORT: u32 = 1024;
pub const COFFER_AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ===================================================================
// Terminal / TTY helpers
// ===================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

// ===================================================================
// Requests (Host → Guest)
// ===================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AgentRequest {
    Exec {
        request_id: String,
        cmd: Vec<String>,
        env: HashMap<String, String>,
        working_dir: Option<String>,
        stdin: Option<Vec<u8>>,
        timeout_ms: Option<u64>,
        #[serde(default)]
        interactive: bool,
        #[serde(default)]
        tty: bool,
        window_size: Option<WindowSize>,
    },
    Upload {
        request_id: String,
        guest_path: String,
        data: Vec<u8>,
        mode: Option<u32>,
    },
    Download {
        request_id: String,
        guest_path: String,
    },
    Ping {
        request_id: String,
    },
    ResizePty {
        cols: u16,
        rows: u16,
    },
}

// ===================================================================
// Responses (Guest → Host)
// ===================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentResponse {
    Ok {
        request_id: String,
        #[serde(flatten)]
        body: ResponseBody,
    },
    Error {
        request_id: String,
        code: ErrorCode,
        message: String,
    },
    Event {
        request_id: String,
        #[serde(flatten)]
        event: ExecEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseBody {
    Exec {
        exit_code: i32,
        stdout: String,
        stderr: String,
        duration_ms: u64,
    },
    Upload {
        bytes_written: u64,
    },
    Download {
        data: Vec<u8>,
    },
    Pong {
        agent_version: String,
        uptime_secs: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecEvent {
    Stdout { chunk: String },
    Stderr { chunk: String },
    Exited { exit_code: i32 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    ExecFailed,
    FileNotFound,
    PermissionDenied,
    Timeout,
    IoError,
    InternalError,
}

// ===================================================================
// Framing: JSON Lines
// ===================================================================

/// Encode a message into a JSON Lines frame (with trailing \n).
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode a single JSON Lines frame.
pub fn decode_frame<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Result<Option<T>, serde_json::Error> {
    if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line = &buf[..pos];
        let msg = serde_json::from_slice(line)?;
        Ok(Some(msg))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let req = AgentRequest::Ping {
            request_id: "r1".into(),
        };
        let frame = encode_frame(&req).unwrap();
        let decoded = decode_frame::<AgentRequest>(&frame).unwrap().unwrap();
        match decoded {
            AgentRequest::Ping { request_id } => assert_eq!(request_id, "r1"),
            _ => panic!("wrong variant"),
        }
    }
}

#[test]
fn test_exec_serialize_interactive() {
    let req = AgentRequest::Exec {
        request_id: "test".into(),
        cmd: vec!["/bin/sh".into()],
        env: std::collections::HashMap::new(),
        working_dir: None,
        stdin: None,
        timeout_ms: None,
        interactive: true,
        tty: true,
        window_size: Some(WindowSize { rows: 24, cols: 80 }),
    };
    let s = serde_json::to_string(&req).unwrap();
    println!("{}", s);
    assert!(s.contains("\"interactive\":true"));
    assert!(s.contains("\"tty\":true"));
}
