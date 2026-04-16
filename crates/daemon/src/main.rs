//! pilot-daemon: Long-running process that owns PTY sessions.
//!
//! Sessions survive TUI quit/restart. The TUI connects via Unix socket
//! to spawn, write to, and receive output from terminals.

mod protocol;

use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use protocol::{Request, Response, SessionInfo};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, warn};

const REPLAY_BUFFER_SIZE: usize = 64 * 1024; // 64KB replay for reconnect

fn default_socket_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".pilot").join("daemon.sock")
}

/// A daemon-owned terminal session.
struct DaemonSession {
    id: String,
    cmd: Vec<String>,
    cwd: String,
    writer: Box<dyn Write + Send>,
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
    size: PtySize,
    finished: Arc<std::sync::atomic::AtomicBool>,
    /// Broadcast channel for live output to subscribers.
    output_tx: broadcast::Sender<Vec<u8>>,
    /// Ring buffer of recent output for replay on reconnect.
    replay_buffer: Vec<u8>,
}

impl DaemonSession {
    fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::Relaxed)
    }
}

type Sessions = Arc<Mutex<HashMap<String, DaemonSession>>>;

#[tokio::main]
async fn main() {
    let log_file = std::fs::File::create("/tmp/pilot-daemon.log").ok();
    if let Some(f) = log_file {
        tracing_subscriber::fmt()
            .with_writer(f)
            .with_ansi(false)
            .init();
    }

    let socket_path = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_socket_path);

    // Remove stale socket.
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind {}: {e}", socket_path.display());
            std::process::exit(1);
        }
    };

    info!("pilot-daemon listening at {}", socket_path.display());

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    // Graceful shutdown on Ctrl-C.
    let sessions_shutdown = Arc::clone(&sessions);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("Shutting down — killing all sessions");
        let mut s = sessions_shutdown.lock().await;
        for (id, session) in s.iter_mut() {
            info!("Killing session {id}");
            let _ = session.writer.write_all(&[0x03]); // Ctrl-C
        }
        s.clear();
        std::process::exit(0);
    });

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!("Accept error: {e}");
                continue;
            }
        };

        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, sessions).await {
                warn!("Client error: {e}");
            }
        });
    }
}

async fn handle_client(mut stream: UnixStream, sessions: Sessions) -> std::io::Result<()> {
    info!("Client connected");

    loop {
        let req: Request = match protocol::recv_message(&mut stream).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                info!("Client disconnected");
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        match req {
            Request::Ping => {
                protocol::send_message(&mut stream, &Response::Pong).await?;
            }

            Request::List => {
                let s = sessions.lock().await;
                let ids: Vec<SessionInfo> = s.values().map(|session| {
                    SessionInfo {
                        id: session.id.clone(),
                        cmd: session.cmd.clone(),
                        cwd: session.cwd.clone(),
                        finished: session.is_finished(),
                        cols: session.size.cols,
                        rows: session.size.rows,
                    }
                }).collect();
                protocol::send_message(&mut stream, &Response::Sessions { ids }).await?;
            }

            Request::Spawn { id, cmd, cwd, env, cols, rows } => {
                let result = spawn_session(&id, &cmd, &cwd, &env, cols, rows, &sessions).await;
                match result {
                    Ok(()) => {
                        protocol::send_message(&mut stream, &Response::Spawned { id }).await?;
                    }
                    Err(e) => {
                        protocol::send_message(&mut stream, &Response::Error {
                            message: e.to_string(),
                        }).await?;
                    }
                }
            }

            Request::Write { id, data } => {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &data,
                ).unwrap_or_default();
                let mut s = sessions.lock().await;
                if let Some(session) = s.get_mut(&id) {
                    let _ = session.writer.write_all(&bytes);
                    let _ = session.writer.flush();
                    protocol::send_message(&mut stream, &Response::Ok).await?;
                } else {
                    protocol::send_message(&mut stream, &Response::Error {
                        message: format!("session not found: {id}"),
                    }).await?;
                }
            }

            Request::Resize { id, cols, rows } => {
                let mut s = sessions.lock().await;
                if let Some(session) = s.get_mut(&id) {
                    let new_size = PtySize {
                        rows, cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    };
                    let _ = session.master.resize(new_size);
                    session.size = new_size;
                    protocol::send_message(&mut stream, &Response::Ok).await?;
                } else {
                    protocol::send_message(&mut stream, &Response::Error {
                        message: format!("session not found: {id}"),
                    }).await?;
                }
            }

            Request::Subscribe { id } => {
                let (mut rx, replay) = {
                    let s = sessions.lock().await;
                    if let Some(session) = s.get(&id) {
                        (session.output_tx.subscribe(), session.replay_buffer.clone())
                    } else {
                        protocol::send_message(&mut stream, &Response::Error {
                            message: format!("session not found: {id}"),
                        }).await?;
                        continue;
                    }
                };

                // Send replay buffer first.
                if !replay.is_empty() {
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &replay,
                    );
                    protocol::send_message(&mut stream, &Response::Output {
                        id: id.clone(),
                        data: encoded,
                    }).await?;
                }

                // Stream live output until unsubscribe or disconnect.
                loop {
                    tokio::select! {
                        result = rx.recv() => {
                            match result {
                                Ok(bytes) => {
                                    let encoded = base64::Engine::encode(
                                        &base64::engine::general_purpose::STANDARD,
                                        &bytes,
                                    );
                                    if protocol::send_message(&mut stream, &Response::Output {
                                        id: id.clone(),
                                        data: encoded,
                                    }).await.is_err() {
                                        break; // Client disconnected.
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                    warn!("Subscriber lagged by {n} messages");
                                }
                                Err(broadcast::error::RecvError::Closed) => {
                                    // Session finished.
                                    let _ = protocol::send_message(&mut stream, &Response::Finished {
                                        id: id.clone(),
                                        exit_code: None,
                                    }).await;
                                    break;
                                }
                            }
                        }
                        // Also check for incoming requests (like Unsubscribe).
                        incoming = protocol::recv_message::<Request>(&mut stream) => {
                            match incoming {
                                Ok(Request::Unsubscribe { .. }) => break,
                                Ok(other) => {
                                    // Handle other requests inline during subscription.
                                    warn!("Got {:?} while subscribed, ignoring", other);
                                }
                                Err(_) => break, // Disconnected.
                            }
                        }
                    }
                }
            }

            Request::Unsubscribe { .. } => {
                // No-op if not subscribed.
                protocol::send_message(&mut stream, &Response::Ok).await?;
            }

            Request::Kill { id } => {
                let mut s = sessions.lock().await;
                if let Some(mut session) = s.remove(&id) {
                    let _ = session.writer.write_all(&[0x03]); // Ctrl-C
                    info!("Killed session {id}");
                    protocol::send_message(&mut stream, &Response::Ok).await?;
                } else {
                    protocol::send_message(&mut stream, &Response::Error {
                        message: format!("session not found: {id}"),
                    }).await?;
                }
            }
        }
    }
}

async fn spawn_session(
    id: &str,
    cmd: &[String],
    cwd: &str,
    env: &HashMap<String, String>,
    cols: u16,
    rows: u16,
    sessions: &Sessions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pty_system = NativePtySystem::default();
    let size = PtySize {
        rows: rows.max(10),
        cols: cols.max(20),
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system.openpty(size)?;

    let program = cmd.first().ok_or("empty command")?;
    let mut command = CommandBuilder::new(program);
    for arg in &cmd[1..] {
        command.arg(arg);
    }
    if !cwd.is_empty() {
        command.cwd(cwd);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    command.env("TERM", "xterm-256color");

    let _child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let writer = pair.master.take_writer()?;
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (output_tx, _) = broadcast::channel::<Vec<u8>>(256);

    // Reader thread — reads PTY output, broadcasts to subscribers, fills replay buffer.
    let reader_finished = Arc::clone(&finished);
    let reader_tx = output_tx.clone();
    let session_id = id.to_string();
    let sessions_for_reader = Arc::clone(sessions);
    let mut reader = pair.master.try_clone_reader()?;

    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    // Append to replay buffer (under lock).
                    {
                        let sessions = sessions_for_reader.blocking_lock();
                        if sessions.contains_key(&session_id) {
                            drop(sessions);
                            let mut sessions = sessions_for_reader.blocking_lock();
                            if let Some(session) = sessions.get_mut(&session_id) {
                                session.replay_buffer.extend_from_slice(&chunk);
                                if session.replay_buffer.len() > REPLAY_BUFFER_SIZE {
                                    let excess = session.replay_buffer.len() - REPLAY_BUFFER_SIZE;
                                    session.replay_buffer.copy_within(excess.., 0);
                                    session.replay_buffer.truncate(REPLAY_BUFFER_SIZE);
                                }
                            }
                        }
                    }
                    // Broadcast to subscribers (ignore if no subscribers).
                    let _ = reader_tx.send(chunk);
                }
                Err(_) => break,
            }
        }
        reader_finished.store(true, std::sync::atomic::Ordering::Relaxed);
        info!("Session {session_id} PTY reader finished");
    });

    let session = DaemonSession {
        id: id.to_string(),
        cmd: cmd.to_vec(),
        cwd: cwd.to_string(),
        writer,
        master: pair.master,
        size,
        finished,
        output_tx,
        replay_buffer: Vec::with_capacity(REPLAY_BUFFER_SIZE),
    };

    sessions.lock().await.insert(id.to_string(), session);
    info!("Spawned session {id}: {:?} in {cwd}", cmd);
    Ok(())
}
