//! Daemon-side PTY.
//!
//! The daemon doesn't render terminals — that's the client's job with
//! its own libghostty-vt. The daemon only needs to (1) spawn a PTY,
//! (2) stream raw bytes out to subscribers, (3) accept bytes in,
//! (4) resize, (5) know when the child exits. So this is deliberately
//! smaller than `pilot-tui-term::TermSession` and critically it's
//! **Send-safe** — no libghostty pointers.
//!
//! Subscription model: one terminal can have N subscribers (e.g. a
//! local TUI plus a remote TUI watching the same daemon). Each new
//! subscription gets the ring-buffer replay first, then a broadcast
//! stream of new bytes. Dropped subscribers are cleaned up in the
//! main loop when `send` errors.

use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Mutex, broadcast, oneshot};

/// Ring-buffer capacity for per-terminal output replay. 64 KiB matches
/// a typical terminal scrollback and is enough to reconstruct the
/// visible screen after a client reconnects.
pub const REPLAY_RING_BYTES: usize = 64 * 1024;

/// Broadcast channel capacity. If a subscriber lags by more than this
/// many chunks it gets dropped with `RecvError::Lagged` — ring-buffer
/// replay on reconnect is how we recover that client.
pub const BROADCAST_CAPACITY: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("PTY open: {0}")]
    Open(String),
    #[error("PTY spawn: {0}")]
    Spawn(String),
    #[error("PTY write: {0}")]
    Write(#[from] std::io::Error),
    #[error("PTY already closed")]
    Closed,
}

/// One chunk of PTY output with its monotonic sequence number.
/// Carried on the broadcast channel so subscribers can detect gaps.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub seq: u64,
    pub bytes: Arc<[u8]>,
}

/// Daemon-side handle to a running PTY. `Send + Sync`.
pub struct DaemonPty {
    /// Writer into the PTY's stdin. Behind a Mutex because `tokio::spawn`'d
    /// tasks can't hold a `&mut` across await points without one.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Master end of the PTY, needed for resize.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Broadcast channel for live output. Subscribers get chunks with
    /// monotonic `seq` so replay+live can be stitched without dupes.
    output_tx: broadcast::Sender<OutputChunk>,
    /// Recent output, capped at `REPLAY_RING_BYTES`.
    ring: Arc<Mutex<ReplayRing>>,
    /// Set by the reader thread when the PTY reports EOF. Subscribers
    /// use this + `exit_code` to stop listening.
    finished: Arc<AtomicBool>,
    /// Filled exactly once when the child exits.
    exit_rx: Arc<Mutex<Option<oneshot::Receiver<Option<i32>>>>>,
    /// Latest assigned seq. Reader thread increments.
    last_seq: Arc<AtomicU64>,
}

/// Fixed-capacity byte ring. Writes overwrite the oldest bytes; reads
/// return a logical linear slice of everything currently stored.
#[derive(Debug)]
pub struct ReplayRing {
    buf: Vec<u8>,
    /// Capacity the ring enforces on the buffer's length.
    cap: usize,
    /// Total bytes ever written. The ring contains bytes
    /// `[total - buf.len(), total)`. Monotonic — useful for
    /// cross-checking seq numbers in tests.
    pub(crate) total_written: u64,
}

impl Default for ReplayRing {
    fn default() -> Self {
        Self::with_capacity(REPLAY_RING_BYTES)
    }
}

impl ReplayRing {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            total_written: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total_written += bytes.len() as u64;
        if bytes.len() >= self.cap {
            // Incoming burst alone exceeds capacity — keep only the tail.
            let tail_start = bytes.len() - self.cap;
            self.buf.clear();
            self.buf.extend_from_slice(&bytes[tail_start..]);
            return;
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.copy_within(excess.., 0);
            self.buf.truncate(self.cap);
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// A subscription to a `DaemonPty`'s output. Includes the replay so
/// the caller can reconstruct the screen, then the live stream for
/// everything after.
pub struct Subscription {
    pub replay: Vec<u8>,
    pub last_seq: u64,
    pub live: broadcast::Receiver<OutputChunk>,
}

impl DaemonPty {
    /// Spawn a command in a new PTY. `env` augments (does not replace)
    /// the parent environment except for `TERM` which we override to
    /// `xterm-256color` so agents render consistent colors.
    pub fn spawn(
        cmd: &[String],
        size: PtySize,
        cwd: Option<&PathBuf>,
        env: Vec<(String, String)>,
    ) -> Result<Self, PtyError> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(size)
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let program = cmd
            .first()
            .ok_or_else(|| PtyError::Spawn("empty command".into()))?;
        let mut command = CommandBuilder::new(program);
        for arg in &cmd[1..] {
            command.arg(arg);
        }
        if let Some(dir) = cwd {
            command.cwd(dir);
        }
        for (k, v) in env {
            command.env(k, v);
        }
        command.env("TERM", "xterm-256color");

        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let (output_tx, _) = broadcast::channel::<OutputChunk>(BROADCAST_CAPACITY);
        let ring = Arc::new(Mutex::new(ReplayRing::with_capacity(REPLAY_RING_BYTES)));
        let finished = Arc::new(AtomicBool::new(false));
        let last_seq = Arc::new(AtomicU64::new(0));

        // Reader thread: blocks on PTY reads, fans bytes out to ring +
        // broadcast. Runs on std::thread because portable-pty's Read
        // impl is blocking.
        let reader_tx = output_tx.clone();
        let reader_ring = ring.clone();
        let reader_finished = finished.clone();
        let reader_seq = last_seq.clone();
        std::thread::Builder::new()
            .name("pilot-daemon-pty".into())
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF — child exited
                        Ok(n) => {
                            let bytes: Arc<[u8]> = Arc::from(&buf[..n]);
                            let seq = reader_seq.fetch_add(1, Ordering::SeqCst) + 1;
                            // Ring write uses blocking lock; this thread is
                            // dedicated so it's fine.
                            {
                                let mut r = reader_ring.blocking_lock();
                                r.push(&bytes);
                            }
                            // If no subscribers, broadcast returns error;
                            // we don't care — the ring holds the data.
                            let _ = reader_tx.send(OutputChunk { seq, bytes });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            tracing::warn!("PTY reader: {e}");
                            break;
                        }
                    }
                }
                reader_finished.store(true, Ordering::Release);
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        // Exit watcher — blocking `wait` on another thread, forwards
        // the exit code through a oneshot so the daemon loop can await.
        let (exit_tx, exit_rx) = oneshot::channel::<Option<i32>>();
        std::thread::Builder::new()
            .name("pilot-daemon-exit".into())
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => status.exit_code().try_into().ok(),
                    Err(e) => {
                        tracing::warn!("child.wait: {e}");
                        None
                    }
                };
                let _ = exit_tx.send(code);
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            master: Arc::new(Mutex::new(pair.master)),
            output_tx,
            ring,
            finished,
            exit_rx: Arc::new(Mutex::new(Some(exit_rx))),
            last_seq,
        })
    }

    /// Fire up a subscription: the current ring snapshot + a live feed.
    pub async fn subscribe(&self) -> Subscription {
        let (replay, last_seq) = {
            let ring = self.ring.lock().await;
            (ring.snapshot(), self.last_seq.load(Ordering::SeqCst))
        };
        let live = self.output_tx.subscribe();
        Subscription {
            replay,
            last_seq,
            live,
        }
    }

    pub async fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if self.finished.load(Ordering::Acquire) {
            return Err(PtyError::Closed);
        }
        let mut w = self.writer.lock().await;
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub async fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let m = self.master.lock().await;
        m.resize(size).map_err(|e| PtyError::Open(e.to_string()))
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Await the child's exit code. Returns None if the exit was
    /// unobservable (rare — `child.wait` error). Can only be called
    /// once per PTY; subsequent calls return None.
    pub async fn wait_exit(&self) -> Option<i32> {
        let mut slot = self.exit_rx.lock().await;
        let rx = slot.take()?;
        rx.await.ok().flatten()
    }

    /// Total bytes ever emitted by the PTY reader — NOT just what's
    /// retained in the ring. Used for byte-count metrics. For gap
    /// detection across reconnect, use `subscribe().last_seq` instead
    /// (seq counts chunks, not bytes).
    pub async fn total_written(&self) -> u64 {
        self.ring.lock().await.total_written
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn empty_ring_snapshot_is_empty() {
        let r = ReplayRing::with_capacity(8);
        assert!(r.is_empty());
        assert_eq!(r.snapshot(), Vec::<u8>::new());
        assert_eq!(r.total_written, 0);
    }

    #[test]
    fn push_under_capacity_preserves_everything() {
        let mut r = ReplayRing::with_capacity(16);
        r.push(b"hello");
        r.push(b" world");
        assert_eq!(r.snapshot(), b"hello world");
        assert_eq!(r.total_written, 11);
    }

    #[test]
    fn push_at_capacity_is_exact() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abcde");
        assert_eq!(r.snapshot(), b"abcde");
        assert_eq!(r.total_written, 5);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn push_over_capacity_drops_oldest() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abcdef");
        // Incoming burst larger than capacity — only the last 5 bytes kept.
        assert_eq!(r.snapshot(), b"bcdef");
        assert_eq!(r.total_written, 6);
    }

    #[test]
    fn wrap_preserves_tail() {
        let mut r = ReplayRing::with_capacity(5);
        r.push(b"abc");
        r.push(b"def");
        r.push(b"g");
        // Total: 7 bytes written, last 5 retained: "cdefg".
        assert_eq!(r.snapshot(), b"cdefg");
        assert_eq!(r.total_written, 7);
    }

    #[test]
    fn large_burst_keeps_only_tail() {
        let mut r = ReplayRing::with_capacity(8);
        r.push(b"early-bytes-"); // 12 bytes, gets dropped entirely next step
        let burst: Vec<u8> = (b'A'..=b'Z').collect(); // 26 bytes
        r.push(&burst);
        // Only the last 8 bytes of the burst should remain.
        assert_eq!(r.snapshot(), b"STUVWXYZ");
        assert_eq!(r.total_written, 12 + 26);
    }

    #[test]
    fn many_small_pushes_then_wrap() {
        let mut r = ReplayRing::with_capacity(10);
        for b in b'0'..=b'9' {
            r.push(&[b]);
        }
        assert_eq!(r.snapshot(), b"0123456789");
        // One more push evicts '0'.
        r.push(b"X");
        assert_eq!(r.snapshot(), b"123456789X");
        assert_eq!(r.total_written, 11);
    }

    #[test]
    fn total_written_is_monotonic_and_exact() {
        let mut r = ReplayRing::with_capacity(4);
        r.push(b"a");
        assert_eq!(r.total_written, 1);
        r.push(b"bc");
        assert_eq!(r.total_written, 3);
        r.push(b"defghijk");
        assert_eq!(r.total_written, 11); // wraps; total still tracks real count
    }
}
