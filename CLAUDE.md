# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is pilot?

A reactive PR inbox TUI. Instead of checking GitHub, events flow to you — new comments, CI failures, review requests surface automatically with read/unread tracking. Each task becomes a session with an embedded terminal for running Claude Code or a shell in a git worktree.

Source-agnostic: GitHub is one provider, but Linear/Jira/etc. plug in the same way.

## Build & Run

```bash
cargo build                    # build (first build compiles SQLite, takes ~30s)
cargo run -p pilot          # run (uses `gh auth token` automatically)
cargo test --workspace         # tests
cargo clippy --workspace       # lint
```

Logs go to `/tmp/pilot.log`. State persisted in `~/.pilot/state.db`.

## Architecture

10 crates. The reusable library crates must NEVER depend on each other.

```
crates/
  core/          # Task, Session, Activity, time helpers. Source-agnostic types.
  auth/          # CredentialProvider trait + chain. Env, Command, Static providers.
  events/        # Event bus (broadcast channel). EventKind enum.
  store/         # Store trait + SQLite backend. Persists full session + read/unread state.
  config/        # YAML config loading from ~/.pilot/config.yaml.
  tui-term/      # Embedded terminal: portable-pty + vt100 + tui-term widget. Scrollback support.
  gh-provider/   # GitHub provider: octocrab polling → generic Events. Needs-reply detection.
  git-ops/       # Git worktree manager (bare clones + worktrees).
  mcp-server/    # MCP server binary (stdio). Claude Code calls this for push/reply/merge/approve.
  app/           # TUI binary. Event loop, pane system, tabs, search, confirmation prompts.
```

### Key patterns

- **Action pattern**: All inputs → `Action` enum → single mpsc channel → drain-then-render.
- **Event bus**: `tokio::sync::broadcast`. Providers produce, app consumes.
- **Credential chain**: `EnvProvider("GH_TOKEN") → EnvProvider("GITHUB_TOKEN") → CommandProvider("gh auth token")`. Trait-based, extensible (Vault, Keychain, OAuth).
- **Store**: `Store` trait with `SqliteStore` backend. Read/unread state persists across sessions.
- **Terminal**: PTY reader on std::thread. vt100 Parser behind Mutex. 100ms tick redraws. Auto-resize.
- **Markdown**: PR descriptions rendered via `tui-markdown` (pulldown-cmark + syntect).
- **MCP integration**: `pilot-mcp-server` runs as stdio MCP server. When spawning Claude, pilot writes `.mcp.json` in the worktree. Claude calls `pilot_push`, `pilot_reply`, `pilot_merge` etc. instead of running git/gh directly. Pilot shows y/n confirmation modal. IPC via `~/.pilot/ipc/` files.

### Adding a new provider

1. Create `crates/foo-provider/` depending on `pilot-core` + `pilot-events` + `pilot-auth`
2. Build a credential chain for auth
3. Implement client returning `Vec<Task>` + poller emitting `Event`s
4. Wire in `app.rs` alongside the GitHub poller

### Adding a new auth source

Implement `CredentialProvider` trait with `name()` and `async resolve(scope) → Credential`.
Add to the chain in `app.rs`.

### Adding a new storage backend

Implement `Store` trait (get/save/mark_read/list/delete session records).
Swap in `app.rs` instead of `SqliteStore`.

## Keybindings

**Sidebar**: j/k navigate, Enter/c Claude Code, b shell, d detail, m mark read, Tab switch, q quit
**Detail**: j/k scroll, c Claude, b shell, Tab/Esc back
**Terminal**: all keys → PTY, Ctrl-] → sidebar, Ctrl-d → detail

## Conventions

- `thiserror` for errors in library crates, `anyhow` in app
- No `unwrap()` in library crates
- Core 4 libraries (core, auth, events, store) must not depend on each other
- Provider crates depend on core + events + auth only
