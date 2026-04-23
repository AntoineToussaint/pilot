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

1. **Source-agnostic.** GitHub PRs, GitHub Issues, Linear tickets,
   Jira, Gerrit — all first-class via `trait TaskProvider`. The inbox
   doesn't care where a task came from; filters let the user scope to
   one provider, one repo, one project.
2. **Agent-agnostic.** Codex, Cursor, Claude Code, generic CLI — all
   first-class via a `trait Agent` in `crates/agents/`.
3. **Client / server split.** Daemon owns PTYs and state; TUI is a thin
   renderer that talks to the daemon over a socket. Enables remote
   deployment (daemon on a beefy box, TUI on a laptop).
4. **No class of recurring bugs.** Component tree with single-path focus
   replaces the four-slot state desync (see `../REWRITE.md`).
5. **Multi-terminal right pane.** Shell, agent, log tail, CI output
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

## Sources (`TaskProvider`)

The same trait that exists in v1 `pilot-core` — reused unchanged. Each
source returns a stream of `Task`s; pilot doesn't care whether a task
is a GitHub PR, a GitHub issue, or a Linear ticket. All share the
same row model, sidebar, status tags, search.

### Shipping in v2.0

| Source | Crate | Status |
|--------|-------|--------|
| GitHub PRs | `gh-provider` (reused from v1) | parity |
| GitHub Issues | `gh-provider` | NEW — single GraphQL query alongside PRs |
| Linear | `crates/linear-provider` (NEW) | Week 3 |
| GenericHttp | `crates/http-provider` (NEW) | Week 4 — poll any JSON endpoint, map fields via config |

### Per-source behaviors that still matter

- **Merge semantics.** PRs have "merge"; issues have "close"; Linear
  tickets have "done". The `Task` model already has a neutral
  `state: TaskState` enum; providers set it.
- **Comments / replies.** `r` to reply → provider-specific API (GH
  comment vs Linear comment vs Slack message for integrations).
- **Worktrees.** Only sources that carry a `branch` get a worktree.
  Linear tickets with no linked branch are "just an inbox row" —
  opens a notes pane instead of Claude-in-worktree. A Linear ticket
  that IS linked to a PR can be rendered as one merged row or two
  linked rows (config: `display.merge_linked_items`).
- **Auth.** Each provider builds its own `CredentialProvider` chain.
  GitHub uses `gh auth token`; Linear uses `LINEAR_API_KEY` env or
  `pilot auth linear` flow (TBD).

### Filter UX

Search grows provider tokens:

```
source:github          # only GitHub (PRs + issues)
source:gh/pr           # only GitHub PRs
source:gh/issue        # only GitHub issues
source:linear          # only Linear
project:ENG            # Linear project key
```

All existing tokens (`needs:reply`, `ci:failed`, `role:author`,
`is:unread`, etc.) still work and compose with AND.

## LLM proxy — structured telemetry from agents

Parsing PTY output to understand what an agent is doing is brittle
(it worked well enough for "working vs asking", but we hit the ceiling
fast on tool calls, token counts, cost). Instead, v2 interposes as an
**LLM API proxy**: the daemon runs a tiny HTTP server, injects
`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` into the agent's env, and
captures structured metadata on every request/response.

```
┌──────────────────────────┐
│ Claude Code (in tmux)    │
│ ANTHROPIC_BASE_URL=       │
│   http://127.0.0.1:PORT/  │
└────────────┬─────────────┘
             │ /v1/messages
             ▼
┌──────────────────────────┐  records:
│ LLM Proxy                │    - timestamp, session_key
│ (in daemon)              │    - model, token counts (in/out/cache)
│                          │    - tool calls (name, args, result size)
│                          │    - request latency
│                          │    - estimated cost
│                          │    - assistant text (for summary / search)
└────────────┬─────────────┘
             │ /v1/messages  (forward upstream, verbatim)
             ▼
        api.anthropic.com
```

### What the proxy gives us

- **Token & cost per session.** Live counter in the sidebar row.
  Aggregate per day / per repo / per agent in the dashboard.
- **Tool-call timeline.** A real structured list of what the agent did
  (read file X, ran `cargo test`, edited Y:42) instead of scraping PTY
  frames. Powers a new "Activity" tile in the right pane dashboard.
- **Reliable state detection.** "Request in flight" = working,
  "response ended without tool_use" = idle, "tool_use with name
  `AskUserQuestion` or `bash` and unresolved" = asking. No more
  pattern-matching on `Esc to cancel`.
- **Search the conversation.** Because we have every assistant turn's
  text, users can `s` for prior conversations. "Find the session where
  I asked about the config_toml migration".
- **Budget guardrails.** Optional: cap $/day or tokens/day per agent;
  return a 429 that the agent surfaces as "your budget is exhausted".
- **Offline cache / replay.** Not v2.0, but the architecture allows it.

### What it deliberately does NOT do

- **Re-routing / model swap.** Proxy is read-only — it forwards the
  exact request to the exact upstream the agent chose. No "secretly
  use Haiku instead of Sonnet." That surprise is not worth the value.
- **Modification of responses.** Same reason. Observability only.
- **Policy enforcement by default.** Budgets are opt-in. Default is
  pure pass-through.

### Implementation

New crate `crates/llm-proxy/`:

```rust
pub struct ProxyConfig {
    pub listen: std::net::SocketAddr,      // typically 127.0.0.1:<ephemeral>
    pub record_bodies: bool,               // true by default
    pub redact: Vec<String>,                // headers/fields to strip from records
}

pub struct ProxyServer {
    /// Records flow out via this channel so the daemon's main loop
    /// can persist them + emit Events.
    pub records_tx: mpsc::Sender<ProxyRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRecord {
    pub session_key: SessionKey,          // pulled from a request header
                                           // we inject when spawning
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration: std::time::Duration,
    pub provider: ApiProvider,             // Anthropic / OpenAI / …
    pub endpoint: String,                  // "/v1/messages"
    pub request_model: Option<String>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub tokens_cache_create: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub tool_calls: Vec<ToolCall>,
    pub assistant_text: Option<String>,    // for search
    pub status: u16,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args_summary: String,              // truncated / structured
    pub result_size_bytes: Option<u64>,
    pub duration: Option<std::time::Duration>,
}
```

### Agent wiring

`Agent::spawn` gets a `ProxyCtx` in `SpawnCtx`:

```rust
pub struct ProxyCtx {
    pub anthropic_url: Option<String>,     // base URL to inject
    pub openai_url: Option<String>,
    pub session_tag: String,               // header value linking
                                           // requests → session_key
}
```

The daemon builds this `ProxyCtx` when starting a terminal, points the
proxy at the upstream for whichever API keys the user has configured,
and injects the env vars into the wrapped command. Agent impls don't
need to know — the env vars are standard.

### Trust model

- **Only listens on 127.0.0.1** — never exposed to the network.
- **Forwards the agent's `Authorization` header verbatim.** The
  daemon never sees the user's API key unless the user explicitly
  configures one for its own polling.
- **Bodies are recorded by default.** Opt-out via `proxy.record_bodies:
  false` in config. A redact list strips known-sensitive headers
  (cookies, extra auth) and JSON paths (`messages[].content` if the
  user wants content-free telemetry).

### Storage

Proxy records land in SQLite next to sessions. A `proxy_records`
table keyed by `(session_key, started_at)`. Separate so users can
easily `DELETE FROM proxy_records` if they change their mind about
recording bodies.

Rough size budget: a verbose Claude turn is ~50 KB of JSON; a busy day
is maybe 100 turns. So ~5 MB/day/active-session. Fine for SQLite;
users who care can enable `record_bodies: false` and keep only the
counters (tiny).

### Open questions

- **Aider / custom agents that hit a model we don't route?** The env
  vars are a hint, not a requirement. If the agent ignores them we
  just don't record that agent. Graceful degrade.
- **Streaming responses.** Proxy needs to be a duplex streamer, not
  buffer-then-forward. Hyper's streaming body types work fine; we
  tee the bytes into a parser that assembles the record as SSE frames
  arrive.
- **OpenAI vs Anthropic wire formats.** Different JSON shapes. Crate
  has a per-provider adapter module.
- **Cost estimation.** Hard-coded price table by model, updated via
  a small `prices.rs` module. Acceptable because models change slowly.

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

## Testing discipline (non-negotiable)

The class of bugs we shipped in v1 came from "I tested it worked once,
edge case broke later." v2 rule: **every public function has a test;
every component has a render snapshot; every bug fix lands with a
regression test.**

| Layer | What gets tested |
|-------|------------------|
| `ipc` | Serde round-trip per Command/Event variant; framing on synthetic streams; property tests for arbitrary frame sizes + malformed bytes. |
| `agents` | Registry lookup; each Agent's spawn/resume argv snapshotted; SessionWrapper behaviors (tmux mocked by intercepting Command). |
| `llm-proxy` | Record serde round-trip; pricing rates for known models; Unknown returns None; redaction on headers + nested JSON; streaming SSE assembly from recorded fixtures. |
| `daemon` | End-to-end via `channel::pair` — Subscribe → Snapshot; PTY spawn → output stream → exit; ring buffer wraparound; reconnect replay fidelity. |
| `tui` components | Pure key-routing tests (no render). Golden render snapshots via `insta` + ratatui `TestBackend`. Event-subscription dispatch tests. Focus chain invariants. |
| providers | GraphQL fixtures checked into `tests/fixtures/`. Never hit live APIs in unit tests. One opt-in integration test per provider gated on env var. |
| Cross-crate | Integration suite in `v2/tests/` exercising the full in-process stack (TUI → in-process daemon → mock provider → mock agent). |

CI matrix: Linux + macOS, `cargo test --workspace` + `cargo clippy
--workspace -- -D warnings` + `cargo fmt --check` on every PR.
`cargo test --doc` enabled. Coverage tracked via `cargo llvm-cov` —
target 80% on library crates (daemon/ipc/agents/llm-proxy/providers).
TUI render tests count as coverage via the ratatui TestBackend.

## Phase plan (rough — each week ships fully tested)

Each phase is "not done" until CI is green, coverage hits the target,
and every golden snapshot is reviewed.

| Week | Deliverable | Key tests |
|------|-------------|-----------|
| 1 | `ipc` wire types + transport. `agents` trait + builtins. `llm-proxy` types + pricing. `daemon` skeleton with PTY lifecycle + ring buffer. | Serde round-trip, framing, PTY spawn→exit, replay on reconnect. |
| 2 | `tui`: Component trait + tree infra. Sidebar + RightPane + Overlays. Feature parity with v1 for browse-only flows. | Key routing, event dispatch, golden render snapshots, focus invariants. |
| 3 | `llm-proxy` real hyper server (Anthropic + OpenAI). Daemon integration (ProxyCtx injection + records → Events). Dashboard tiles including Cost/Tokens. TerminalStack with multi-terminal. | SSE stream assembly from fixtures, redaction, proxy record attribution, tile ordering. |
| 4 | GitHub Issues in `gh-provider`. Linear provider. SSH remote + daemon subcommands. Migration shim (v1 ↔ v2 SQLite). Flip default behind opt-in flag. | Fixture-based provider tests, daemon lifecycle idempotence, cross-version SQLite load. |

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
