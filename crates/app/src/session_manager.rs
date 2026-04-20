use std::collections::BTreeMap;

use pilot_core::Session;
use pilot_store::Store;

/// Owns all session-related state and enforces invariants
/// (e.g. `order` always stays in sync with `sessions`).
pub struct SessionManager {
    sessions: BTreeMap<String, Session>,
    order: Vec<String>,
}

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

    #[allow(dead_code)]
    pub fn order_mut(&mut self) -> &mut Vec<String> {
        &mut self.order
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

    /// Load sessions from store.
    pub fn load_from_store(&mut self, store: &dyn Store) {
        if let Ok(records) = store.list_sessions() {
            for record in records {
                if let Some(json) = &record.session_json {
                    if let Ok(session) = serde_json::from_str::<Session>(json) {
                        self.insert(record.task_id.clone(), session);
                    }
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
