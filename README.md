# pilot

A reactive PR inbox TUI for developers managing many repos and PRs. Instead of checking GitHub, events flow to you -- new comments, CI failures, review requests surface automatically. Each PR becomes a session with an embedded terminal for running Claude Code or a shell in an isolated git worktree.

## Features

- **Reactive inbox** -- PRs sorted by latest activity, with triage badges (`reply`, `CI`, `changes`, `review`, `MERGE`, `QUEUE`) and colored label pills
- **Embedded terminals** -- Claude Code or shell in isolated git worktrees, with live agent state detection (spinner = thinking, `>_` = idle, `?` = asking)
- **Monitor mode** -- Automatic CI failure fixing and merge conflict rebasing. Press `w`, walk away
- **Smart filters** -- `needs:reply ci:failed role:author` finds exactly what needs your attention
- **MCP integration** -- Claude can push, comment, merge, approve with your confirmation
- **Desktop notifications** -- macOS alerts when CI fails, PRs get approved, or someone comments
- **Quick actions** -- Merge (`M`), quick reply (`e`), edit reviewers (`R`), Slack nudge (`S`), snooze (`z`)
- **Per-comment read tracking** -- Individual comments marked as read via `Space`, auto-mark-read after 2s of viewing
- **Diff stats** -- `+123 -45` shown per PR in sidebar and detail pane
- **Standalone sessions** -- Create sessions without a PR (`N`), launch Claude, work freely
- **Agent-agnostic** -- Configurable agent command (Claude Code default, swap to Cursor/Aider/anything via config)
- **PR number colors** -- Each `#1234` gets a unique color for visual anchoring

## Install

### Prerequisites

- **Rust** 1.75+ (`cargo`)
- **GitHub CLI** (`gh`) -- authenticated via `gh auth login`
- **Claude Code** (`claude`) -- for AI-assisted workflows (optional)
- **libghostty-vt** -- terminal emulation (see Cargo.toml path deps)

### Build

```bash
git clone https://github.com/AntoineToussaint/gh-pilot.git
cd gh-pilot
cargo build                    # first build ~30s (compiles SQLite)
cargo test --workspace         # 24 tests
cargo clippy --workspace       # lint
```

### Run

```bash
cargo run -p pilot             # uses `gh auth token` automatically
# or with libghostty dynamic linking:
./run.sh
```

Logs: `/tmp/pilot.log` | State: `~/.pilot/state.db`

## Quick Start

1. Start pilot -- it connects via `gh auth token` and polls every 30s
2. `j`/`k` to navigate PRs, `Enter` to see details
3. `c` to open Claude Code in the PR's worktree
4. Select review comments with `Space`, press `f` to send to Claude for fixing
5. `Tab` to cycle between sidebar / detail / terminal
6. `?` for full keybinding reference

## Keybindings

### Sidebar

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate |
| `Enter` | Open detail |
| `c` | Claude Code in worktree |
| `b` | Shell in worktree |
| `m` | Mark all read |
| `w` | Toggle monitor |
| `M` | Merge (double-press) |
| `N` | New standalone session |
| `z` | Snooze for 4 hours |
| `g` | Refresh now |
| `t` | Cycle time filter |
| `/` | Search/filter |
| `?` | Help |

### Detail

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate comments |
| `Space` | Mark comment read / select |
| `f` | Send selected to Claude (fix) |
| `r` | Send selected to Claude (reply) |
| `e` | Quick reply (post directly) |
| `R` | Edit reviewers |
| `A` | Edit assignees |
| `S` | Slack nudge |

### Terminal

| Key | Action |
|-----|--------|
| `Tab` | Exit terminal, cycle panes |
| `Ctrl-]`/`Ctrl-o` | Exit terminal |
| `Ctrl-w` + `h/j/k/l` | Focus pane |
| `Ctrl-w` + `v/s` | Split |
| `Ctrl-w` + `z` | Fullscreen |

## Search Syntax

Press `/` then type. Combine filters with spaces (AND):

```
needs:reply              # you need to respond
ci:failed                # CI is broken
role:author              # your PRs
is:unread                # unread activity
repo:api                 # by repo name
ci:failed role:author    # your failing PRs
```

## Monitor Mode

Press `w` to auto-fix a PR:

1. CI fails -> Claude Code spawns with fix prompt
2. Claude pushes (auto-approved) -> waits for CI
3. CI passes -> back to watching
4. 3 failures -> gives up, notifies you
5. Merge conflict -> auto-rebases onto default branch

## Configuration

`~/.pilot/config.yaml`:

```yaml
providers:
  github:
    poll_interval: 30s
    filters:
      - org: my-org

display:
  activity_days: 7

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

```
crates/
  core/          # Task, Session, AgentConfig, TaskProvider traits
  auth/          # Credential chain (env, command, static)
  events/        # Event bus (tokio broadcast)
  store/         # Store trait + SQLite + MemoryStore (tests)
  config/        # YAML config
  tui-term/      # PTY terminal (portable-pty + libghostty-vt)
  gh-provider/   # GitHub GraphQL polling, implements TaskProvider
  git-ops/       # Git worktree manager
  mcp-server/    # MCP stdio server for Claude integration
  app/           # TUI binary
```

Key crate rules: core, auth, events, store never depend on each other. Provider crates depend on core + events + auth. App depends on everything.

## Extending

**New provider** (Linear, Jira): implement `TaskProvider` trait (`name()` + `fetch_tasks()`), wire into app.

**New agent** (Cursor, Aider): set `agent.command` in config. Customize `asking_patterns` for idle/question detection.

**New storage**: implement `Store` trait, swap in app.

## License

MIT
