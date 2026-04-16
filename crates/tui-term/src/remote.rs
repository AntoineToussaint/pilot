//! Remote terminal session — connects to pilot-daemon via Unix socket.
//!
//! Uses TWO connections to the daemon:
//! - Connection 1: Spawn + Subscribe (receives output stream)
//! - Connection 2: Write/Resize (sends input)
//!
//! This avoids interleaving reads and writes on a single socket.

use crate::local::TermError;
use portable_pty::PtySize;
use std::io::Write as _;
use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

pub struct RemoteTermSession {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    socket_path: std::path::PathBuf,
    write_tx: mpsc::Sender<Vec<u8>>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    size: PtySize,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
    terminal: libghostty_vt::Terminal<'static, 'static>,
    render_state: libghostty_vt::RenderState<'static>,
    row_iter: libghostty_vt::render::RowIterator<'static>,
    cell_iter: libghostty_vt::render::CellIterator<'static>,
    last_output_at: Instant,
    recent_output: Vec<u8>,
}

/// Send a length-prefixed JSON message (blocking).
fn send_msg(stream: &mut std::os::unix::net::UnixStream, val: &serde_json::Value) -> Result<(), TermError> {
    let payload = serde_json::to_vec(val).map_err(|e| TermError::Terminal(e.to_string()))?;
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).map_err(TermError::Io)?;
    stream.write_all(&payload).map_err(TermError::Io)?;
    stream.flush().map_err(TermError::Io)?;
    Ok(())
}

/// Read a length-prefixed JSON message (blocking).
fn recv_msg(stream: &mut std::os::unix::net::UnixStream) -> Result<serde_json::Value, TermError> {
    use std::io::Read;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(TermError::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).map_err(TermError::Io)?;
    serde_json::from_slice(&buf).map_err(|e| TermError::Terminal(e.to_string()))
}

impl RemoteTermSession {
    pub fn connect(
        socket_path: &Path,
        session_id: &str,
        cmd: &[&str],
        size: PtySize,
        cwd: Option<&Path>,
        env: Vec<(String, String)>,
    ) -> Result<Self, TermError> {
        use std::os::unix::net::UnixStream;

        // ── Connection 1: Spawn + Subscribe (for reading output) ──
        let mut read_conn = UnixStream::connect(socket_path).map_err(TermError::Io)?;

        // Spawn.
        send_msg(&mut read_conn, &serde_json::json!({
            "type": "spawn",
            "id": session_id,
            "cmd": cmd,
            "cwd": cwd.map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            "env": env.into_iter().collect::<std::collections::HashMap<_, _>>(),
            "cols": size.cols,
            "rows": size.rows,
        }))?;

        let resp = recv_msg(&mut read_conn)?;
        if resp.get("type").and_then(|t| t.as_str()) == Some("error") {
            let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(TermError::Terminal(format!("daemon: {msg}")));
        }

        // Subscribe.
        send_msg(&mut read_conn, &serde_json::json!({
            "type": "subscribe",
            "id": session_id,
        }))?;

        // ── Connection 2: Write (separate connection for sending input) ──
        let mut write_conn = UnixStream::connect(socket_path).map_err(TermError::Io)?;

        // ── Background threads ──
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Reader thread — reads output from subscribe connection.
        let reader_finished = finished.clone();
        std::thread::spawn(move || {
            loop {
                match recv_msg(&mut read_conn) {
                    Ok(resp) => {
                        match resp.get("type").and_then(|t| t.as_str()) {
                            Some("output") => {
                                if let Some(data) = resp.get("data").and_then(|d| d.as_str()) {
                                    if let Ok(bytes) = base64::Engine::decode(
                                        &base64::engine::general_purpose::STANDARD,
                                        data,
                                    ) {
                                        let _ = output_tx.send(bytes);
                                    }
                                }
                            }
                            Some("finished") => {
                                reader_finished.store(true, std::sync::atomic::Ordering::Relaxed);
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Writer thread — sends keystrokes on the write connection.
        let write_id = session_id.to_string();
        std::thread::spawn(move || {
            while let Ok(bytes) = write_rx.recv() {
                let encoded = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &bytes,
                );
                let req = serde_json::json!({
                    "type": "write",
                    "id": write_id,
                    "data": encoded,
                });
                if send_msg(&mut write_conn, &req).is_err() {
                    break;
                }
                // Read and discard the Ok response.
                let _ = recv_msg(&mut write_conn);
            }
        });

        let terminal = libghostty_vt::Terminal::new(libghostty_vt::TerminalOptions {
            cols: size.cols,
            rows: size.rows,
            max_scrollback: 10_000,
        }).map_err(|e| TermError::Terminal(e.to_string()))?;
        let render_state = libghostty_vt::RenderState::new()
            .map_err(|e| TermError::Terminal(e.to_string()))?;
        let row_iter = libghostty_vt::render::RowIterator::new()
            .map_err(|e| TermError::Terminal(e.to_string()))?;
        let cell_iter = libghostty_vt::render::CellIterator::new()
            .map_err(|e| TermError::Terminal(e.to_string()))?;

        Ok(Self {
            session_id: session_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            write_tx,
            output_rx,
            size,
            finished,
            terminal,
            render_state,
            row_iter,
            cell_iter,
            last_output_at: Instant::now(),
            recent_output: Vec::with_capacity(4096),
        })
    }

    pub fn process_pending(&mut self) -> bool {
        let mut had_output = false;
        while let Ok(chunk) = self.output_rx.try_recv() {
            self.terminal.vt_write(&chunk);
            self.recent_output.extend_from_slice(&chunk);
            if self.recent_output.len() > 4096 {
                let excess = self.recent_output.len() - 4096;
                self.recent_output.copy_within(excess.., 0);
                self.recent_output.truncate(4096);
            }
            had_output = true;
        }
        if had_output {
            self.last_output_at = Instant::now();
        }
        had_output
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), TermError> {
        self.write_tx.send(data.to_vec())
            .map_err(|e| TermError::Terminal(format!("write channel closed: {e}")))?;
        Ok(())
    }

    pub fn resize(&mut self, size: PtySize) -> Result<(), TermError> {
        self.size = size;
        // TODO: send resize on write connection.
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn last_output_at(&self) -> Instant { self.last_output_at }
    pub fn recent_output(&self) -> &[u8] { &self.recent_output }
    pub fn size(&self) -> PtySize { self.size }

    pub fn render_data(
        &mut self,
    ) -> (
        &mut libghostty_vt::Terminal<'static, 'static>,
        &mut libghostty_vt::RenderState<'static>,
        &mut libghostty_vt::render::RowIterator<'static>,
        &mut libghostty_vt::render::CellIterator<'static>,
    ) {
        (&mut self.terminal, &mut self.render_state, &mut self.row_iter, &mut self.cell_iter)
    }

    pub fn scroll_up(&mut self, _lines: usize) {}
    pub fn scroll_down(&mut self, _lines: usize) {}
    pub fn scroll_reset(&mut self) {}
    pub fn is_scrolled(&self) -> bool { false }
}
