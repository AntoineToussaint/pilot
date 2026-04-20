use std::collections::BTreeMap;

use pilot_tui_term::TermSession;

use crate::action::ShellKind;

/// Owns all terminal-related state (TermSessions, kinds, tabs) and
/// enforces invariants — e.g. closing a terminal cleans up ALL maps.
pub struct TerminalManager {
    terminals: BTreeMap<String, TermSession>,
    kinds: BTreeMap<String, ShellKind>,
    tab_order: Vec<String>,
    active_tab: usize,
}

#[allow(dead_code)]
impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: BTreeMap::new(),
            kinds: BTreeMap::new(),
            tab_order: Vec::new(),
            active_tab: 0,
        }
    }

    // ── Accessors ────────────────────────────────────────────────────────

    pub fn get(&self, key: &str) -> Option<&TermSession> {
        self.terminals.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut TermSession> {
        self.terminals.get_mut(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.terminals.contains_key(key)
    }

    pub fn kind(&self, key: &str) -> Option<&ShellKind> {
        self.kinds.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &TermSession)> {
        self.terminals.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut TermSession)> {
        self.terminals.iter_mut()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.terminals.keys()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut TermSession> {
        self.terminals.values_mut()
    }

    pub fn is_empty(&self) -> bool {
        self.terminals.is_empty()
    }

    pub fn len(&self) -> usize {
        self.terminals.len()
    }

    pub fn tab_order(&self) -> &[String] {
        &self.tab_order
    }

    pub fn active_tab(&self) -> usize {
        self.active_tab
    }

    pub fn set_active_tab(&mut self, idx: usize) {
        if idx < self.tab_order.len() {
            self.active_tab = idx;
        }
    }

    pub fn active_tab_key(&self) -> Option<&String> {
        self.tab_order.get(self.active_tab)
    }

    // ── Mutations ────────────────────────────────────────────────────────

    pub fn insert(&mut self, key: String, term: TermSession, kind: ShellKind) {
        self.terminals.insert(key.clone(), term);
        self.kinds.insert(key.clone(), kind);
        if !self.tab_order.contains(&key) {
            self.tab_order.push(key);
        }
        self.active_tab = self.tab_order.len() - 1;
    }

    /// Close a terminal and clean up ALL associated state.
    /// Returns true if the terminal existed.
    pub fn close(&mut self, key: &str) -> bool {
        let existed = self.terminals.remove(key).is_some();
        self.kinds.remove(key);
        self.tab_order.retain(|k| k != key);
        if self.active_tab >= self.tab_order.len() && !self.tab_order.is_empty() {
            self.active_tab = self.tab_order.len() - 1;
        }
        existed
    }

    /// Process all pending PTY output.
    pub fn process_pending(&mut self) {
        for term in self.terminals.values_mut() {
            term.process_pending();
        }
    }

    /// Collect and remove finished terminals. Returns their keys.
    pub fn collect_finished(&mut self) -> Vec<String> {
        let exited: Vec<String> = self
            .terminals
            .iter()
            .filter(|(_, t)| t.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for key in &exited {
            self.close(key);
        }
        exited
    }

    /// Cycle to the next tab. Returns the new active index.
    pub fn next_tab(&mut self) -> usize {
        if !self.tab_order.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tab_order.len();
        }
        self.active_tab
    }

    /// Cycle to the previous tab. Returns the new active index.
    pub fn prev_tab(&mut self) -> usize {
        if !self.tab_order.is_empty() {
            self.active_tab =
                (self.active_tab + self.tab_order.len() - 1) % self.tab_order.len();
        }
        self.active_tab
    }
}
