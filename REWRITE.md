# REWRITE — target architecture

Notes for the eventual rewrite of pilot's TUI layer. Do not treat as a
feature plan; this is the shape we want to reach when UX is stable
enough to justify a clean cut.

## Problems today

`crates/app/` grew organically. Three overlapping sources of truth end
up desyncing on every edge case:

| Slot | What it tracks | Who writes it |
|------|----------------|---------------|
| `state.selected` | Sidebar cursor row | `SelectNext`, `SelectPrev`, `JumpToNextAsking`, `NewSessionConfirm`, first-poll purge, filter changes, Shift-S, `ExternalEvent` restore path, more |
| `panes.focused` | Pane tree focus | `FocusPaneNext/Prev`, `FocusTerminalPane`, splits, closes, `sync_for_selection`, `ResetLayout` |
| `terminal_index.active_tab` | Active PTY tab | `NextTab`, `PrevTab`, `OpenSession`, `SetActiveTab`, `CloseTab`, `KillSession` |
| Pane tree's `Terminal(leaf_key)` | Which session's PTY is shown in the terminal pane | `sync_for_selection`, `FocusTerminalPane`, old `enforce_terminal_invariant` retarget (since removed) |

Every bug we've chased this week is a different corner of "two of these
disagree":

- Tab stops cycling → pane focus and selected diverge
- Terminals appear but keys don't reach them → focused pane targets a
  dead key
- `/` moves cursor but not focus → move-cursor path doesn't trigger sync
- Refresh teleports to top → first-poll purge writes `selected = 0`

Patches work. They don't compose: each fix closes one corner, a
different corner breaks the next session. The root cause is the
**four-slot desync**, not any individual mistake.

## Target: component tree + message bus

Two layers, nothing else.

### 1. Component tree (focus + keys)

```rust
trait Component {
    /// Keys this component handles when focused (or always, if global).
    fn keymap(&self) -> &[Binding];

    /// Handle a key. Returns Some(msg) to publish to the bus, None if
    /// the component consumed the key without side effects, or falls
    /// through to parent if not matched in keymap.
    fn handle(&mut self, key: KeyEvent) -> Option<Message>;

    /// Paint into `area`. `focused` is true iff this component is the
    /// current focus tip.
    fn render(&self, area: Rect, frame: &mut Frame, focused: bool);

    /// Children in tab-order. Parent decides how focus flows between
    /// them (Tab cycle, focus-next-after-N, etc.).
    fn children(&mut self) -> &mut [Box<dyn Component>];
}
```

- **Focus = a single path from root to leaf.** There is exactly ONE
  "current focus" and it lives in the tree, not in four bags of state.
- **Key dispatch walks focus from tip to root.** Innermost component
  gets first shot; unhandled keys bubble up. The parent's keymap is a
  fallback, never a preempt.
- **Each component owns its state and its children's identity.** No
  global `state.sessions` + `state.panes` + `state.terminal_index`.
  The Sidebar owns the session list; the Terminal pane owns its PTY
  binding; the Session row owns its agent-state indicator.
- **Serialization** is each component's business. `trait Persist`
  lets a component opt into SQLite, a file, or nothing.

### 2. Message bus

```rust
enum Topic { GhPrUpdated(SessionKey), TmuxDied(SessionKey), AgentState(SessionKey, State), ... }

trait Bus {
    fn publish(&self, msg: Message);
    fn subscribe(&self, topic: Topic) -> impl Stream<Item = Message>;
}

trait Component {
    fn subscribe(&self) -> Vec<Topic>;
    fn on_message(&mut self, msg: Message);
}
```

- Producers (GitHub poller, PTY reader, Claude hook watcher, monitor
  ticks) publish to topics without knowing who listens.
- Consumers subscribe declaratively. Adding a "CI status badge" widget
  is: write a Component, subscribe to `Topic::CiStatus`, no app-level
  wiring.
- The bus is the ONLY channel across components. No `Arc<Mutex<...>>`
  sprinkled on shared state.

## Consequences

- Adding a component never modifies existing code — it mounts itself
  into the tree and subscribes to what it cares about.
- Testing a component = mount it with a stub bus, assert
  `key → message → state mutation`. No app harness.
- The "four-slot desync" bug class disappears: there's one focus chain,
  one source of truth for what the user is looking at.

## Component tree sketch for pilot

```
App
├── TabBar (subscribes to term.spawned / term.closed)
├── Sidebar                             ← default focus
│   ├── FilterRow (owns search + time filter)
│   └── SessionList (subscribes to pr.updated, session.state.changed)
│       └── SessionRow × N
├── RightPane (Tab reaches this from Sidebar)
│   ├── Header (subscribes to selection changes)
│   ├── CommentList (j/k/Space while focused)
│   └── Terminal (owns PTY subscription; eats all keys when focused)
└── Overlays (Help / NewWorktree / Picker — focus steal when active)
```

## Migration path (one-shot, not incremental)

Incremental rewrite is how we got here. The layers fight each other if
mixed. Plan is:

1. Freeze the current `app/` crate.
2. New crate `app-v2/` with `Component` + `Bus` infra.
3. Port the Sidebar first (the heaviest component). Run both UIs
   behind a CLI flag during dev.
4. Port Detail, Terminal, Overlays, Picker in sequence.
5. Switch default to v2. Delete old `app/`.

Estimate: ~1 week of focused effort for someone who can hold the whole
system in their head. Not a good fit for drive-by patches — this has
to be one coherent change or we end up with both architectures fused
together (worse than either alone).

## Non-goals for the rewrite

- No new features. Feature parity only.
- No provider changes. `TaskProvider`/`Store`/`CredentialProvider`
  traits in `core`/`auth`/`store` are fine — the rewrite is purely
  the TUI layer.
- No theming system. Colors stay hard-coded.
- No plugin loader. Components are compiled in.

## Worktree invariants that pilot can't currently enforce

Several bug classes we've hit are "pilot assumes X about worktrees but
git will happily violate X." Fix by owning worktree state more tightly
in the rewrite.

### Branch swap inside a worktree

A user (or Claude) can `cd` into pilot's worktree and `git checkout`
a different branch. Now pilot's session records `branch = X` but the
worktree is actually on branch `Y`, and branch `Y` is "stolen" from
any other worktree that might want it. Next `c` on the PR whose head
is `Y` fails with "`Y` is already used by worktree at ...".

**Rewrite rule:** worktree ownership is one-way. Pilot writes the
branch at creation time and treats any subsequent branch swap as a
corruption. Either:
  - Re-assert on every attach (`git -C <wt> checkout <expected>`),
    respecting uncommitted changes via stash.
  - Detect drift and surface a clear error + one-keystroke rescue
    ("your worktree switched branches — stash and reset? [y/N]").

Do NOT silently let git's flexibility leak into pilot's data model.

### Stacked PRs sharing state

Stacked PRs (PR B based on PR A's head) cause several conflicts:
  - Creating a worktree for B while A's worktree already has A's
    head checked out → the "refusing to fetch into branch X" family
    we spent a day patching with remote-tracking refs.
  - Rebasing A's head invalidates every downstream stack's commit
    tree; pilot's per-PR monitor can loop on this.
  - A branch being force-deleted from origin (common after
    squash-merge auto-delete) leaves pilot's cached ref stale; the
    next `c` fails unless we fall back to the local ref.

**Rewrite rule:** the worktree subsystem needs first-class awareness
of stack graphs. Operations should be stack-aware:
  - "Update A" → optionally offer to rebase the stack's children.
  - Creating a child worktree should reuse the parent's tip rather
    than re-fetching if the parent is already local.
  - Remote-tracking ref + local-branch fallback (what git-ops does
    now) needs to be the explicit contract, not an accident.

### Local-branch collision

Today `git worktree add <path> <branch>` fails if the branch is
checked out elsewhere, even when "elsewhere" is a stale/irrelevant
worktree. We work around it by fetching to a remote-tracking ref and
`-B <branch>` the new worktree. That's fragile — it relies on no
other process claiming the local branch ref at the same time.

**Rewrite rule:** pilot is the sole writer for worktree layout. On
startup it reconciles `git worktree list` against its session database,
moves / renames / removes anything that doesn't match, and refuses to
proceed until the view is consistent. No "silently work around git's
weirdness" — assert the invariant.

## Open questions

- **Persistence granularity.** Per-component `Persist` or a single
  serialized root? Leaning per-component to avoid the "edit one field
  breaks the whole save" pattern.
- **Overlay ownership.** Modal overlays (Help, Picker, NewWorktree)
  could be children of the focused component OR a separate
  OverlayStack at the app root. App root feels cleaner; consistent
  with how Help/quick-reply work today.
- **Terminal rendering.** Current `tui-term` is solid — keep as-is.
  The Terminal component wraps a `TermSession`, subscribes to its
  output, and renders via libghostty-vt.
- **Backpressure.** If the bus is tokio channels, bounded or
  unbounded? Bounded is safer but adds "what do you drop" decisions.
  Unbounded is fine for now; revisit if memory creeps.
