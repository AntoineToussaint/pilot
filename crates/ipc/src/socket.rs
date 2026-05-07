//! Remote transport: length-prefixed bincode over a tokio `AsyncRead` /
//! `AsyncWrite`. Works with Unix sockets (local daemon) and SSH-tunneled
//! Unix sockets (remote daemon accessed via `ssh -L`).
//!
//! The surface mirrors `channel::pair` so the TUI receives the same
//! `Client` type regardless of transport. Internally, each transport
//! spawns a pair of tokio tasks that translate between the framed wire
//! and the local channels.

use crate::transport;
use crate::{Client, Command, Connection, Event, MAX_FRAME_BYTES};
use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large ({0} bytes); max is {MAX_FRAME_BYTES}")]
    TooLarge(u32),
    #[error("encode/decode: {0}")]
    Codec(#[from] bincode::Error),
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = bincode::serialize(msg)?;
    let len = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(bincode::deserialize(&buf)?))
}

/// Connect to a daemon listening at `path` (possibly tunneled by SSH
/// when the path is forwarded through `ssh -L`). Returns a `Client`
/// whose send/recv map to frames on the wire. Transport is delegated
/// to `transport::connect` — Unix domain socket today, named pipe /
/// TCP later.
pub async fn connect(path: &Path) -> std::io::Result<Client> {
    let (rd, wr) = transport::connect(path)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<Event>();

    // Writer task: drain Commands from TUI, frame them onto the socket.
    tokio::spawn(writer_loop(wr, cmd_rx));
    // Reader task: parse framed Events from the socket, push to TUI.
    tokio::spawn(reader_loop(rd, evt_tx));

    Ok(Client::from_channels(cmd_tx, evt_rx))
}

/// Connection-side: wrap an accepted socket as a `Connection` handle.
pub fn serve<R, W>(rd: R, wr: W) -> Connection
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<Event>();
    tokio::spawn(reader_loop(rd, cmd_tx));
    tokio::spawn(writer_loop(wr, evt_rx));
    Connection::from_channels(evt_tx, cmd_rx)
}

async fn writer_loop<W, M>(mut w: W, mut rx: mpsc::UnboundedReceiver<M>)
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_frame(&mut w, &msg).await {
            tracing::warn!("writer: {e}");
            break;
        }
    }
}

async fn reader_loop<R, M>(mut r: R, tx: mpsc::UnboundedSender<M>)
where
    R: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    loop {
        match read_frame::<_, M>(&mut r).await {
            Ok(Some(msg)) => {
                if tx.send(msg).is_err() {
                    break;
                }
            }
            Ok(None) => break, // clean EOF
            Err(e) => {
                tracing::warn!("reader: {e}");
                break;
            }
        }
    }
}
