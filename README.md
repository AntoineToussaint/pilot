# pilot

A reactive PR inbox TUI. Events flow to you — new comments, CI failures, review requests surface automatically. Each PR becomes a session with an embedded terminal (Claude Code or shell) in an isolated git worktree, wrapped in tmux so sessions survive quit.

## Install

### Prerequisites

- **Rust** 1.75+ — [rustup.rs](https://rustup.rs)
- **Zig** — `brew install zig` (macOS) or [ziglang.org](https://ziglang.org/download/)
- **GitHub CLI** — `brew install gh` then `gh auth login`
- **tmux** — `brew install tmux` (sessions persist across quit)
- **Claude Code** (optional) — for AI-assisted fix/reply workflows

### Build

```bash
git clone https://github.com/AntoineToussaint/pilot.git
git clone https://github.com/ghostty-org/libghostty-rs.git  # terminal emulator
cd pilot
make setup   # verify deps
make build   # first build ~2min (compiles Zig terminal + SQLite)
make run     # start pilot
```

## Quick Start

```
  #      Title                                   CI Rv ●  Time
  ── tensorzero (11) ────────────────────────────────────────
  #7302 refactor: decouple GCP provider...        ✓  ◦  ●  14m
  #7282 Add OTel GenAI fields to stored...        ✓  ✓      20h
  #41   Fix TOCTOU race in resolve_org...         ◦  ◦  ●    4h
```

1. `j`/`k` navigate — `Enter` opens detail
2. `f` fix (spawns Claude to fix CI/conflicts/comments)
3. `r` reply (post comment directly)
4. `M M` merge (double-press)
5. `Tab` cycle panes — `?` help

## Keybindings

### Sidebar

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate PRs |
| `Enter` | Open detail pane |
| `f` | Fix — spawns Claude for CI failures, conflicts, or comments |
| `r` | Reply to comment directly |
| `c` | Open Claude Code in worktree (tmux, survives quit) |
| `b` | Open shell in worktree |
| `o` | Open PR in browser |
| `M M` | Merge (double-press to confirm) |
| `w` | Toggle monitor (auto-fix CI + rebase) |
| `R` | Edit reviewers |
| `A` | Edit assignees |
| `S` | Slack nudge to reviewers |
| `N` | New standalone session |
| `z` | Snooze for 4 hours |
| `m` | Mark all as read |
| `g` | Refresh from GitHub |
| `t` | Cycle time filter (1d/3d/7d/30d/all) |
| `/` | Search/filter |
| `?` | Help |
| `q` | Quit |

### Detail Pane

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate comments (auto-marks as read) |
| `Space` | Select comment |
| `f` | Fix selected with Claude |
| `r` | Reply directly |
| `Esc`/`Left` | Back to sidebar |

### Terminal

| Key | Action |
|-----|--------|
| `Tab` | Exit terminal, cycle panes |
| `Ctrl-]` | Exit terminal |
| Double `Ctrl-C` | Force quit app |

## Sidebar Icons

```
CI Rv ● !
✓  ✓  ●    — CI passing, approved, unread activity
✗  ◦  ● !  — CI failing, review pending, unread, merge conflict
◦  ·       — CI running, no review
```

| Icon | Column | Meaning |
|------|--------|---------|
| `✓` | CI | CI passing |
| `✗` | CI | CI failing |
| `◦` | CI | CI running/pending |
| `✓` | Rv | Approved |
| `✗` | Rv | Changes requested |
| `◦` | Rv | Review pending |
| `●` | Unread | Has unread comments |
| `!` | Conflict | Merge conflict |

## Search / Filter

Press `/` then type. Filters combine with AND:

```
needs:reply              # you need to respond
ci:failed                # CI broken
role:author              # your PRs
is:unread                # unread activity
is:conflict              # merge conflicts
repo:api                 # filter by repo name
ci:failed role:author    # your PRs with failing CI
```

## Monitor Mode

Press `w` on a PR:

1. CI fails → Claude spawns with fix prompt, pushes automatically
2. Merge conflict → auto-rebases onto default branch
3. CI passes → back to watching
4. 3 failures → gives up

## Terminal Sessions

Terminals run inside **tmux** — Claude Code survives pilot quit.

- Press `c` → opens Claude in worktree via tmux
- Quit pilot → tmux keeps Claude alive
- Restart pilot → navigate to same PR → auto-reattaches

## Configuration

`~/.pilot/config.yaml`:

```yaml
providers:
  github:
    poll_interval: 30s
    filters:
      - org: my-org
      - watch: owner/repo    # watch ALL PRs, not just yours

display:
  activity_days: 7
  hide_approved_by_me: true

agent:
  command: claude
  resume_args: ["--continue"]
  mcp: true

shell:
  command: bash

slack:
  webhook_url: https://hooks.slack.com/services/...
```

## Architecture

10 crates. Library crates never depend on each other.

```
crates/
  core/          Task, Session, AgentConfig, TaskProvider traits
  auth/          Credential chain (env, command)
  events/        Event bus (tokio broadcast)
  store/         Store trait + SQLite + MemoryStore
  config/        YAML config
  tui-term/      PTY terminal (portable-pty + libghostty-vt)
  gh-provider/   GitHub GraphQL polling, implements TaskProvider
  git-ops/       Git worktree manager
  mcp-server/    MCP stdio server for Claude integration
  app/           TUI binary
```

### Extending

**New provider** (Linear, Jira): implement `TaskProvider` trait, wire into app.

**New agent** (Cursor, Aider): set `agent.command` in config.

## Data

| Data | Location |
|------|----------|
| Sessions + read state | `~/.pilot/state.db` |
| Git worktrees | `~/.pilot/worktrees/` |
| Logs | `/tmp/pilot.log` |

## License

MIT
