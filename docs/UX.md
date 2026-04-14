# pilot UX Manual

A reactive PR inbox for developers managing many repos and PRs. Events flow to you — new comments, CI failures, review requests surface automatically.

## Quick Start

```bash
cargo run -p pilot        # uses `gh auth token` automatically
```

Press `?` for the keybinding cheat sheet at any time.

## Layout

```
+------ Sidebar (50%) ------+-------- Detail Pane --------+
| PILOT  12 PRs  2 reply    | OPEN  Fix auth bug     2h  |
| ─────────────────────────  | ─────────────────────────── |
| / filter...                | CI passing  Review pending  |
| 2 reply  1 CI  1 merge    | Reviewers: alice, bob       |
|                            | ─────────────────────────── |
| ── tensorzero (5) ──────── | Description                 |
| ✓ #7283 Add model fields  |   Fixes the auth flow by... |
| ✗ #7260 Fix dark theme    | ─────────────────────────── |
| ◦ #7255 Align conventions | ┌ alice · 2h · review ───── |
| ✓ #7250 Dedup key  MERGE  | │ Looks good, one nit...    |
|                            | └─────────────────────────── |
| ── other-repo (2) ──────── |                             |
| ✓ #456  New feature QUEUE  |                             |
+----------------------------+-------- Terminal -----------+
| INBOX  alice (gh)  * 5     | $ claude (in worktree)     |
+----------------------------+-----------------------------+
```

## Key Modes

| Mode | How to enter | How to exit |
|------|-------------|-------------|
| **INBOX** (sidebar) | `Tab` from other panes | `Tab` cycles |
| **DETAIL** | `Enter` or `Tab` from sidebar | `Esc`, `Left`, `Tab` |
| **TERMINAL** | `c` (Claude) or `b` (shell) | `Tab`, `Ctrl-]`, `Ctrl-o` |
| **PANE** | `Ctrl-w` then one key | Auto-exits after one key |

## Sidebar Navigation

| Key | Action |
|-----|--------|
| `j` / `k` | Move cursor down/up |
| `Enter` | Open detail pane |
| `Left` / `Right` | Collapse/expand repo group |
| `t` | Cycle time filter: 1d / 3d / 7d / 30d / all |
| `/` | Search/filter (type query, Enter to keep, Esc to clear) |
| `g` | Refresh from GitHub immediately |

## Session Actions

| Key | Action |
|-----|--------|
| `c` | Open Claude Code in worktree |
| `b` | Open shell in worktree |
| `m` | Mark as read |
| `w` | Toggle monitor (auto-fix CI + rebase) |
| `M` | Merge PR (press twice to confirm) |
| `N` | Create new standalone session |

## Detail Pane

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate comments |
| `Space` | Select/deselect comment |
| `f` | Send selected to Claude for **fixing** |
| `r` | Send selected to Claude for **replying** |
| `R` | Edit reviewers (picker) |
| `A` | Edit assignees (picker) |
| `S` | Send Slack nudge to reviewers |

## Search Syntax

Combine filters with spaces (AND logic):

| Filter | Matches |
|--------|---------|
| `needs:reply` | You need to respond |
| `needs:review` | Waiting for your review |
| `ci:failed` | CI is failing |
| `ci:passed` | CI is green |
| `is:unread` | Has unread activity |
| `is:draft` | Draft PRs |
| `role:author` | PRs you authored |
| `role:reviewer` | PRs you're reviewing |
| `repo:name` | Filter by repo name |
| *free text* | Matches title, body, comments |

Example: `ci:failed role:author` shows your PRs with failing CI.

## Status Indicators

### Sidebar badges (right-aligned)

| Badge | Meaning |
|-------|---------|
| `MERGE` (green) | Approved, ready to merge |
| `QUEUE` (purple) | In merge queue / auto-merge |
| `←alice` (red) | Needs your reply to alice |
| `CI ✗` (red) | CI is failing |
| `changes` (orange) | Reviewer requested changes |
| `review` (yellow) | Waiting for your review |
| `2h` (gray) | Time since last update |

### Agent indicators

| Icon | Meaning |
|------|---------|
| `⠋` (yellow spinner) | Claude is working |
| `>_` (green) | Claude is idle at prompt |
| `?` (red) | Claude needs your input |
| `$` (gray) | Shell terminal running |

### PR number colors
Each `#1234` gets a unique color (hashed from the number) so your brain can associate color with work.

## Monitor Mode

Press `w` to enable automatic monitoring on a PR. The monitor:

1. **Detects CI failures** and spawns Claude Code to fix them
2. **Detects merge conflicts** and auto-rebases onto the default branch
3. **Retries** up to 3 times if CI keeps failing
4. **Auto-approves** `pilot_push` so Claude can push without confirmation

States: `watching` → `fixing CI` → `waiting CI` → back to `watching` (or `failed` after 3 tries).

## Auto-mark-read

When you navigate to a session and view it for 2+ seconds, it's automatically marked as read. No need to press `m` manually.

## Merged/Closed PRs

Merged and closed PRs are automatically hidden from the sidebar. They're still in the database — just not displayed.

## MCP Tools (Claude Integration)

When Claude Code runs in a worktree, it has access to pilot tools via MCP:

| Tool | Requires confirmation |
|------|-----------------------|
| `pilot_push` | Yes (auto in monitor) |
| `pilot_reply` | Yes |
| `pilot_merge` | Yes |
| `pilot_approve` | Yes |
| `pilot_resolve_thread` | Yes |
| `pilot_request_changes` | Yes |
| `pilot_get_pr_state` | No (read-only) |
| `pilot_get_context` | No (read-only) |

## Configuration (~/.pilot/config.yaml)

```yaml
providers:
  github:
    poll_interval: 30s
    filters:
      - org: my-org          # only show PRs from this org
      - repo: owner/repo     # or specific repo

display:
  activity_days: 7            # hide PRs older than 7 days (0 = all)

agent:
  command: claude             # agent binary
  resume_args: ["--continue"] # args for resuming previous session
  mcp: true                   # write .mcp.json for MCP discovery

shell:
  command: bash               # shell binary

slack:
  webhook_url: https://hooks.slack.com/services/...   # for S nudge
```

## Persistence

- Sessions, read/unread state, and activity persist in `~/.pilot/state.db`
- Claude Code conversations persist in worktrees (use `--continue` on reopen)
- Logs go to `/tmp/pilot.log`

## Pane Management (Ctrl-w prefix)

| Key | Action |
|-----|--------|
| `Ctrl-w v` | Split vertically |
| `Ctrl-w s` | Split horizontally |
| `Ctrl-w c/q` | Close pane |
| `Ctrl-w h/j/k/l` | Focus left/down/up/right |
| `Ctrl-w +/-` | Resize |
| `Ctrl-w z` | Fullscreen toggle |
