//! TerminalStack tests: event-driven state machine, tab management,
//! key → Write routing, ANSI strip, render.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::SessionKey;
use pilot_v2_ipc::{Command, Event, TerminalId, TerminalKind, TerminalSnapshot};
use pilot_v2_tui::components::TerminalStack;
use pilot_v2_tui::components::terminal_stack::{MAX_PER_TERM_BYTES, strip_ansi};
use pilot_v2_tui::{Component, ComponentId, Outcome};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn sk(s: &str) -> SessionKey {
    s.into()
}

fn spawned(id: u64, session: &str, kind: TerminalKind) -> Event {
    Event::TerminalSpawned {
        terminal_id: TerminalId(id),
        session_key: sk(session),
        kind,
    }
}

// ── Event-driven state ─────────────────────────────────────────────────

#[test]
fn spawn_event_creates_slot() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    assert_eq!(t.terminal_count(), 1);
}

#[test]
fn terminals_filtered_by_active_session() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    assert_eq!(t.visible_terminals().len(), 1);
    assert_eq!(t.visible_terminals()[0], TerminalId(1));

    t.set_active_session(Some(sk("o/r#2")));
    assert_eq!(t.visible_terminals().len(), 1);
    assert_eq!(t.visible_terminals()[0], TerminalId(2));

    t.set_active_session(None);
    assert!(t.visible_terminals().is_empty());
}

#[test]
fn output_event_appends_to_content() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"hello world\n".to_vec(),
        seq: 1,
    });
    let content = t.active_content().unwrap();
    assert_eq!(content, b"hello world\n");
}

#[test]
fn output_for_unknown_terminal_is_dropped() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    // No spawn — output arrives for a terminal we don't know about.
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(999),
        bytes: b"nobody home".to_vec(),
        seq: 1,
    });
    assert_eq!(t.terminal_count(), 0);
}

#[test]
fn output_strips_ansi_before_storing() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"\x1b[31mred\x1b[0m text".to_vec(),
        seq: 1,
    });
    assert_eq!(t.active_content().unwrap(), b"red text");
}

#[test]
fn exit_event_marks_terminal_dead_but_keeps_slot() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&Event::TerminalExited {
        terminal_id: TerminalId(1),
        exit_code: Some(0),
    });
    // Slot stays so the user can still scroll its final output.
    assert_eq!(t.terminal_count(), 1);
}

#[test]
fn content_is_capped() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    // Push more than MAX_PER_TERM_BYTES in small chunks.
    let chunk = vec![b'A'; 4096];
    for _ in 0..10 {
        t.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: chunk.clone(),
            seq: 1,
        });
    }
    let content = t.active_content().unwrap();
    assert!(
        content.len() <= MAX_PER_TERM_BYTES,
        "content {} must be capped at {}",
        content.len(),
        MAX_PER_TERM_BYTES
    );
    // Last bytes are preserved (tail semantics).
    assert!(content.iter().all(|b| *b == b'A'));
}

#[test]
fn session_removed_prunes_all_its_terminals() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(3, "o/r#2", TerminalKind::Shell));
    t.on_event(&Event::SessionRemoved(sk("o/r#1")));
    assert_eq!(t.terminal_count(), 1, "only o/r#2's terminal remains");
}

#[test]
fn snapshot_replaces_all_terminals() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    // Snapshot arrives with different set — prior gets wiped.
    t.on_event(&Event::Snapshot {
        sessions: vec![],
        terminals: vec![TerminalSnapshot {
            terminal_id: TerminalId(99),
            session_key: sk("o/r#3"),
            kind: TerminalKind::Shell,
            replay: b"\x1b[0mhi\n".to_vec(),
            last_seq: 42,
        }],
    });
    assert_eq!(t.terminal_count(), 1);
    t.set_active_session(Some(sk("o/r#3")));
    assert_eq!(t.active_content().unwrap(), b"hi\n");
}

// ── Tab navigation ─────────────────────────────────────────────────────

#[test]
fn tab_idx_starts_at_zero() {
    let t = TerminalStack::new(ComponentId::new(1));
    assert_eq!(t.active_tab_idx(), 0);
}

#[test]
fn cycle_tab_forward_wraps() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    for i in 1..=3 {
        t.on_event(&spawned(i, "o/r#1", TerminalKind::Shell));
    }
    t.set_active_session(Some(sk("o/r#1")));
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 1);
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 2);
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 0, "wraps");
}

#[test]
fn cycle_tab_backward_wraps() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    for i in 1..=3 {
        t.on_event(&spawned(i, "o/r#1", TerminalKind::Shell));
    }
    t.set_active_session(Some(sk("o/r#1")));
    t.cycle_tab_backward();
    assert_eq!(t.active_tab_idx(), 2, "wraps to end");
}

#[test]
fn set_active_session_resets_tab_idx() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(3, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 1);

    t.set_active_session(Some(sk("o/r#2")));
    assert_eq!(t.active_tab_idx(), 0, "reset on session change");
}

// ── Key routing ────────────────────────────────────────────────────────

#[test]
fn char_key_emits_write_to_active_terminal() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(42, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    let outcome = t.handle_key(ch('a'), &mut cmds);
    assert_eq!(outcome, Outcome::Consumed);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Write { terminal_id, bytes } => {
            assert_eq!(*terminal_id, TerminalId(42));
            assert_eq!(bytes, b"a");
        }
        other => panic!("expected Write, got {other:?}"),
    }
}

#[test]
fn enter_emits_cr_to_terminal() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    match &cmds[0] {
        Command::Write { bytes, .. } => assert_eq!(bytes, b"\r"),
        _ => panic!(),
    }
}

#[test]
fn shift_enter_emits_alt_enter() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    t.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut cmds,
    );
    match &cmds[0] {
        Command::Write { bytes, .. } => assert_eq!(bytes, b"\x1b\r"),
        _ => panic!(),
    }
}

#[test]
fn ctrl_letter_emits_control_byte() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    t.handle_key(ctrl('a'), &mut cmds);
    match &cmds[0] {
        Command::Write { bytes, .. } => assert_eq!(bytes, &[0x01]),
        _ => panic!(),
    }
}

#[test]
fn ctrl_bracket_bubbles_up_to_exit_terminal_mode() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ctrl(']'), &mut cmds);
    assert_eq!(outcome, Outcome::BubbleUp);
    assert!(cmds.is_empty(), "Ctrl-] must NOT write to the PTY");
}

#[test]
fn ctrl_o_bubbles_up_too() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ctrl('o'), &mut cmds);
    assert_eq!(outcome, Outcome::BubbleUp);
}

#[test]
fn tab_emits_focus_next() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(code(KeyCode::Tab), &mut cmds);
    assert_eq!(outcome, Outcome::FocusNext);
}

#[test]
fn keys_without_active_terminal_bubble_up() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    // No spawned terminals for the active session.
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ch('x'), &mut cmds);
    assert_eq!(outcome, Outcome::BubbleUp);
    assert!(cmds.is_empty());
}

#[test]
fn keys_to_dead_terminal_bubble_up() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&Event::TerminalExited {
        terminal_id: TerminalId(1),
        exit_code: Some(0),
    });
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ch('x'), &mut cmds);
    assert_eq!(outcome, Outcome::BubbleUp);
}

// ── ANSI strip ─────────────────────────────────────────────────────────

#[test]
fn strip_ansi_removes_csi() {
    assert_eq!(strip_ansi(b"\x1b[31mred\x1b[0m"), b"red");
    assert_eq!(strip_ansi(b"\x1b[1;32;40mmulti\x1b[m"), b"multi");
}

#[test]
fn strip_ansi_removes_osc() {
    // OSC terminated by BEL.
    assert_eq!(strip_ansi(b"before\x1b]0;title\x07after"), b"beforeafter");
    // OSC terminated by ST (ESC \).
    assert_eq!(strip_ansi(b"x\x1b]0;title\x1b\\y"), b"xy");
}

#[test]
fn strip_ansi_drops_bell() {
    assert_eq!(strip_ansi(b"ding\x07dong"), b"dingdong");
}

#[test]
fn strip_ansi_preserves_newlines_and_utf8() {
    assert_eq!(strip_ansi(b"line1\nline2\r\n"), b"line1\nline2\r\n");
    // "é" in UTF-8 is C3 A9 — both bytes should survive.
    assert_eq!(strip_ansi("café".as_bytes()), "café".as_bytes());
}

#[test]
fn strip_ansi_handles_stray_esc_at_end() {
    // ESC at end of buffer — no panic.
    let input = b"text\x1b";
    let out = strip_ansi(input);
    // Either "text" or "text\x1b"; not crashing is the contract.
    assert!(out.starts_with(b"text"));
}

// ── Render ─────────────────────────────────────────────────────────────

fn render_to_string(t: &mut TerminalStack, w: u16, h: u16, focused: bool) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| t.render(Rect::new(0, 0, w, h), f, focused))
        .unwrap();
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn render_empty_shows_placeholder() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        out.contains("no terminals"),
        "empty state visible; got:\n{out}"
    );
}

#[test]
fn render_shows_tab_bar_and_content() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"first line\nsecond line\n".to_vec(),
        seq: 1,
    });

    let out = render_to_string(&mut t, 60, 10, true);
    assert!(out.contains("claude"), "first tab label; got:\n{out}");
    assert!(out.contains("shell"), "second tab label; got:\n{out}");
    assert!(
        out.contains("first line") && out.contains("second line"),
        "active terminal content; got:\n{out}"
    );
}

#[test]
fn render_tab_bar_updates_after_cycle() {
    let mut t = TerminalStack::new(ComponentId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"AGENT_OUTPUT".to_vec(),
        seq: 1,
    });
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(2),
        bytes: b"SHELL_OUTPUT".to_vec(),
        seq: 1,
    });

    let out_before = render_to_string(&mut t, 60, 10, true);
    assert!(out_before.contains("AGENT_OUTPUT"));
    assert!(!out_before.contains("SHELL_OUTPUT"));

    t.cycle_tab_forward();
    let out_after = render_to_string(&mut t, 60, 10, true);
    assert!(out_after.contains("SHELL_OUTPUT"));
    assert!(!out_after.contains("AGENT_OUTPUT"));
}
