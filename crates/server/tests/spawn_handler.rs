//! End-to-end tests for the daemon's Spawn → PTY → bus pipeline.
//!
//! Each test drives `Server::serve` over `channel::pair`, sends real
//! `Command::Spawn` / `Write` / `Resize` / `Close` commands, and
//! observes the events that come back. PTYs are real — we spawn
//! actual subprocesses (`echo`, `cat`) so the contract under test is
//! genuine, not mocked.

use pilot_store::MemoryStore;
use pilot_ipc::{Command, Event, TerminalKind, channel};
use pilot_server::backend::{SessionBackend, TmuxBackend};
use pilot_server::{Server, ServerConfig};
use std::sync::Arc;
use std::time::Duration;

/// Drain events until we see one matching `pred` or hit the deadline.
async fn wait_for<F: FnMut(&Event) -> bool>(
    client: &mut pilot_ipc::Client,
    mut pred: F,
    timeout: Duration,
) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, client.recv()).await {
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

#[tokio::test]
async fn spawn_shell_emits_terminal_spawned_event() {
    let config = ServerConfig::in_memory();
    let mut client = run_daemon(config.clone()).await;

    client.send(Command::Subscribe).unwrap();
    let _snapshot = client.recv().await.expect("snapshot");

    client
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
        })
        .unwrap();

    let evt = wait_for(
        &mut client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await
    .expect("TerminalSpawned arrived");

    match evt {
        Event::TerminalSpawned { kind, .. } => {
            assert!(matches!(kind, TerminalKind::Shell));
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn unknown_agent_id_emits_provider_error() {
    let config = ServerConfig::in_memory();
    let mut client = run_daemon(config.clone()).await;
    client.send(Command::Subscribe).unwrap();
    let _ = client.recv().await;

    client
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind: TerminalKind::Agent("does-not-exist".into()),
            cwd: None,
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
}

#[tokio::test]
async fn spawned_subprocess_output_reaches_client_via_bus() {
    // Spawn `printf hello`; observe the bytes coming back. Uses
    // printf rather than echo for portable no-newline output.
    let config = ServerConfig::in_memory();
    let mut client = run_daemon(config.clone()).await;
    client.send(Command::Subscribe).unwrap();
    let _ = client.recv().await;

    // Override the Shell kind by spawning a specific argv via a
    // LogTail of /dev/null then write — actually simpler: spawn the
    // shell and immediately Write a command, then read the echo.
    client
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
        })
        .unwrap();

    let spawned = wait_for(
        &mut client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await
    .expect("spawned");
    let terminal_id = match spawned {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    };

    // Run a deterministic command and look for its output. We use
    // `printf` because echo's behavior differs across shells.
    client
        .send(Command::Write {
            terminal_id,
            bytes: b"printf 'pilot-marker'\n".to_vec(),
        })
        .unwrap();

    let saw_marker = wait_for(
        &mut client,
        |e| match e {
            Event::TerminalOutput {
                terminal_id: tid,
                bytes,
                ..
            } => {
                *tid == terminal_id
                    && std::str::from_utf8(bytes)
                        .unwrap_or("")
                        .contains("pilot-marker")
            }
            _ => false,
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        saw_marker.is_some(),
        "expected to see 'pilot-marker' in PTY output"
    );
}

#[tokio::test]
async fn close_drops_terminal_and_emits_exit_event() {
    let config = ServerConfig::in_memory();
    let mut client = run_daemon(config.clone()).await;
    client.send(Command::Subscribe).unwrap();
    let _ = client.recv().await;

    client
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
        })
        .unwrap();

    let spawned = wait_for(
        &mut client,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await
    .expect("spawned");
    let terminal_id = match spawned {
        Event::TerminalSpawned { terminal_id, .. } => terminal_id,
        _ => unreachable!(),
    };

    client.send(Command::Close { terminal_id }).unwrap();

    // The close handler removes the PTY from the map; the master is
    // dropped which sends SIGHUP to the child. The output pump task
    // observes the broadcast close and emits TerminalExited.
    let exited = wait_for(
        &mut client,
        |e| matches!(e, Event::TerminalExited { terminal_id: tid, .. } if *tid == terminal_id),
        Duration::from_secs(5),
    )
    .await;
    assert!(exited.is_some(), "TerminalExited should arrive after Close");

    // And the map should be empty.
    let map_len = config.terminals.lock().await.len();
    assert_eq!(map_len, 0, "terminal map cleared after exit");
}

#[tokio::test]
async fn snapshot_includes_running_terminals_for_late_subscribers() {
    let config = ServerConfig::in_memory();
    let mut producer = run_daemon(config.clone()).await;
    producer.send(Command::Subscribe).unwrap();
    let _ = producer.recv().await;

    producer
        .send(Command::Spawn {
            session_key: "test:ws-1".into(),
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
        })
        .unwrap();
    let _ = wait_for(
        &mut producer,
        |e| matches!(e, Event::TerminalSpawned { .. }),
        Duration::from_secs(2),
    )
    .await;

    // Now a second client subscribes — it should see the terminal in
    // the snapshot (no replay of the spawn event since that already
    // fired before this client connected).
    let mut consumer = run_daemon(config.clone()).await;
    consumer.send(Command::Subscribe).unwrap();
    let evt = consumer.recv().await.expect("snapshot");
    match evt {
        Event::Snapshot { terminals, .. } => {
            assert_eq!(terminals.len(), 1, "running terminal in snapshot");
        }
        _ => panic!("expected Snapshot first"),
    }
}

/// Recovery scenario: a TmuxBackend has a session running (simulating
/// "pilot crashed"), then a fresh ServerConfig is built around the same
/// backend (simulating "pilot restarted"). `recover_sessions` should
/// register the survivor on the new config so the TUI sees it.
#[tokio::test]
async fn recover_sessions_reattaches_survivors() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: tmux unavailable");
        return;
    }

    let socket = format!("pilot-recover-{}", std::process::id());
    let backend = Arc::new(TmuxBackend::with_socket(&socket).unwrap());

    // Pre-existing session — simulates one that survived the previous
    // pilot run. We spawn it directly through the backend, NOT through
    // spawn_handler, so it's known to tmux but not to any ServerConfig.
    let preexisting = backend
        .spawn(
            &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            None,
            &[],
        )
        .await
        .unwrap();

    // Build a fresh config pointing at the SAME backend instance — its
    // terminals map starts empty.
    let store: Arc<dyn pilot_store::Store> = Arc::new(MemoryStore::new());
    let config = ServerConfig::with_store_and_backend(store, backend.clone());
    assert!(config.terminals.lock().await.is_empty());

    // Listen on the bus before recovery so the TerminalSpawned event
    // doesn't get lost.
    let mut bus = config.bus.subscribe();

    pilot_server::spawn_handler::recover_sessions(&config).await;

    // Map now has the survivor under a fresh wire id.
    let map = config.terminals.lock().await;
    assert_eq!(map.len(), 1, "expected one recovered session, got {map:?}");
    let recovered_key = map.values().next().unwrap().clone();
    assert_eq!(recovered_key, preexisting);
    drop(map);

    // The TerminalSpawned event hits the bus so any connected TUI sees
    // the recovered terminal alongside fresh spawns.
    let evt = tokio::time::timeout(Duration::from_secs(1), bus.recv())
        .await
        .expect("bus event")
        .expect("not closed");
    assert!(matches!(evt, Event::TerminalSpawned { .. }));

    // Cleanup: kill the session so the tmux server has nothing left.
    backend.kill(&preexisting).await.unwrap();
}
