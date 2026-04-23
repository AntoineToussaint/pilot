use std::collections::BTreeMap;

use pilot_core::Session;
use pilot_store::Store;

/// Owns all session-related state and enforces invariants
/// (e.g. `order` always stays in sync with `sessions`).
pub struct SessionManager {
    sessions: BTreeMap<String, Session>,
    order: Vec<String>,
}

#[allow(dead_code)]
impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            order: Vec::new(),
        }
    }

    // ── Accessors ────────────────────────────────────────────────────────

    pub fn get(&self, key: &str) -> Option<&Session> {
        self.sessions.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Session> {
        self.sessions.get_mut(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Session)> {
        self.sessions.iter()
    }

    pub fn values(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    pub fn order(&self) -> &[String] {
        &self.order
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.sessions.contains_key(key)
    }

    // ── Mutations — enforce invariants ────────────────────────────────────

    pub fn insert(&mut self, key: String, session: Session) {
        if !self.order.contains(&key) {
            self.order.push(key.clone());
        }
        self.sessions.insert(key, session);
    }

    pub fn remove(&mut self, key: &str) {
        self.sessions.remove(key);
        self.order.retain(|k| k != key);
    }

    pub fn sort_by_updated(&mut self) {
        self.order.sort_by(|a, b| {
            let sa = self.sessions.get(a);
            let sb = self.sessions.get(b);
            match (sa, sb) {
                (Some(sa), Some(sb)) => sb.primary_task.updated_at.cmp(&sa.primary_task.updated_at),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });
    }

    /// Load sessions from store, skipping merged/closed.
    pub fn load_from_store(&mut self, store: &dyn Store) {
        if let Ok(records) = store.list_sessions() {
            for record in records {
                if let Some(json) = &record.session_json
                    && let Ok(mut session) = serde_json::from_str::<Session>(json) {
                        // Skip merged/closed — they're done.
                        if matches!(
                            session.primary_task.state,
                            pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
                        ) {
                            let _ = store.delete_session(&session.task_id);
                            continue;
                        }
                        // If a session was persisted mid-checkout, the git
                        // process that was supposed to land it in Active is
                        // long gone. Don't boot into a spinner that nothing
                        // can resolve — reset to Active and let the user
                        // retry with `c` or delete it with `Shift-X`.
                        if matches!(session.state, pilot_core::SessionState::CheckingOut) {
                            tracing::info!(
                                "Resetting stuck CheckingOut session on load: {}",
                                session.task_id
                            );
                            session.state = pilot_core::SessionState::Active;
                        }
                        self.insert(record.task_id.clone(), session);
                    }
            }
        }
    }

    /// Save all sessions to store.
    pub fn save_all(&self, store: &dyn Store) {
        let mut saved = 0;
        let mut errors = 0;
        for (key, session) in &self.sessions {
            let json = serde_json::to_string(session).ok();
            match store.save_session(&pilot_store::SessionRecord {
                task_id: key.clone(),
                seen_count: session.seen_count as i64,
                last_viewed_at: session.last_viewed_at,
                created_at: session.created_at,
                session_json: json,
                metadata: None,
            }) {
                Ok(()) => saved += 1,
                Err(e) => {
                    tracing::error!("Failed to save session {key}: {e}");
                    errors += 1;
                }
            }
        }
        tracing::info!("Saved {saved} sessions ({errors} errors)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState,
    };

    fn task(title: &str, updated: chrono::DateTime<chrono::Utc>) -> Task {
        Task {
            id: TaskId { source: "test".into(), key: title.into() },
            title: title.into(), body: None,
            state: TaskState::Open, role: TaskRole::Author,
            ci: CiStatus::None, review: ReviewStatus::None,
            checks: vec![], unread_count: 0,
            url: format!("https://github.com/o/r/pull/{title}"),
            repo: Some("o/r".into()), branch: Some("f".into()),
            base_branch: None,
            updated_at: updated,
            labels: vec![], reviewers: vec![], assignees: vec![],
            auto_merge_enabled: false, is_in_merge_queue: false, has_conflicts: false,
            is_behind_base: false, node_id: None,
            needs_reply: false, last_commenter: None,
            recent_activity: vec![], additions: 0, deletions: 0,
        }
    }

    #[test]
    fn insert_dedups_order() {
        let mut m = SessionManager::new();
        m.insert("a".into(), Session::new_at(task("a", chrono::Utc::now()), chrono::Utc::now()));
        m.insert("a".into(), Session::new_at(task("a", chrono::Utc::now()), chrono::Utc::now()));
        assert_eq!(m.len(), 1);
        assert_eq!(m.order(), &["a"]);
    }

    #[test]
    fn remove_is_atomic() {
        let mut m = SessionManager::new();
        m.insert("a".into(), Session::new_at(task("a", chrono::Utc::now()), chrono::Utc::now()));
        m.insert("b".into(), Session::new_at(task("b", chrono::Utc::now()), chrono::Utc::now()));
        m.remove("a");
        assert!(!m.contains_key("a"));
        assert_eq!(m.order(), &["b"]);
    }

    #[test]
    fn remove_missing_is_noop() {
        let mut m = SessionManager::new();
        m.insert("a".into(), Session::new_at(task("a", chrono::Utc::now()), chrono::Utc::now()));
        m.remove("does-not-exist");
        assert!(m.contains_key("a"));
    }

    #[test]
    fn sort_by_updated_orders_newest_first() {
        let mut m = SessionManager::new();
        let older = chrono::Utc::now() - chrono::Duration::hours(2);
        let newer = chrono::Utc::now();
        m.insert("old".into(), Session::new_at(task("old", older), chrono::Utc::now()));
        m.insert("new".into(), Session::new_at(task("new", newer), chrono::Utc::now()));
        m.sort_by_updated();
        assert_eq!(m.order(), &["new", "old"]);
    }

    #[test]
    fn order_stays_in_sync_after_mixed_ops() {
        // Regression guard — `order` and `sessions` must never diverge.
        let mut m = SessionManager::new();
        m.insert("a".into(), Session::new_at(task("a", chrono::Utc::now()), chrono::Utc::now()));
        m.insert("b".into(), Session::new_at(task("b", chrono::Utc::now()), chrono::Utc::now()));
        m.insert("c".into(), Session::new_at(task("c", chrono::Utc::now()), chrono::Utc::now()));
        m.remove("b");
        m.insert("b".into(), Session::new_at(task("b2", chrono::Utc::now()), chrono::Utc::now()));
        // Every ordered key must be gettable; every gettable key must be ordered.
        for k in m.order() {
            assert!(m.get(k).is_some(), "order key {k} has no session");
        }
        let mut ordered = m.order().to_vec();
        ordered.sort();
        let mut mapped: Vec<String> = m.iter().map(|(k, _)| k.clone()).collect();
        mapped.sort();
        assert_eq!(ordered, mapped);
    }
}
