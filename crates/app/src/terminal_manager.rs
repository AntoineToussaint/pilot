use std::collections::HashMap;

use pilot_tui_term::{Terminal, TermSession};

use crate::action::ShellKind;

/// Pure helper: given an iterator of `(key, &dyn Terminal)`, return the
/// keys whose underlying PTY has finished. Separated from the struct so
/// it's unit-testable with a fake Terminal.
pub(crate) fn finished_keys<'a, I, T>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = (&'a String, &'a T)>,
    T: Terminal + 'a,
{
    items
        .into_iter()
        .filter(|(_, t)| t.is_finished())
        .map(|(k, _)| k.clone())
        .collect()
}

/// Owns all terminal-related state (TermSessions, kinds, tabs) and
/// enforces invariants — e.g. closing a terminal cleans up ALL maps.
///
/// Uses `HashMap` rather than `BTreeMap` because we don't need key ordering
/// (tab order is tracked separately in `tab_order`) and O(1) lookup is
/// preferable on the hot key-to-terminal path.
pub struct TerminalManager {
    terminals: HashMap<String, TermSession>,
    kinds: HashMap<String, ShellKind>,
    tab_order: Vec<String>,
    active_tab: usize,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: HashMap::new(),
            kinds: HashMap::new(),
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

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.terminals.keys()
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
        let exited = finished_keys(self.terminals.iter());
        for key in &exited {
            self.close(key);
        }
        exited
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct FakeTerm {
        finished: bool,
    }

    impl Terminal for FakeTerm {
        fn is_finished(&self) -> bool {
            self.finished
        }
        fn process_pending(&mut self) {}
    }

    #[test]
    fn finished_keys_picks_out_exited() {
        let mut map: BTreeMap<String, FakeTerm> = BTreeMap::new();
        map.insert("alive".into(), FakeTerm { finished: false });
        map.insert("dead".into(), FakeTerm { finished: true });
        map.insert("alive2".into(), FakeTerm { finished: false });

        let exited = finished_keys(map.iter());
        assert_eq!(exited, vec!["dead".to_string()]);
    }

    #[test]
    fn finished_keys_empty_when_all_alive() {
        let mut map: BTreeMap<String, FakeTerm> = BTreeMap::new();
        map.insert("a".into(), FakeTerm { finished: false });
        map.insert("b".into(), FakeTerm { finished: false });
        assert!(finished_keys(map.iter()).is_empty());
    }

    #[test]
    fn finished_keys_all_when_all_dead() {
        let mut map: BTreeMap<String, FakeTerm> = BTreeMap::new();
        map.insert("a".into(), FakeTerm { finished: true });
        map.insert("b".into(), FakeTerm { finished: true });
        let mut got = finished_keys(map.iter());
        got.sort();
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
    }
}
