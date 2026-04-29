//! Trait-level tests for `SessionBackend`.
//!
//! Each scenario is written once against `Arc<dyn SessionBackend>` and
//! then invoked from a per-impl test wrapper. Adding a new backend
//! means writing one new wrapper module — the contract suite stays
//! shared so all impls satisfy the same behavior.

use pilot_v2_server::backend::{RawPtyBackend, SessionBackend, TmuxBackend};
use std::sync::Arc;
use std::time::Duration;

fn shell_argv(cmd: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), cmd.into()]
}

// ---------------------------------------------------------------------
// Shared scenarios. Each takes an Arc<dyn SessionBackend> so it doesn't
// know which impl it's running against.
// ---------------------------------------------------------------------

async fn scenario_spawn_returns_unique_keys(b: Arc<dyn SessionBackend>) {
    let a = b.spawn(&shell_argv("sleep 5"), None, &[]).await.unwrap();
    let z = b.spawn(&shell_argv("sleep 5"), None, &[]).await.unwrap();
    assert_ne!(a, z, "each spawn yields a fresh key");
    let _ = b.kill(&a).await;
    let _ = b.kill(&z).await;
}

async fn scenario_list_reflects_live_sessions(b: Arc<dyn SessionBackend>) {
    let a = b.spawn(&shell_argv("sleep 5"), None, &[]).await.unwrap();
    let z = b.spawn(&shell_argv("sleep 5"), None, &[]).await.unwrap();
    let ls = b.list().await.unwrap();
    assert!(ls.contains(&a), "list missing {a}: {ls:?}");
    assert!(ls.contains(&z), "list missing {z}: {ls:?}");
    let _ = b.kill(&a).await;
    let _ = b.kill(&z).await;
}

async fn scenario_subscribe_streams_and_closes(b: Arc<dyn SessionBackend>) {
    // Sleep first so the marker is emitted AFTER subscription —
    // exercises the live stream, not a race against pre-subscribe
    // scrollback. (Catching pre-subscribe output is a pipe-pane
    // refactor; orthogonal concern.)
    let key = b
        .spawn(
            &shell_argv("sleep 1; printf 'pilot-marker'; sleep 1"),
            None,
            &[],
        )
        .await
        .unwrap();
    let mut sub = b.subscribe(&key).await.unwrap();
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if String::from_utf8_lossy(&got).contains("pilot-marker") {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(200), sub.live.recv()).await {
            Ok(Some(chunk)) => got.extend_from_slice(&chunk.bytes),
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    let _ = b.kill(&key).await;
    let s = String::from_utf8_lossy(&got);
    assert!(
        s.contains("pilot-marker"),
        "expected marker in output, got {s:?}"
    );
}

async fn scenario_write_unknown_returns_not_found(b: Arc<dyn SessionBackend>) {
    let err = b.write("does-not-exist", b"hi").await.unwrap_err();
    assert!(
        format!("{err}").contains("not found"),
        "expected NotFound, got {err}"
    );
}

async fn scenario_kill_is_idempotent(b: Arc<dyn SessionBackend>) {
    let key = b.spawn(&shell_argv("sleep 5"), None, &[]).await.unwrap();
    b.kill(&key).await.unwrap();
    // Callers may not know the first kill already happened (race
    // between user input and natural exit) — second kill must succeed.
    b.kill(&key).await.unwrap();
}

async fn scenario_wait_exit_caches(b: Arc<dyn SessionBackend>) {
    // The pump task in spawn_handler calls wait_exit once; tests +
    // future code may call again. Trait contract: cached, repeatable.
    let key = b.spawn(&shell_argv("exit 7"), None, &[]).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), b.wait_exit(&key))
        .await
        .expect("first wait_exit completes");
    let second = tokio::time::timeout(Duration::from_secs(1), b.wait_exit(&key))
        .await
        .expect("second wait_exit completes");
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------
// RawPtyBackend
// ---------------------------------------------------------------------

fn raw_pty() -> Arc<dyn SessionBackend> {
    Arc::new(RawPtyBackend::new())
}

#[tokio::test]
async fn raw_pty_spawn_returns_unique_keys() {
    scenario_spawn_returns_unique_keys(raw_pty()).await;
}

#[tokio::test]
async fn raw_pty_list_reflects_live_sessions() {
    scenario_list_reflects_live_sessions(raw_pty()).await;
}

#[tokio::test]
async fn raw_pty_subscribe_streams_and_closes() {
    scenario_subscribe_streams_and_closes(raw_pty()).await;
}

#[tokio::test]
async fn raw_pty_write_unknown_returns_not_found() {
    scenario_write_unknown_returns_not_found(raw_pty()).await;
}

#[tokio::test]
async fn raw_pty_kill_is_idempotent() {
    scenario_kill_is_idempotent(raw_pty()).await;
}

#[tokio::test]
async fn raw_pty_wait_exit_caches() {
    scenario_wait_exit_caches(raw_pty()).await;
}

#[tokio::test]
async fn raw_pty_id_is_stable() {
    let b = RawPtyBackend::new();
    assert_eq!(b.id(), "raw-pty");
}

// ---------------------------------------------------------------------
// TmuxBackend
//
// Each test allocates a unique socket so concurrent runs don't share
// state. If tmux isn't on PATH, the test logs and skips — keeps CI
// portable to platforms where tmux is unavailable.
// ---------------------------------------------------------------------

fn tmux_for_test(label: &str) -> Option<Arc<dyn SessionBackend>> {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let socket = format!("pilot-test-{label}-{}", std::process::id());
        Some(Arc::new(TmuxBackend::with_socket(&socket).unwrap()))
    } else {
        eprintln!("skipping tmux backend test: tmux not on PATH");
        None
    }
}

#[tokio::test]
async fn tmux_spawn_returns_unique_keys() {
    if let Some(b) = tmux_for_test("spawnu") {
        scenario_spawn_returns_unique_keys(b).await;
    }
}

#[tokio::test]
async fn tmux_list_reflects_live_sessions() {
    if let Some(b) = tmux_for_test("list") {
        scenario_list_reflects_live_sessions(b).await;
    }
}

#[tokio::test]
async fn tmux_subscribe_streams_and_closes() {
    if let Some(b) = tmux_for_test("subscribe") {
        scenario_subscribe_streams_and_closes(b).await;
    }
}

#[tokio::test]
async fn tmux_write_unknown_returns_not_found() {
    if let Some(b) = tmux_for_test("writenf") {
        scenario_write_unknown_returns_not_found(b).await;
    }
}

#[tokio::test]
async fn tmux_kill_is_idempotent() {
    if let Some(b) = tmux_for_test("kill") {
        scenario_kill_is_idempotent(b).await;
    }
}

#[tokio::test]
async fn tmux_id_is_stable() {
    if let Some(b) = TmuxBackend::detect() {
        assert_eq!(b.id(), "tmux");
    }
}
