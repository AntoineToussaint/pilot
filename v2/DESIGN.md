# pilot v2 — design

Clean-slate rewrite of pilot's TUI layer. Lives in `v2/` as a separate
workspace so v1 keeps running while this is built.

## North star: v2 is an OSS project people want to use

Every design decision is filtered through: *would a new contributor
understand this in 30 minutes? Would a user try this after 5 minutes
of reading the README?* Concretely:

- **Zero-config first run.** `pilot` with an empty `~/.pilot/config.yaml`
  works — agents auto-detect (claude binary in PATH → Claude; codex in
  PATH → Codex). GitHub credentials come from `gh auth token`.
- **Single binary, no subprocess juggling.** In-process daemon is the
  default. Remote daemon is opt-in, not required.
- **One-paragraph pitch** at the top of the README: what pilot is,
  what problem it solves, one GIF.
- **Every crate has a top-of-file doc comment** explaining its role in
  one paragraph. Newcomers can navigate by folder.
- **Traits are small and focused.** `Agent`, `SessionWrapper`,
  `TaskProvider`, `Store` — each is a 5-method contract at most. No
  god-interfaces.
- **Config is discoverable.** Every option has a default, a type, and
  a one-line description. A `pilot config dump` command prints the
  effective config (defaults + user overrides + `$PILOT_*` env).
- **Errors tell you what to do next.** No "Error: GraphQL: A query
  attribute must be specified" with zero context. Messages link to
  docs or suggest a remediation.

## Goals

1. **Agent-agnostic.** Codex, Cursor, Claude Code, generic CLI — all
   first-class via a `trait Agent` in `crates/agents/`.
2. **Client / server split.** Daemon owns PTYs and state; TUI is a thin
   renderer that talks to the daemon over a socket. Enables remote
   deployment (daemon on a beefy box, TUI on a laptop).
3. **No class of recurring bugs.** Component tree with single-path focus
   replaces the four-slot state desync (see `../REWRITE.md`).
4. **Multi-terminal right pane.** Shell, agent, log tail, CI output
   coexist per session — not "one terminal per PR."

## Non-goals (v2.0)

- Windows support.
- Multi-user sharing of a daemon (single-user service only).
- Plugin loader for agents (recompile to add).
- Visual theming config.

## Architecture

```
┌───────────────────────────┐
│ pilot (TUI client)        │   Component tree + message bus.
│ v2/crates/tui/            │   Holds its own libghostty-vt for render.
└──────────────┬────────────┘
               │  Transport:
               │  - Local: Unix socket at ~/.pilot/daemon.sock
               │  - Remote: SSH tunnel to remote Unix socket
               │                (no TCP/TLS in v2.0 — SSH handles both)
               │  Framing: length-prefixed bincode
               │  Wire types: v2/crates/ipc/
┌──────────────▼────────────┐
│ pilot-daemon              │   Owns: SessionManager, TaskProviders,
│ v2/crates/daemon/         │     WorktreeManager, PTY TerminalManager,
│                           │     AgentRuntime registry, Store.
└───────────────────────────┘
```

### Answered design questions

| Question | Answer |
|----------|--------|
| Configurability | **Everything that could reasonably be configured, is.** Dashboard tile set + order, agent registry (name, spawn cmd, resume args, state patterns), keybindings per component, filter defaults, snooze presets. YAML, with sensible built-in defaults so empty config is fine. |
| Client / daemon communication | **Abstracted behind a `Client` trait so local == in-process.** When client and daemon are in the same process (the common case) the "transport" is a pair of tokio mpsc channels — zero serialization, zero sockets. Only when actually remote does it serialize over a socket. TUI code doesn't branch on local vs remote. |
| Session wrapper (tmux etc.) | **Abstracted via `SessionWrapper` trait.** `TmuxWrapper` is the default impl. Swappable so we can add `ScreenWrapper`, `ZellijWrapper`, or a no-wrapper "raw PTY" mode later without touching the daemon core. |
| Remote access | **SSH-tunneled Unix socket.** Daemon binds `~/.pilot/daemon.sock`; remote clients connect through `ssh -L`. No TCP, no TLS cert management — SSH is the trust boundary. |
| Daemon lifetime | **Long-running service when out-of-process.** First client auto-starts the daemon subprocess; survives client disconnect; `pilot daemon stop` terminates. Same model as tmux server. For the common in-process case, daemon lives and dies with the TUI. |
| Binary | **One binary.** `pilot` with subcommands: `pilot` (default: TUI + in-process daemon), `pilot daemon start/stop/status` (manage a standalone daemon), `pilot --connect <socket>` (remote TUI, don't start a local daemon). |

## Crate layout

```
v2/
├── DESIGN.md                       ← this file
├── Cargo.toml                      ← workspace
├── crates/
│   ├── ipc/                        NEW  Wire types + framing + transport
│   ├── daemon/                     NEW  Server binary
│   ├── agents/                     NEW  Agent trait + impls
│   └── tui/                        NEW  Client binary (the TUI)
└── shared/                         Reused from v1 via path deps:
    ├── core/                         source-agnostic types
    ├── auth/                         credential chain
    ├── events/                       (daemon-side) event bus
    ├── store/                        SQLite backend
    ├── config/                       YAML loader
    ├── gh-provider/                  GitHub
    ├── git-ops/                      worktrees
    └── tui-term/                     PTY + ghostty
```

Shared crates stay where they are (`../crates/`); v2 depends on them via
path. This keeps the leaf libraries (`core`, `store`, etc.) single-source;
only the app layer is rewritten.

## IPC protocol

Length-prefixed bincode over the transport.

```rust
// Command: client → daemon
enum Command {
    Subscribe(SubscribeSpec),          // start streaming events
    Spawn { session_key, kind, cwd },  // agent or shell
    Write { terminal_id, bytes },
    Resize { terminal_id, cols, rows },
    Close { terminal_id },
    Kill { session_key },              // tmux + metadata
    MarkRead { session_key },
    Snooze { session_key, until },
    Merge { session_key },
    Approve { session_key },
    // ...etc
}

// Event: daemon → client (broadcast to subscribers)
enum Event {
    SessionUpserted(Session),
    SessionRemoved(SessionKey),
    TerminalSpawned { terminal_id, session_key, kind },
    TerminalOutput { terminal_id, bytes, seq },
    TerminalExited { terminal_id, code },
    AgentState { session_key, state },
    ProviderError { source, message },
    Notification { title, body },
}
```

Reconnect: daemon keeps a per-terminal ring buffer (64 KB). On
`Subscribe`, daemon replays the ring before streaming live bytes. Client
feeds bytes into its local libghostty-vt, reconstructs the screen.

## Component tree (TUI)

```rust
trait Component {
    fn keymap(&self) -> &[Binding];
    fn handle(&mut self, key: KeyEvent) -> Outcome;
    fn on_event(&mut self, ev: &Event);      // from the daemon stream
    fn render(&self, area: Rect, frame: &mut Frame, focused: bool);
    fn children(&mut self) -> &mut [Box<dyn Component>];
}

enum Outcome {
    Consumed,                // key handled, stop bubbling
    BubbleUp,                // parent handles
    Dispatch(Command),       // send to daemon
    Focus(FocusTarget),      // change focus (child id, sibling, root)
}
```

- **One focus chain**, root → leaf. Key dispatch walks innermost to
  outermost; unhandled keys bubble. No `state.selected` vs
  `panes.focused` vs `active_tab` split.
- **Internal bus** between components (for cross-cutting concerns that
  don't go to the daemon): selection changes, layout events. Separate
  from the daemon event stream.

### Tree sketch

```
App
├── TabBar                   — subscribes: TerminalSpawned/Exited
├── Sidebar                  — default focus
│   ├── FilterRow            — owns search + time filter
│   └── SessionList          — subscribes: SessionUpserted/Removed
│       └── SessionRow × N   — subscribes: AgentState(key)
├── RightPane                — Tab reaches here from Sidebar
│   ├── Header
│   ├── Dashboard
│   │   ├── CommentsTile
│   │   ├── CiLogTile
│   │   ├── DiffTile
│   │   └── AgentStateTile
│   └── TerminalStack        — multi-terminal per session
│       ├── AgentTerminal    — subscribes: TerminalOutput(agent_id)
│       ├── ShellTerminal    — subscribes: TerminalOutput(shell_id)
│       └── LogsTerminal     — subscribes: TerminalOutput(logs_id)
└── OverlayStack             — Help / Picker / NewWorktree /
                               ConfirmKill — steals focus when active
```

## Agent abstraction

```rust
trait Agent: Send + Sync {
    fn id(&self) -> &'static str;
    fn spawn(&self, ctx: &SpawnCtx) -> Vec<String>;   // argv for tmux inner
    fn resume(&self, ctx: &SpawnCtx) -> Vec<String>;  // --continue-like
    fn detect_state(&self, ring: &[u8]) -> Option<AgentState>;
    fn hooks(&self) -> Option<HookConfig>;            // Claude has, Codex doesn't
    fn inject_prompt(&self, prompt: &str) -> Vec<u8>; // bytes to write
}
```

Built-ins (shipped in `crates/agents/`):

- `Claude` — today's behavior: spawn cmd from config, Claude Code
  lifecycle hooks for state.
- `Codex` — spawn cmd per config, no hooks; pattern-detect state from
  recent output.
- `Cursor` — same as Codex, different pattern set.
- `GenericCli` — user-defined via YAML. Fallback for anything else.

## Daemon responsibilities

1. **Providers.** `TaskProvider` pollers (currently GitHub; Linear etc.
   plug in). Emit `Event::SessionUpserted` on results.
2. **Worktrees.** `WorktreeManager` owns the layout. On startup
   reconciles disk against the session DB per the rules in `REWRITE.md`.
3. **Agent runtimes.** Look up Agent by id, spawn inside tmux, hold
   `TermSession`, stream output to subscribers.
4. **Store.** SQLite persistence of sessions, read state, snooze.
5. **IPC server.** Bind Unix socket, accept connections, route
   commands, multicast events.

## Migration path

v1 (`crates/`) stays untouched and continues working. v2 development
happens in `v2/crates/`. When v2 reaches parity:

1. `pilot` CLI grows a `--v2` flag.
2. Opt-in period (days/weeks) for dogfooding.
3. Flip default; delete v1.

The SQLite schema should stay compatible so users don't lose sessions
across the switch. Hook IPC directory (`~/.pilot/ipc/`) stays the same.

## Open questions (decide before week 3)

- **Config format.** v1 uses YAML. Keep, or move to TOML (more
  Rust-native)? Leaning: keep YAML, user shouldn't re-learn.
- **Single-binary vs two.** Ship `pilot` with a subcommand `pilot
  daemon` (single binary) or separate `pilot` + `pilot-daemon`?
  Leaning: single binary. `pilot daemon start`, `pilot daemon stop`,
  `pilot daemon status`; plain `pilot` auto-starts if needed.
- **Dashboard tile layout.** Fixed grid (2×2) or stacked (1×N with user
  reorder)? Leaning: stacked, user can drag/reorder later.

## Phase plan (rough)

| Week | Deliverable |
|------|-------------|
| 1 | `ipc` crate (wire types + transport). `daemon` binary that can spawn one PTY and stream it to a CLI test client. |
| 2 | `tui` crate with Component tree. Sidebar + Detail feature parity with v1 (reads from daemon). |
| 3 | `agents` crate: Claude + Codex + GenericCli. RightPane Dashboard + TerminalStack. Shell-in-worktree. |
| 4 | SSH remote mode. Migration shim (v1 ↔ v2 on same SQLite). Flip default. |

## What stays from v1

- `core` — Task, Session, StatusTag, TaskProvider trait.
- `auth` — credential chain.
- `store` — SQLite schema. Minor additions for per-component persistence.
- `config` — YAML loader (extended with daemon/client sections).
- `gh-provider`, `git-ops` — unchanged.
- `tui-term` — unchanged (used by daemon AND client; client needs it
  to replay PTY bytes locally).
- `events` — in-daemon bus only; daemon → client goes through IPC.

## What dies from v1

- `crates/app/src/reduce.rs` — the pure reducer model is replaced by
  per-component state and event subscription.
- `crates/app/src/pane.rs` — replaced by Component tree.
- `crates/app/src/state.rs` — no god-struct; state is component-local.
- `crates/app/src/action.rs` / `command.rs` — replaced by `ipc::Command`.
- `crates/app/src/keymap.rs` — each component ships its own keymap.
