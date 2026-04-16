//! Wire protocol for TUI ↔ daemon communication.
//!
//! Length-prefixed JSON over Unix socket. Binary data (PTY output)
//! is base64-encoded within JSON for simplicity.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Request from TUI to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Spawn a new terminal session.
    Spawn {
        id: String,
        cmd: Vec<String>,
        cwd: String,
        env: HashMap<String, String>,
        cols: u16,
        rows: u16,
    },
    /// Send input bytes to a session's PTY.
    Write {
        id: String,
        /// Base64-encoded bytes.
        data: String,
    },
    /// Resize a session's PTY.
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Subscribe to output from a session (get replay + live stream).
    Subscribe {
        id: String,
    },
    /// Stop receiving output from a session.
    Unsubscribe {
        id: String,
    },
    /// List all active sessions.
    List,
    /// Kill a session.
    Kill {
        id: String,
    },
    /// Ping (health check).
    Ping,
}

/// Response from daemon to TUI.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Session spawned successfully.
    Spawned { id: String },
    /// PTY output data (streamed while subscribed).
    Output {
        id: String,
        /// Base64-encoded raw PTY bytes.
        data: String,
    },
    /// Session's child process exited.
    Finished { id: String, exit_code: Option<i32> },
    /// List of active session IDs.
    Sessions { ids: Vec<SessionInfo> },
    /// Error response.
    Error { message: String },
    /// Pong (health check reply).
    Pong,
    /// Acknowledgment.
    Ok,
}

/// Info about an active daemon session.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub finished: bool,
    pub cols: u16,
    pub rows: u16,
}

// ─── Framing: 4-byte BE length prefix + JSON ─────────────────────────────

/// Send a length-prefixed JSON message.
pub async fn send_message<T: Serialize>(
    stream: &mut (impl AsyncWriteExt + Unpin),
    msg: &T,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Receive a length-prefixed JSON message.
pub async fn recv_message<T: for<'de> Deserialize<'de>>(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 10 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })
}
