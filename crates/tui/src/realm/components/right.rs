//! `Right` — tuirealm wrapper around `crate::components::right_pane::RightPane`.
//!
//! Same pattern as the sidebar wrapper: hold an instance of the
//! existing pilot pane and delegate `view`/`on_event`/`handle_key`
//! through UFCS.

use crate::components::right_pane::RightPane as PilotRight;
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

/// tuirealm-shaped right pane.
pub struct Right {
    inner: PilotRight,
    focused: bool,
    pending_cmds: Vec<IpcCommand>,
}

impl Right {
    /// Construct.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: PilotRight::new(id),
            focused: false,
            pending_cmds: Vec::new(),
        }
    }

    /// Set the workspace whose details + activity feed the pane
    /// renders. Called from `Model::update` after sidebar selection
    /// changes.
    pub fn set_workspace(&mut self, workspace: Option<pilot_core::Workspace>) {
        self.inner.set_workspace(workspace);
    }

    /// Drain queued IPC commands.
    pub fn drain_cmds(&mut self) -> Vec<IpcCommand> {
        std::mem::take(&mut self.pending_cmds)
    }

    /// Drive the auto-mark-on-hover timer; the orchestrator calls
    /// this each tick. Returns the `(SessionKey, index)` to mark read
    /// when the timer fires, otherwise None.
    pub fn tick(&mut self) -> Option<(pilot_core::SessionKey, usize)> {
        self.inner.tick(self.focused)
    }

    /// Forward a daemon event so the inner pane can refresh.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt,
        );
    }

    /// Direct render entry point. See `Sidebar::view_in`.
    pub fn view_in(&mut self, area: Rect, frame: &mut Frame) {
        self.inner.render(area,
            frame,
            self.focused,
        );
    }

    /// Direct key dispatch.
    pub fn handle_key_direct(
        &mut self,
        key: crossterm::event::KeyEvent,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let _ = self.inner.handle_key(key,
            cmds,
        );
    }

    /// Update the focused-flag.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Forward to the inner pane's keymap so the help panel can list
    /// the same bindings the legacy hint bar showed.
    pub fn keymap(&self) -> &'static [crate::pane::Binding] {
        self.inner.keymap()
    }

    /// Forward to the inner pane's detach spec, if any.
    pub fn detachable(&self) -> Option<crate::pane::DetachSpec> {
        self.inner.detachable()
    }
}

impl Component for Right {
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

impl AppComponent<Msg, UserEvent> for Right {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::User(UserEvent::Daemon(evt)) => {
                self.inner.on_event(evt,
                );
                None
            }
            Event::Keyboard(key) if self.focused => {
                let ct_key = realm_key_to_crossterm(key);
                let mut cmds: Vec<IpcCommand> = Vec::new();
                let _ = self.inner.handle_key(ct_key,
                    &mut cmds,
                );
                if !cmds.is_empty() {
                    self.pending_cmds.extend(cmds);
                    return Some(Msg::RightCmds);
                }
                None
            }
            _ => None,
        }
    }
}
