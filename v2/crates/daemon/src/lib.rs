//! pilot-daemon — owns state and IO on behalf of TUI clients.
//!
//! Lives as a library so the in-process transport can call `Daemon::serve`
//! without a subprocess. When out-of-process (remote access, long-running
//! service), the `pilot` binary's `daemon` subcommand invokes the same
//! `Daemon::serve` entrypoint over a Unix socket.
//!
//! Today the daemon exposes the PTY lifecycle (spawn/write/resize/close,
//! per-terminal ring buffer, reconnect replay) and the serve loop that
//! accepts `ipc::Command`s and emits `ipc::Event`s. Provider polling,
//! worktree management, agent hook plumbing, and LLM proxy integration
//! land on top of this core in the order described in `../DESIGN.md`.

pub mod pty;

use pilot_v2_agents::Registry;
use pilot_v2_ipc::{Event, Server};

/// Handle returned from `Daemon::start`. Dropping this terminates all
/// daemon background tasks via the attached tokio runtime / cancellation.
pub struct DaemonHandle {
    _marker: std::marker::PhantomData<()>,
}

pub struct DaemonConfig {
    pub agents: Registry,
    // pub config: pilot_config::Config,
    // pub store: Arc<dyn Store>,
    // ... filled in as the crate matures.
}

impl DaemonConfig {
    pub fn from_user_config() -> Self {
        Self {
            agents: Registry::default_builtins(),
        }
    }
}

pub struct Daemon {
    _config: DaemonConfig,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self { _config: config }
    }

    /// Accept a client connection (either an in-process `Server` from
    /// `ipc::channel::pair` or a remote `Server` from `ipc::socket::serve`).
    ///
    /// This serve loop currently handles the lifecycle commands needed
    /// for the Week-2 TUI build-out: `Subscribe` sends a `Snapshot` and
    /// holds the stream open, `Shutdown` exits cleanly, and any other
    /// command is acknowledged as received (for observability) and left
    /// to the handler modules being wired in for Week 2-3. Each new
    /// command gets handled here as its backing subsystem lands — not
    /// by rewriting this loop, but by dispatching to the module that
    /// owns that concern.
    pub async fn serve(&self, mut server: Server) -> anyhow::Result<()> {
        while let Some(cmd) = server.rx.recv().await {
            tracing::debug!("daemon ← {cmd:?}");
            match cmd {
                pilot_v2_ipc::Command::Subscribe => {
                    let _ = server.tx.send(Event::Snapshot {
                        sessions: vec![],
                        terminals: vec![],
                    });
                }
                pilot_v2_ipc::Command::Shutdown => break,
                other => {
                    tracing::trace!("daemon: command handler not yet wired: {other:?}");
                }
            }
        }
        Ok(())
    }
}
