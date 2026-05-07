//! PTY lifecycle: spawn, stream, write, resize, exit.
//!
//! These tests spawn real child processes (echo, cat, sh). They're
//! still fast — each test completes in well under a second — but they
//! are NOT pure, so we keep them in `tests/` so they don't run on
//! every `cargo check --lib`.

use pilot_server::pty::{DaemonPty, OutputChunk, PtyError};
use portable_pty::PtySize;
use std::time::Duration;
use tokio::sync::broadcast;

fn default_size() -> PtySize {
    PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Helper: drain a live stream until we either see the exit marker
/// or we've collected at least `want` bytes total. Returns everything
/// accumulated. Breaks on channel close (no more senders).
async fn collect_until_bytes(mut rx: broadcast::Receiver<OutputChunk>, want: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if out.len() >= want {
            break;
        }
        let remaining = match deadline.checked_duration_since(tokio::time::Instant::now()) {
            Some(d) => d,
            None => break,
        };
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(chunk)) => out.extend_from_slice(&chunk.bytes),
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Err(_) => break, // timed out
        }
    }
    out
}

#[tokio::test]
async fn echo_produces_output_then_exits() {
    let pty = DaemonPty::spawn(
        &["echo".to_string(), "hello-pilot".to_string()],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn echo");

    // Wait for the child to finish so the ring is populated
    // deterministically. Timeout is generous — tests run in parallel
    // on CI, and echo-spawn+exit under concurrency can take a beat.
    let code = tokio::time::timeout(Duration::from_secs(10), pty.wait_exit())
        .await
        .expect("exit within timeout");
    assert_eq!(code, Some(0), "echo should exit 0");
    assert!(pty.is_finished());

    // Poll the ring until output shows up. The reader thread writes
    // asynchronously; under load there can be a 100-200ms gap between
    // wait_exit returning and the ring being flushed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let sub = pty.subscribe().await;
        let text = String::from_utf8_lossy(&sub.replay).into_owned();
        if text.contains("hello-pilot") {
            assert!(sub.last_seq > 0);
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("expected 'hello-pilot' in replay after 3s, got {text:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn write_is_echoed_by_cat() {
    // `cat` with no args echoes stdin → stdout. We write "ping\n" and
    // expect to see it come back in the output stream.
    let pty =
        DaemonPty::spawn(&["cat".to_string()], default_size(), None, vec![]).expect("spawn cat");

    let sub = pty.subscribe().await;
    pty.write(b"ping\n").await.expect("write to PTY");

    // `cat` with a PTY runs in cooked/line mode so we expect to see
    // our input echoed once via the line discipline AND then the
    // cat-produced copy. Accept either, just look for the word.
    let output = collect_until_bytes(sub.live, 5).await;
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("ping"),
        "expected 'ping' in output, got {text:?}"
    );

    // Send EOF (^D) so cat exits cleanly.
    pty.write(&[0x04]).await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), pty.wait_exit()).await;
}

#[tokio::test]
async fn write_after_finish_errors() {
    let pty =
        DaemonPty::spawn(&["true".to_string()], default_size(), None, vec![]).expect("spawn true");

    let _ = tokio::time::timeout(Duration::from_secs(5), pty.wait_exit()).await;

    // Give the reader thread a moment to observe EOF and flip the
    // finished flag. On fast systems this is instant; on CI sometimes
    // we need one tick.
    for _ in 0..50 {
        if pty.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let err = pty.write(b"x").await.expect_err("write post-finish errors");
    assert!(matches!(err, PtyError::Closed));
}

#[tokio::test]
async fn resize_does_not_crash_running_pty() {
    // Just verify the call doesn't blow up. Verifying the child saw
    // the new size would require SIGWINCH handling in the child.
    let pty = DaemonPty::spawn(
        &["sleep".to_string(), "5".to_string()],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn sleep");

    pty.resize(PtySize {
        rows: 40,
        cols: 132,
        pixel_width: 0,
        pixel_height: 0,
    })
    .await
    .expect("resize succeeds");

    // Clean up the sleep — we don't want the test hanging for 5s.
    pty.write(&[0x03]).await.ok(); // ^C
    let _ = tokio::time::timeout(Duration::from_secs(3), pty.wait_exit()).await;
}

#[tokio::test]
async fn subscription_sees_bytes_from_ring_on_late_subscribe() {
    // Subscribing AFTER the child has produced output — the Snapshot
    // contract says we get the replay. This is the foundation for
    // reconnect.
    let pty = DaemonPty::spawn(
        &["echo".to_string(), "late-hello".to_string()],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn echo");

    // Wait for exit so all bytes are guaranteed in the ring.
    let _ = tokio::time::timeout(Duration::from_secs(5), pty.wait_exit()).await;
    // Extra tick to ensure the reader thread finished writing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let sub = pty.subscribe().await;
    let text = String::from_utf8_lossy(&sub.replay);
    assert!(
        text.contains("late-hello"),
        "replay should contain 'late-hello', got {text:?}"
    );
    assert!(sub.last_seq > 0, "at least one chunk was recorded");
}

#[tokio::test]
async fn nonexistent_command_errors_cleanly() {
    // `DaemonPty` doesn't implement Debug (holds Mutex<Box<dyn Write>>
    // etc.), so we can't use `expect_err` — match manually.
    match DaemonPty::spawn(
        &["pilot-definitely-not-a-real-binary-xyz".to_string()],
        default_size(),
        None,
        vec![],
    ) {
        Ok(_) => panic!("spawn of missing binary should have errored"),
        Err(e) => assert!(
            matches!(e, PtyError::Open(_) | PtyError::Spawn(_)),
            "expected Open or Spawn error, got {e:?}"
        ),
    }
}

#[tokio::test]
async fn spawn_with_empty_cmd_is_an_error() {
    match DaemonPty::spawn(&[], default_size(), None, vec![]) {
        Ok(_) => panic!("empty cmd should have errored"),
        Err(e) => assert!(matches!(e, PtyError::Spawn(_))),
    }
}
