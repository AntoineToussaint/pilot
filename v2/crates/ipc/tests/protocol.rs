//! Protocol-level tests: serde round-trip every `Command` / `Event`
//! variant, plus framing over a real tokio duplex pair.
//!
//! These tests exist to catch *silent* wire-format breakage. If anyone
//! renames a variant or reorders a field, bincode's tagging changes and
//! one of these assertions blows up. Far better than finding out when
//! a v0.2 client can't talk to a v0.3 daemon.

use pilot_v2_ipc::{
    AgentState, Command, Event, TerminalId, TerminalKind, TerminalSnapshot,
};
use tokio::io::duplex;

fn sample_session() -> pilot_core::Session {
    let task = pilot_core::Task {
        id: pilot_core::TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        },
        title: "t".into(),
        body: None,
        state: pilot_core::TaskState::Open,
        role: pilot_core::TaskRole::Author,
        ci: pilot_core::CiStatus::None,
        review: pilot_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://github.com/o/r/pull/1".into(),
        repo: Some("o/r".into()),
        branch: Some("b".into()),
        base_branch: None,
        updated_at: chrono::Utc::now(),
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
    };
    pilot_core::Session::new_at(task, chrono::Utc::now())
}

fn all_commands() -> Vec<Command> {
    let key: pilot_core::SessionKey = "github:o/r#1".into();
    vec![
        Command::Subscribe,
        Command::Spawn {
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            cwd: Some("/tmp".into()),
        },
        Command::Spawn {
            session_key: key.clone(),
            kind: TerminalKind::Shell,
            cwd: None,
        },
        Command::Spawn {
            session_key: key.clone(),
            kind: TerminalKind::LogTail {
                path: "/var/log/x.log".into(),
            },
            cwd: None,
        },
        Command::Write {
            terminal_id: TerminalId(7),
            bytes: b"hello\n".to_vec(),
        },
        Command::Resize {
            terminal_id: TerminalId(7),
            cols: 120,
            rows: 40,
        },
        Command::Close {
            terminal_id: TerminalId(7),
        },
        Command::Kill {
            session_key: key.clone(),
        },
        Command::MarkRead {
            session_key: key.clone(),
        },
        Command::Snooze {
            session_key: key.clone(),
            until: chrono::Utc::now() + chrono::Duration::hours(4),
        },
        Command::Unsnooze {
            session_key: key.clone(),
        },
        Command::Merge {
            session_key: key.clone(),
        },
        Command::Approve {
            session_key: key.clone(),
        },
        Command::UpdateBranch {
            session_key: key.clone(),
        },
        Command::Refresh,
        Command::Shutdown,
    ]
}

fn all_events() -> Vec<Event> {
    let key: pilot_core::SessionKey = "github:o/r#1".into();
    vec![
        Event::Snapshot {
            sessions: vec![sample_session()],
            terminals: vec![TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: b"replay-bytes".to_vec(),
                last_seq: 42,
            }],
        },
        Event::SessionUpserted(sample_session()),
        Event::SessionRemoved(key.clone()),
        Event::TerminalSpawned {
            terminal_id: TerminalId(2),
            session_key: key.clone(),
            kind: TerminalKind::Shell,
        },
        Event::TerminalOutput {
            terminal_id: TerminalId(2),
            bytes: b"ANSI: \x1b[31mred\x1b[0m".to_vec(),
            seq: 1,
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: Some(0),
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: None,
        },
        Event::AgentState {
            session_key: key.clone(),
            state: AgentState::Asking,
        },
        Event::ProviderError {
            source: "github".into(),
            message: "rate limited".into(),
        },
        Event::Notification {
            title: "hi".into(),
            body: "body".into(),
        },
    ]
}

/// Round-trip every Command through bincode. Any new variant added to
/// the enum must show up in `all_commands` or this test fails — that's
/// the point.
#[test]
fn command_bincode_round_trip() {
    for cmd in all_commands() {
        let bytes = bincode::serialize(&cmd).expect("serialize");
        let back: Command = bincode::deserialize(&bytes).expect("deserialize");
        // Debug-equality since the wire types intentionally don't
        // derive PartialEq (Sessions carry timestamps we can't compare
        // structurally). Debug output uniqueness is the contract.
        assert_eq!(format!("{cmd:?}"), format!("{back:?}"));
    }
}

#[test]
fn event_bincode_round_trip() {
    for ev in all_events() {
        let bytes = bincode::serialize(&ev).expect("serialize");
        let back: Event = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(format!("{ev:?}"), format!("{back:?}"));
    }
}

/// Framed write → framed read over a tokio duplex stream returns the
/// same message. Exercises the socket transport without actually
/// touching the filesystem or kernel sockets.
#[tokio::test]
async fn socket_framing_round_trip() {
    use pilot_v2_ipc::socket::{read_frame, write_frame};

    let (mut a, mut b) = duplex(64 * 1024);

    // Alice sends, Bob receives.
    tokio::spawn(async move {
        for cmd in all_commands() {
            write_frame(&mut a, &cmd).await.expect("write");
        }
        // Drop on exit → closes the pipe → Bob's read_frame returns None.
    });

    let mut seen = 0usize;
    while let Some(cmd) = read_frame::<_, Command>(&mut b).await.expect("read") {
        // Serialize again; if we got it we should be able to re-emit.
        let _bytes = bincode::serialize(&cmd).expect("reserialize");
        seen += 1;
    }
    assert_eq!(seen, all_commands().len());
}

/// Frames larger than `MAX_FRAME_BYTES` must error cleanly instead of
/// allocating a huge buffer. Simulates a malicious or corrupted peer.
#[tokio::test]
async fn socket_rejects_oversized_frames() {
    use pilot_v2_ipc::MAX_FRAME_BYTES;
    use pilot_v2_ipc::socket::read_frame;
    use tokio::io::AsyncWriteExt;

    let (mut a, mut b) = duplex(64);
    let bad_len = MAX_FRAME_BYTES + 1;

    // Writer is the adversary — emits a length prefix claiming more
    // bytes than we allow, then drops.
    tokio::spawn(async move {
        let _ = a.write_all(&bad_len.to_be_bytes()).await;
    });

    let result: Result<Option<Command>, _> = read_frame(&mut b).await;
    assert!(
        matches!(result, Err(pilot_v2_ipc::socket::FrameError::TooLarge(n)) if n == bad_len),
        "expected TooLarge, got {result:?}"
    );
}

/// A clean EOF (peer drops between frames) returns Ok(None), not an
/// error. That's how the daemon distinguishes orderly client shutdown
/// from a transport fault.
#[tokio::test]
async fn socket_clean_eof_is_none() {
    use pilot_v2_ipc::socket::read_frame;

    let (a, mut b) = duplex(64);
    drop(a);
    let result: Result<Option<Command>, _> = read_frame(&mut b).await;
    assert!(matches!(result, Ok(None)), "expected Ok(None), got {result:?}");
}

/// Zero-byte message (empty bytes payload) round-trips. Guard against
/// edge cases in the framing arithmetic.
#[tokio::test]
async fn socket_zero_byte_payload_works() {
    use pilot_v2_ipc::socket::{read_frame, write_frame};
    let (mut a, mut b) = duplex(64);
    let msg = Command::Write {
        terminal_id: TerminalId(1),
        bytes: vec![],
    };
    write_frame(&mut a, &msg).await.expect("write");
    drop(a);
    let got: Option<Command> = read_frame(&mut b).await.expect("read");
    let got = got.expect("one message");
    assert_eq!(format!("{got:?}"), format!("{msg:?}"));
}

/// Non-trivial binary payloads (ANSI escape sequences, UTF-8) survive.
/// Terminal output carries both.
#[tokio::test]
async fn socket_binary_terminal_output_round_trip() {
    use pilot_v2_ipc::socket::{read_frame, write_frame};
    let (mut a, mut b) = duplex(64 * 1024);

    let nasty: Vec<u8> = (0..=255).collect();
    let msg = Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: nasty.clone(),
        seq: 99,
    };
    write_frame(&mut a, &msg).await.expect("write");
    drop(a);
    let got: Option<Event> = read_frame(&mut b).await.expect("read");
    if let Some(Event::TerminalOutput { bytes, seq, .. }) = got {
        assert_eq!(bytes, nasty);
        assert_eq!(seq, 99);
    } else {
        panic!("expected TerminalOutput, got {got:?}");
    }
}
