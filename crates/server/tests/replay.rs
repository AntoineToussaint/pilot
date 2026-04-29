//! Reconnect replay tests: simulate a client subscribing, dropping,
//! and re-subscribing mid-stream. The second subscription must see
//! the replay buffer AND pick up live bytes without duplicating what
//! the replay already covers (client is responsible for gap-checking
//! via seq).

use pilot_v2_server::pty::{DaemonPty, OutputChunk};
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

/// Collect until we've seen `want` bytes OR the stream ends.
async fn drain(mut rx: broadcast::Receiver<OutputChunk>, want: usize) -> (Vec<u8>, u64) {
    let mut out = Vec::new();
    let mut last_seq = 0u64;
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
            Ok(Ok(chunk)) => {
                last_seq = chunk.seq;
                out.extend_from_slice(&chunk.bytes);
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Err(_) => break,
        }
    }
    (out, last_seq)
}

/// Baseline: subscription sees the first bytes of a running process.
#[tokio::test]
async fn first_subscription_sees_output() {
    let pty = DaemonPty::spawn(
        &[
            "sh".into(),
            "-c".into(),
            "printf first-round; sleep 0.2; printf second-round".into(),
        ],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn sh");

    let sub = pty.subscribe().await;
    let (bytes, last_seq) = drain(sub.live, 20).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("first-round"), "got {text:?}");
    assert!(last_seq > 0, "at least one chunk recorded");

    let _ = tokio::time::timeout(Duration::from_secs(3), pty.wait_exit()).await;
}

/// Reconnect: subscribe, then drop, then subscribe again. Second
/// subscription's replay must contain everything previously observed.
/// Sequence numbers must be strictly monotonic across the break.
#[tokio::test]
async fn reconnect_replay_contains_prior_bytes() {
    let pty = DaemonPty::spawn(
        &[
            "sh".into(),
            "-c".into(),
            "printf batch-one; sleep 0.15; printf batch-two".into(),
        ],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn sh");

    // First subscription — drain batch-one, then disconnect.
    {
        let sub = pty.subscribe().await;
        let (bytes, _) = drain(sub.live, 9).await;
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("batch-one"), "first sub got {text:?}");
        // Dropping the receiver simulates a client disconnect.
    }

    // Wait for the rest of the output to land.
    let _ = tokio::time::timeout(Duration::from_secs(3), pty.wait_exit()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Reconnect — replay must cover everything the command produced
    // because the ring is big enough.
    let sub = pty.subscribe().await;
    let replay = String::from_utf8_lossy(&sub.replay);
    assert!(
        replay.contains("batch-one") && replay.contains("batch-two"),
        "replay should contain both batches, got {replay:?}"
    );
    assert!(sub.last_seq > 0, "last_seq carried over from prior chunks");
}

/// Sequence numbers are monotonic and never reused across subscribers.
/// This is the invariant a client relies on to detect gaps.
#[tokio::test]
async fn sequence_numbers_are_monotonic_across_subscribers() {
    let pty = DaemonPty::spawn(
        &[
            "sh".into(),
            "-c".into(),
            // Emit a sequence of tagged chunks with small sleeps so they
            // land in separate PTY reads. Each sleep is tiny so the
            // test still finishes quickly.
            "for i in 1 2 3 4 5; do printf 'tick-%d-' $i; sleep 0.03; done".into(),
        ],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn sh");

    // Subscriber A — collects early chunks, tracks max seq.
    let sub_a = pty.subscribe().await;
    let (_, seq_a_final) = drain(sub_a.live, 10).await;

    // Subscriber B — joins late; we compare its first seq to A's last.
    let mut sub_b = pty.subscribe().await;
    let mut seq_b_first = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match tokio::time::timeout(remaining, sub_b.live.recv()).await {
            Ok(Ok(chunk)) => {
                seq_b_first = Some(chunk.seq);
                break;
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), pty.wait_exit()).await;

    let seq_b_first = seq_b_first.expect("subscriber B saw at least one chunk");
    assert!(
        seq_b_first > seq_a_final,
        "expected seq_b_first ({seq_b_first}) > seq_a_final ({seq_a_final}) — seq must never repeat"
    );
}

/// A ring-buffer capped at 64 KiB means a very long-running process
/// eventually loses the early bytes. Verify `replay` has a ceiling.
#[tokio::test]
async fn ring_replay_stays_bounded_under_long_output() {
    use pilot_v2_server::pty::REPLAY_RING_BYTES;

    let pty = DaemonPty::spawn(
        &[
            "sh".into(),
            "-c".into(),
            // ~256 KiB of output so the ring definitely wraps.
            "yes x | head -c 262144".into(),
        ],
        default_size(),
        None,
        vec![],
    )
    .expect("spawn yes|head");

    let _ = tokio::time::timeout(Duration::from_secs(5), pty.wait_exit()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let sub = pty.subscribe().await;
    assert!(
        sub.replay.len() <= REPLAY_RING_BYTES,
        "replay {} should be <= ring capacity {}",
        sub.replay.len(),
        REPLAY_RING_BYTES
    );
    // And we wrote more than the ring capacity, so we MUST have lost bytes.
    assert!(
        pty.total_written().await > REPLAY_RING_BYTES as u64,
        "we fed more than the ring capacity"
    );
}
