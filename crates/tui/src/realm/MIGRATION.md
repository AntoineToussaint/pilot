# Pilot · tuirealm migration

## Status: complete + cleaned up

Pilot is realm-native end-to-end. The `tui-kit` and `realm-probe`
crates have been removed; pilot's binary depends on `tuirealm 4.1`
and pilot's own `theme.rs` / `pane.rs` modules.

```sh
cargo run -p pilot-tui          # default
cargo run -p pilot-tui -- --fresh   # wipe ~/.pilot/v2/state.db, re-run setup
cargo run -p pilot-tui -- --test    # throwaway tempdir, no GitHub
```

## What landed

- **8 modals + polling** ported to `tuirealm`: `Splash`, `Confirm`,
  `Input`, `Error`, `Help`, `Loading`, `Textarea`, `Choice`,
  `Polling`.
- **3 panes** wrapped: `Sidebar`, `Right`, `Terminals`. Wrappers hold
  pilot's existing pane structs and call inherent methods directly.
  Pilot's `impl tui_kit::Pane` blocks were lifted to inherent methods
  before deletion.
- **Orchestrator** (`Model`): typed pane fields + `Application` for
  modals. Tab cycle, focus tracking, daemon event broadcast, IPC
  forwarding, layout (40% sidebar, 25/75% right column).
- **Setup wizard** (`SetupRunner`): realm-native state machine that
  drives `Splash → Providers → Agents → Filters → Scopes → Repos`
  using realm `Choice<T>` / `Loading` / `ErrorModal` directly. No
  more kit-modal adapter.
- **`!Send` libghostty embed** validated empirically and lives on —
  `AppComponent` only requires `Any + 'static`, not `Send`.
- **Theme + `PaneId`** moved into pilot at `crate::theme` and
  `crate::pane` so `tui-kit` could be deleted entirely.

## What's gone

- `crates/tui-kit/` — deleted.
- `crates/realm-probe/` — deleted.
- `crates/tui/src/app.rs` — the legacy run loop.
- `crates/tui/src/components/{splash,polling_modal,help}.rs` — legacy
  kit-modal versions; realm equivalents live in
  `crates/tui/src/realm/components/`.
- `crates/tui/tests/app_loop.rs` — drove the legacy `App` struct.
  Surviving coverage: `sidebar.rs`, `right_pane.rs`,
  `terminal_stack.rs`, `snapshots.rs` exercise the same pane structs
  via their inherent methods; `setup_flow.rs` lib tests cover the
  wizard state machine.
- `--legacy` flag, `--connect` mode, `KitModalAdapter`,
  `realm/setup.rs` stub.

## What stayed

- Pilot's three pane structs (`Sidebar`, `RightPane`,
  `TerminalStack`) — domain code, called via inherent methods from
  the realm wrappers.
- `setup_flow.rs` — kept the file path, but it's now the realm-native
  `SetupRunner` + types.
