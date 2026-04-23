//! pilot-daemon — owns state and IO on behalf of TUI clients.
//!
//! Lives as a library so the in-process transport can call `Daemon::start`
//! without a subprocess. When out-of-process (remote / long-running
//! service) the separate `pilot-daemon` binary calls the same entrypoint.
//!
//! This is a Week-1 skeleton. The actual session/provider/worktree/PTY
//! wiring moves in from v1 over the following weeks. See `../DESIGN.md`.

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
    pub async fn serve(&self, mut server: Server) -> anyhow::Result<()> {
        // Week-1 stub: accept Subscribe, echo Snapshot, sit idle.
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
                _ => {
                    // Real handling ships progressively in weeks 2–3.
                }
            }
        }
        Ok(())
    }
}
