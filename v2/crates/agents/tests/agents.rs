//! Tests for `Agent` impls and `Registry`. These lock in the argv each
//! built-in uses so a rename or flag change is caught immediately.
//! Generic CLI gets its own block — it's the extensibility surface
//! users will drive from YAML.

use pilot_v2_agents::agent::builtins::{Claude, Codex, Cursor, GenericCli};
use pilot_v2_agents::{Agent, AgentState, Registry, SessionWrapper, SpawnCtx};
use std::collections::HashMap;
use std::path::PathBuf;

fn sample_ctx() -> SpawnCtx {
    SpawnCtx {
        session_key: "github:o/r#1".into(),
        worktree: PathBuf::from("/tmp/wt"),
        repo: Some("o/r".into()),
        pr_number: Some("1".into()),
        env: HashMap::new(),
    }
}

#[test]
fn registry_has_expected_builtins() {
    let r = Registry::default_builtins();
    assert!(r.get("claude").is_some(), "claude agent registered");
    assert!(r.get("codex").is_some(), "codex agent registered");
    assert!(r.get("cursor-agent").is_some(), "cursor-agent registered");
    assert!(r.get("does-not-exist").is_none(), "unknown returns None");
}

#[test]
fn claude_spawn_and_resume_argv() {
    let agent = Claude;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["claude".to_string()]);
    assert_eq!(
        agent.resume(&ctx),
        vec!["claude".to_string(), "--continue".to_string()],
        "resume must use --continue so the previous conversation is picked up"
    );
}

#[test]
fn codex_argv() {
    let agent = Codex;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["codex".to_string()]);
    // Default trait impl: resume == spawn when the agent doesn't
    // override. Codex has no --continue flag today.
    assert_eq!(agent.resume(&ctx), agent.spawn(&ctx));
}

#[test]
fn cursor_argv() {
    let agent = Cursor;
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["cursor-agent".to_string()]);
}

#[test]
fn inject_prompt_appends_newline() {
    let agent = Claude;
    assert_eq!(agent.inject_prompt("hi"), b"hi\n");
    assert_eq!(agent.inject_prompt(""), b"\n");
    assert_eq!(agent.inject_prompt("multi\nline"), b"multi\nline\n");
}

#[test]
fn default_detect_state_returns_none() {
    // Base trait default — agents without hooks or detectors shouldn't
    // lie and return a state; returning None lets the daemon fall back
    // cleanly.
    let agent = Codex;
    assert_eq!(agent.detect_state(b"whatever"), None);
}

#[test]
fn default_hooks_is_none() {
    let agent = Codex;
    assert!(agent.hooks().is_none());
}

#[test]
fn generic_cli_spawn_and_resume() {
    let agent = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom-bin".into(), "--start".into()],
        resume_cmd: Some(vec!["custom-bin".into(), "--resume".into()]),
        asking_patterns: vec![],
    };
    let ctx = sample_ctx();
    assert_eq!(agent.spawn(&ctx), vec!["custom-bin", "--start"]);
    assert_eq!(agent.resume(&ctx), vec!["custom-bin", "--resume"]);
}

#[test]
fn generic_cli_resume_defaults_to_spawn() {
    let agent = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom".into()],
        resume_cmd: None,
        asking_patterns: vec![],
    };
    let ctx = sample_ctx();
    assert_eq!(agent.resume(&ctx), agent.spawn(&ctx));
}

#[test]
fn generic_cli_asking_pattern_matching() {
    let agent = GenericCli {
        id: "custom",
        display_name: "Custom",
        spawn_cmd: vec!["custom".into()],
        resume_cmd: None,
        asking_patterns: vec!["Press Enter to continue".into(), "[y/N]".into()],
    };
    assert_eq!(
        agent.detect_state(b"Some output... Press Enter to continue\n"),
        Some(AgentState::Asking)
    );
    assert_eq!(
        agent.detect_state(b"Install all? [y/N]"),
        Some(AgentState::Asking)
    );
    assert_eq!(agent.detect_state(b"just normal output"), None);
}

#[test]
fn generic_cli_empty_patterns_returns_none() {
    // Empty patterns = "no opinion"; must return None (not Asking!)
    let agent = GenericCli {
        id: "x",
        display_name: "x",
        spawn_cmd: vec!["x".into()],
        resume_cmd: None,
        asking_patterns: vec![],
    };
    assert_eq!(agent.detect_state(b"anything"), None);
}

// ── SessionWrapper tests ───────────────────────────────────────────────

#[test]
fn tmux_wrap_shape() {
    use pilot_v2_agents::TmuxWrapper;
    let w = TmuxWrapper::new();
    let argv = w.wrap(
        "github:o/r#1",
        &["claude".to_string(), "--continue".to_string()],
        std::path::Path::new("/tmp/wt"),
    );
    assert_eq!(argv[0], "tmux");
    assert_eq!(argv[1], "new-session");
    assert_eq!(argv[2], "-A", "-A makes tmux attach if session exists");
    assert_eq!(argv[3], "-s");
    assert_eq!(
        argv[4], "github_o_r#1",
        "session id must be sanitized — colons and slashes become underscores"
    );
    assert_eq!(
        argv[5], "claude --continue",
        "inner command is joined into one string for tmux"
    );
}

#[test]
fn tmux_sanitize_id_replaces_reserved_chars() {
    use pilot_v2_agents::TmuxWrapper;
    let w = TmuxWrapper::new();
    assert_eq!(w.sanitize_id("a:b/c"), "a_b_c");
    assert_eq!(w.sanitize_id("simple"), "simple");
    assert_eq!(w.sanitize_id("deep/nested:key#1"), "deep_nested_key#1");
}

#[test]
fn raw_wrapper_returns_inner_unchanged() {
    use pilot_v2_agents::session_wrapper::RawWrapper;
    let w = RawWrapper;
    let inner = vec!["bash".to_string(), "-c".to_string(), "echo x".to_string()];
    assert_eq!(
        w.wrap("any-key", &inner, std::path::Path::new("/")),
        inner,
        "RawWrapper must not modify the argv"
    );
    assert!(w.list_sessions().is_empty(), "raw has no session registry");
    assert!(w.kill("anything").is_ok(), "raw kill is always Ok");
}
