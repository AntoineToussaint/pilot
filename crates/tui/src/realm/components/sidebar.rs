//! `Sidebar` — tuirealm wrapper around pilot's existing
//! `crate::components::sidebar::Sidebar`.
//!
//! Pilot's sidebar is ~1.4k LOC of bespoke render logic (workspace
//! rows with role badges, status pills, runner badges, mailbox
//! cycling, time column, …). Rather than copying it, this wrapper
//! holds an instance and delegates `view` + `on` through to the
//! existing `Pane` impl via UFCS.
//!
//! ## Why this is the right shape during the migration
//!
//! The end-state lifts pilot's `impl tui_kit::Pane for Sidebar` body
//! into inherent methods (or a free `Sidebar::handle_key` /
//! `::render` / `::on_event`). That conversion is a one-shot
//! mechanical edit we can do once the kit is deleted. Until then,
//! UFCS keeps both code paths alive.

use crate::components::sidebar::Sidebar as PilotSidebar;
use crate::realm::keymap::realm_key_to_crossterm;
use crate::realm::{Msg, UserEvent};
use crate::PaneId;
use pilot_ipc::Command as IpcCommand;
use pilot_ipc::Event as IpcEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

/// Wrap pilot's existing Sidebar so it can be mounted into a
/// tuirealm `Application`.
pub struct Sidebar {
    inner: PilotSidebar,
    /// Whether this pane is the focused one. tuirealm sets it via
    /// the `Attribute::Focus` flag.
    focused: bool,
    /// Outbound commands queued by `handle_key`. We drain them in the
    /// `Model::update` arm for `Msg::SidebarCmds(...)` and forward
    /// them to the daemon.
    pending_cmds: Vec<IpcCommand>,
}

impl Sidebar {
    /// Construct using the same `PaneId` the existing pilot sidebar
    /// uses, so detach specs + helper lookups continue to match.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: PilotSidebar::new(id),
            focused: true, // sidebar is the default-focused pane
            pending_cmds: Vec::new(),
        }
    }

    /// Drain any commands the inner sidebar pushed in response to a
    /// recent `handle_key`. Caller forwards to the daemon.
    pub fn drain_cmds(&mut self) -> Vec<IpcCommand> {
        std::mem::take(&mut self.pending_cmds)
    }

    /// Forward an incoming daemon event to the inner sidebar so its
    /// workspace map / live-terminal tracking stays in sync.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt,
        );
    }

    /// Render directly into a rect — orchestrator-friendly entry
    /// point that bypasses tuirealm's mount/active dance for panes.
    pub fn view_in(&mut self, area: Rect, frame: &mut Frame) {
        self.inner.render(area,
            frame,
            self.focused,
        );
    }

    /// Direct (non-tuirealm) key dispatch. The orchestrator calls
    /// this after Tab routing is resolved.
    pub fn handle_key_direct(
        &mut self,
        key: crossterm::event::KeyEvent,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let _ = self.inner.handle_key(key,
            cmds,
        );
    }

    /// Update the focused-flag (drives border / cursor styling).
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Read currently selected workspace key (for selection projection).
    pub fn selected_workspace_key(&self) -> Option<&pilot_core::SessionKey> {
        self.inner.selected_session_key()
    }

    /// Read the full Workspace under the cursor (for projection into
    /// `Right::set_workspace`).
    pub fn selected_workspace(&self) -> Option<&pilot_core::Workspace> {
        self.inner.selected_workspace()
    }

    /// Move the cursor onto the workspace whose key matches.
    /// Returns true if found.
    pub fn focus_workspace_key(&mut self, key: &pilot_core::SessionKey) -> bool {
        self.inner.focus_workspace_key(key)
    }

    /// Move the cursor onto the named session sub-row. Caller is
    /// expected to have first selected the parent workspace.
    pub fn focus_session_id(&mut self, id: pilot_core::SessionId) -> bool {
        self.inner.focus_session_id(id)
    }

    /// Forward to the inner pane's keymap so the help panel can list
    /// the same bindings the legacy hint bar showed.
    pub fn keymap(&self) -> &'static [crate::pane::Binding] {
        self.inner.keymap()
    }

    /// Click-to-select a row. Returns true on a hit.
    pub fn click_to_select(&mut self, area: Rect, click_row: u16) -> bool {
        self.inner.click_to_select(area, click_row)
    }

    /// Forward to the inner pane's detach spec, if any.
    pub fn detachable(&self) -> Option<crate::pane::DetachSpec> {
        self.inner.detachable()
    }
}

impl Component for Sidebar {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.inner.render(area,
            frame,
            self.focused,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }

    fn attr(&mut self, attr: Attribute, value: AttrValue) {
        if let (Attribute::Focus, AttrValue::Flag(f)) = (attr, value) {
            self.focused = f;
        }
    }

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Sidebar {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            // Daemon events route through the inner pilot sidebar's
            // `on_event` so its `workspaces` map + `running_terminals`
            // stay current.
            Event::User(UserEvent::Daemon(evt)) => {
                self.inner.on_event(evt,
                );
                None
            }
            Event::Keyboard(key) if self.focused => {
                // Translate tuirealm KeyEvent → crossterm KeyEvent so
                // we can delegate to the existing `handle_key`.
                let ct_key = realm_key_to_crossterm(key);
                let mut cmds: Vec<IpcCommand> = Vec::new();
                let outcome = self.inner.handle_key(ct_key,
                    &mut cmds,
                );
                if !cmds.is_empty() {
                    self.pending_cmds.extend(cmds);
                    return Some(Msg::SidebarCmds);
                }
                let _ = outcome;
                None
            }
            _ => None,
        }
    }
}

