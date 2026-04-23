//! Smoke tests for the daemon's serve loop. We drive it over
//! `ipc::channel::pair` — zero serialization, zero sockets — so tests
//! are fast and deterministic.

use pilot_v2_daemon::{Daemon, DaemonConfig};
use pilot_v2_ipc::{Command, Event, channel};

#[tokio::test]
async fn subscribe_yields_snapshot() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Daemon::new(DaemonConfig::from_user_config())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Subscribe).unwrap();
    let evt = client.recv().await.expect("daemon responds");
    match evt {
        Event::Snapshot { sessions, terminals } => {
            // Contract under test: Subscribe ALWAYS replies with a
            // Snapshot before any live events. With no sessions loaded
            // and no terminals spawned, both lists are empty; the
            // provider-polling and terminal-manager subsystems that
            // populate them come in Week 2.
            assert!(sessions.is_empty());
            assert!(terminals.is_empty());
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_closes_loop_cleanly() {
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        Daemon::new(DaemonConfig::from_user_config())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Shutdown).unwrap();
    // Drop client to unblock the channel close path.
    drop(client);
    // If Shutdown isn't honored the task would hang here forever; the
    // test timeout is our backstop but a clean exit is the contract.
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("daemon exits promptly on Shutdown")
        .unwrap();
}

#[tokio::test]
async fn unknown_command_does_not_crash() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Daemon::new(DaemonConfig::from_user_config())
            .serve(server)
            .await
            .unwrap();
    });

    // Send a command whose handler module isn't wired in yet. The loop
    // must keep running (trace-log + continue), not panic. This is the
    // contract that lets us land handlers incrementally without
    // breaking clients that know about more commands than the daemon.
    client
        .send(Command::MarkRead {
            session_key: "x".into(),
        })
        .unwrap();
    // Then prove we're still responsive.
    client.send(Command::Subscribe).unwrap();
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon is still responsive")
        .expect("got an event");
    matches!(evt, Event::Snapshot { .. });
}

#[tokio::test]
async fn client_drop_terminates_daemon_loop() {
    // The daemon is a long-running service but a single-client loop
    // should exit when its client drops; multi-client handling comes
    // later with a multiplexer.
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        Daemon::new(DaemonConfig::from_user_config())
            .serve(server)
            .await
            .unwrap();
    });
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("daemon exits when client drops")
        .unwrap();
}
