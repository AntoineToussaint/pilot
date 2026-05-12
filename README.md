# pilot

A reactive PR inbox in your terminal.

Instead of refreshing GitHub, events flow to you — new comments, CI
failures, review requests surface as they land. Each task becomes a
session with an embedded terminal for running Claude Code, Codex,
Cursor, or a shell in a git worktree.

Source-agnostic: GitHub today, Linear tomorrow, Jira after that.
Same UI, same key bindings, same inbox.

## Status

Pre-1.0. Daily-driver-ish on macOS + Linux for the author. Expect
sharp edges and frequent breaking changes until the first release.

## Quick start (dev)

```sh
git clone https://github.com/AntoineToussaint/pilot.git
cd pilot
make setup           # one-shot: download pinned zig 0.15.2 to vendor/zig/
make run             # build + run
```

Or for repeated dev launches:

```sh
make run                       # default
make run ARGS="--fresh"        # any flag via ARGS=
make run-fresh                 # shortcut for the common cases
make run-test
make run-connect SOCKET=/tmp/pilot.sock

./run.sh --fresh               # the bash flavor — same effect
```

**What `make setup` does:**
- Downloads zig 0.15.2 into `vendor/zig/<host>/`. That's the only
  out-of-band dep — `libghostty-rs` is vendored under
  `crates/libghostty-vt*/`, so a fresh clone builds standalone.
- Verifies `cargo` and `gh` are on PATH.

**Prerequisites:** Rust 1.85+, a C compiler (for bundled SQLite),
and the GitHub CLI for credentials (`gh auth login`).

If you `cargo build` directly (no Makefile / `run.sh`), put
**zig 0.15.2** on PATH first — newer zig trips ghostty's
`requireZig` check.

## Install (release)

The `cargo-dist` release pipeline is wired (see
`.github/workflows/release.yml` + `[workspace.metadata.dist]`).
It builds prebuilt binaries for macOS (aarch64 + x86_64) and Linux
(aarch64 + x86_64) and publishes:

- **Homebrew tap** — `brew install <user>/pilot/pilot`
- **Curl install script** — `curl -sSf https://.../install.sh | sh`
- **GitHub Releases** — binary tarballs for manual download

Released by pushing a `v*.*.*` tag. Pre-1.0, no tag has been pushed
yet — these channels are scaffolded and ready, not yet active.

## Run

```sh
pilot                       # default — in-process daemon + TUI
pilot --fresh               # wipe ~/.pilot/v2/state.db, re-run setup
pilot --test                # tempdir + seeded session, no GitHub
pilot --connect <socket>    # connect to a remote daemon over a Unix socket
pilot server start          # standalone daemon (for SSH / multi-client)
pilot server api [addr:port]   # JSON HTTP API gateway
```

`gh auth token` provides GitHub creds by default. `LINEAR_API_KEY`
provides Linear creds. Run with `RUST_LOG=pilot=debug` for verbose
logs, which go to `/tmp/pilot.log` (the TUI takes the screen).

## Key bindings

**Global** (anywhere except inside a terminal):

| Key | Action |
|---|---|
| `Tab` | Cycle Sidebar → Activity → Terminals |
| `?` | Help overlay |
| `,` | Settings palette (add repo / edit roles / pick agents / …) |
| `q q` | Quit (double-tap within 800ms) |
| `Ctrl-Shift-D` | Detach focused pane into a new pilot window |
| `Shift-arrows` | Resize splitters |

**Sidebar** (workspace list):

| Key | Action |
|---|---|
| `j/k` | Navigate |
| `Enter` | Open the focused workspace |
| `Space` | Fold / unfold the parent repo group |
| `s` | Spawn a shell |
| `c` | Spawn Claude Code |
| `x` | Spawn Codex |
| `u` | Spawn Cursor |
| `m` | Mark **all** of this workspace's activity as read (bulk) |
| `n` | New pre-PR workspace |
| `e` | Open the worktree in your editor (Zed / VS Code / Cursor / …) |
| `g` | Manual refresh — re-poll providers right now |
| `/` | Search |
| `Shift-X X` | Kill workspace (two-press) |

**Activity** (right pane):

| Key | Action |
|---|---|
| `j/k` | Scroll the activity feed |
| `g/G` | Top / bottom |
| `m` | Mark the focused comment as read |

**Cross-pane** (works from Sidebar OR Activity):

| Key | Action |
|---|---|
| `r` | Reply (open the textarea targeted at the selected workspace) |

**Terminals:** all keys forward to the PTY. `Ctrl-c` is SIGINT.
`]]` (two presses) returns to the sidebar. Mouse wheel scrolls the
inner program if it has mouse-tracking on (Claude Code, vim, less,
…) or scrolls libghostty's scrollback otherwise.

**Pickers** (Settings palette, scope/agent/repo pickers — any
`Choice` modal):

| Key | Action |
|---|---|
| `j/k` or `↑/↓` | Navigate |
| `Space` | Toggle (multi-select pickers) |
| `Enter` | Confirm |
| `PageUp/PageDown` | Jump a screen at a time |
| `Ctrl-u/Ctrl-d` | Half-page jump (vim-style) |
| `Home/End` or `g/G` | First / last item |
| `Backspace` | Back to the previous step (where applicable) |
| `Esc` or `Ctrl-c` | Cancel |

## Architecture

14 pilot crates in a client/daemon split (+ 2 vendored libghostty
crates for the embedded terminal). The four core libraries (`core`,
`auth`, `events`, `store`) never depend on each other. Providers
plug in via two traits — see
[`crates/core/src/provider.rs`](crates/core/src/provider.rs) and
[`crates/core/src/scope.rs`](crates/core/src/scope.rs).

```
crates/
  core/            # Task, Session, Activity. Source-agnostic.
  auth/            # CredentialProvider trait + chain.
  events/          # In-process event bus.
  store/           # Store trait + SQLite backend.
  config/          # YAML loader.
  git-ops/         # Worktree manager.
  tui-term/        # Embedded terminal (libghostty-vt + portable-pty).
  gh-provider/     # GitHub PRs + Issues.
  linear-provider/ # Linear issues.
  ipc/             # Wire types + transport (in-process channel + Unix socket).
  agents/          # Agent trait + Claude/Codex/Cursor/Generic builtins.
  llm-proxy/       # 127.0.0.1 HTTP pass-through that records token usage.
  server/          # Daemon: PTY lifecycle, polling, JSON API gateway.
  tui/             # Realm-based TUI client. Hosts the `pilot` binary.
```

See [`CLAUDE.md`](CLAUDE.md) for the long-form architecture notes
and [`DESIGN.md`](DESIGN.md) for the rationale behind the
client/daemon split + the pane / modal / component tiers.

## Configuration

Per-user config lives at `~/.pilot/config.yaml`. The setup wizard
(`,` from inside pilot, or first-launch) writes most of it; press
`,` any time to add a repo / change agents / edit roles without
nuking state.

**Editors** (the `e` shortcut): pilot detects Zed / VS Code /
Cursor / Windsurf / Fleet / IDEA at startup. Add custom entries
or override builtins via `editors:`:

```yaml
editors:
  - id: zed
    display: "Zed (insider)"
    command: /Applications/Zed-Insiders.app/Contents/MacOS/zed
    args: ["{path}"]
  - id: my-editor
    command: /opt/myeditor/bin/edit
    args: ["--workspace", "{path}"]
```

`{path}` is replaced with the workspace's worktree dir at launch.

State (workspace activity, read/unread, snooze, terminal scrollback
ring) persists in `~/.pilot/v2/state.db` via SQLite.

## Contributing

Issues and PRs welcome. Two ground rules:

- **Tests with every change.** Library crates have unit tests; UI
  components have ratatui `TestBackend` snapshots; the orchestrator
  has integration tests in `crates/tui/tests/`.
- **No new dependencies in the four core libraries** without
  discussion — the layering is what keeps the codebase honest.

## License

MIT — see [`LICENSE`](LICENSE).
