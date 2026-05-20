//! End-to-end tests for the daemon's Spawn → backend → bus pipeline.
//!
//! Backend is the in-memory [`MockBackend`] — no real shells / tmux /
//! curl. Tests drive synthetic output via `MockBackend::emit` and end
//! sessions via `finish`.

use pilot_ipc::{Command, Event, TerminalKind, channel};
use pilot_server::backend::{MockBackend, SessionBackend};
use pilot_server::{Server, ServerConfig};
use pilot_store::MemoryStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Per-test deadline. Workspace rule: every async test bounds itself
/// so a deadlock is reported as a failure, not a hung suite.
const TEST_DEADLINE: Duration = Duration::from_secs(5);

/// Drain events until we see one matching `pred` or hit the deadline.
async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &mut pilot_ipc::Client,
    mut pred: F,
    budget: Duration,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, client.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            _ => return None,
        }
    }
    None
}

async fn run_daemon(config: ServerConfig) -> pilot_ipc::Client {
    let (client, server) = channel::pair();
    tokio::spawn(async move {
        let _ = Server::new(config).serve(server).await;
    });
    client
}

async fn subscribed(config: ServerConfig) -> pilot_ipc::Client {
    let mut client = run_daemon(config).await;
    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");
    client
}

async fn spawn_and_wait(
    client: &mut pilot_ipc::Client,
    kind: TerminalKind,
) -> pilot_ipc::TerminalId {
    client
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind,
            cwd: None,
            initial_prompt: None,
        })
        .unwrap();
    let spawned = wait_for(
        client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await
    .expect("TerminalSpawned arrived");
    match spawned {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn spawn_shell_emits_terminal_spawned_event() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;
        let _ = spawn_and_wait(&mut client, TerminalKind::Shell).await;
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn unknown_agent_id_emits_provider_error() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config).await;
        client
            .send(Command::Spawn {
                session_key: "test:ws-1".into(),
                session_id: None,
                kind: TerminalKind::Agent("does-not-exist".into()),
                cwd: None,
                initial_prompt: None,
            })
            .unwrap();
        let evt = wait_for(
            &mut client,
            |e| matches!(e, Event::ProviderError { .. }),
            Duration::from_secs(2),
        )
        .await
        .expect("ProviderError arrived");
        if let Event::ProviderError { message, .. } = evt {
            assert!(
                message.contains("no agent registered"),
                "unexpected message: {message}"
            );
        }
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn spawned_subprocess_output_reaches_client_via_bus() {
    timeout(TEST_DEADLINE, async {
        // Build the config + grab the typed mock so the test can
        // inject output the daemon's pump task will forward.
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut client = subscribed(config).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Shell).await;

        // Find the backend key the daemon assigned. There's exactly
        // one mocked session at this point.
        let keys = mock.list().await.unwrap();
        assert_eq!(keys.len(), 1);
        let key = keys.into_iter().next().unwrap();

        // Inject synthetic output. The pump task should forward it as
        // Event::TerminalOutput, exactly like a real PTY would.
        mock.emit(&key, b"pilot-marker").await;

        let evt = wait_for(
            &mut client,
            |e| match e {
                Event::TerminalOutput {
                    terminal_id: tid,
                    bytes,
                    ..
                } => *tid == terminal_id && bytes == b"pilot-marker",
                _ => false,
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(
            evt.is_some(),
            "expected to see 'pilot-marker' in TerminalOutput"
        );
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn close_drops_terminal_and_emits_exit_event() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut client = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut client, TerminalKind::Shell).await;

        client.send(Command::Close { terminal_id }).unwrap();

        // handle_close calls backend.kill; the mock closes its
        // subscribers, the pump task awaits wait_exit, then broadcasts
        // TerminalExited and removes the terminal from the map.
        let exited = wait_for(
            &mut client,
            |e| matches!(e, Event::TerminalExited { terminal_id: tid, .. } if *tid == terminal_id),
            Duration::from_secs(2),
        )
        .await;
        assert!(exited.is_some(), "TerminalExited should arrive after Close");

        // Map should be empty.
        let map_len = config.terminals.lock().await.len();
        assert_eq!(map_len, 0, "terminal map cleared after exit");
    })
    .await
    .expect("deadline");
}
#[tokio::test]
async fn snapshot_includes_running_terminals_for_late_subscribers() {
    timeout(TEST_DEADLINE, async {
        let config = ServerConfig::in_memory();
        let mut producer = subscribed(config.clone()).await;
        let _ = spawn_and_wait(&mut producer, TerminalKind::Shell).await;

        // A second client subscribes — its initial Snapshot should
        // include the terminal already running.
        let mut consumer = run_daemon(config.clone()).await;
        consumer.send(Command::Subscribe).unwrap();
        let evt = consumer.recv().await.expect("snapshot");
        match evt {
            Event::Snapshot { terminals, .. } => {
                assert_eq!(terminals.len(), 1, "running terminal in snapshot");
            }
            _ => panic!("expected Snapshot first"),
        }
    })
    .await
    .expect("deadline");
}
/// Regression: `--connect` clients reconnecting mid-session need the
/// PTY ring buffer in `TerminalSnapshot.replay` to reconstruct the
/// screen. Without it they see a blank terminal until the next chunk
/// arrives — which for an idle agent could be never.
#[tokio::test]
async fn snapshot_replay_includes_buffered_pty_output_for_late_subscribers() {
    timeout(TEST_DEADLINE, async {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let mut producer = subscribed(config.clone()).await;
        let terminal_id = spawn_and_wait(&mut producer, TerminalKind::Shell).await;

        // Drive synthetic output and wait for the pump task to fan it
        // out, so the next Snapshot will include it in `replay`.
        let key = mock.list().await.unwrap().into_iter().next().unwrap();
        mock.emit(&key, b"pilot-replay-marker").await;
        let _ = wait_for(
            &mut producer,
            |e| match e {
                Event::TerminalOutput { bytes, .. } => bytes == b"pilot-replay-marker",
                _ => false,
            },
            Duration::from_secs(2),
        )
        .await
        .expect("marker output reached bus");

        // Fresh client subscribes after the output landed.
        let mut consumer = run_daemon(config).await;
        consumer.send(Command::Subscribe).unwrap();
        let evt = consumer.recv().await.expect("snapshot");
        match evt {
            Event::Snapshot { terminals, .. } => {
                let term = terminals
                    .iter()
                    .find(|t| t.terminal_id == terminal_id)
                    .expect("our terminal in snapshot");
                assert_eq!(
                    term.replay, b"pilot-replay-marker",
                    "snapshot replay should contain pre-subscription output",
                );
                assert!(term.last_seq > 0, "last_seq advanced past 0");
            }
            _ => panic!("expected Snapshot first"),
        }
    })
    .await
    .expect("deadline");
}
/// Recovery scenario: a backend has a session running (simulating
/// "pilot crashed"), then a fresh `ServerConfig` is built around the
/// same backend (simulating "pilot restarted"). `recover_sessions`
/// should register the survivor on the new config so the TUI sees it.
#[tokio::test]
async fn recover_sessions_reattaches_survivors() {
    timeout(TEST_DEADLINE, async {
        let backend = MockBackend::new();
        // Pre-existing session — simulates one that survived the
        // previous pilot run. Spawned directly through the backend,
        // not through spawn_handler, so it's known to the backend
        // but not to any ServerConfig.
        let preexisting = backend
            .spawn(&["echo".into(), "hello".into()], None, &[], "preexisting")
            .await
            .unwrap();

        // Fresh config pointing at the SAME backend instance.
        let store: Arc<dyn pilot_store::Store> = Arc::new(MemoryStore::new());
        let backend_arc: Arc<dyn SessionBackend> = Arc::new(backend.clone());
        let config = ServerConfig::with_store_and_backend(store, backend_arc);
        assert!(config.terminals.lock().await.is_empty());

        // Listen on the bus before recovery so TerminalSpawned isn't lost.
        let mut bus = config.bus.subscribe();

        pilot_server::spawn_handler::recover_sessions(&config).await;

        // Map now has the survivor under a fresh wire id.
        let map = config.terminals.lock().await;
        assert_eq!(map.len(), 1, "expected one recovered session, got {map:?}");
        let recovered_key = map.values().next().unwrap().clone();
        assert_eq!(recovered_key, preexisting);
        drop(map);

        // TerminalSpawned hits the bus.
        let evt = timeout(Duration::from_secs(1), bus.recv())
            .await
            .expect("bus event")
            .expect("not closed");
        assert!(matches!(evt, Event::TerminalSpawned { .. }));
    })
    .await
    .expect("deadline");
}
