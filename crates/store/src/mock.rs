//! In-memory Store implementation for tests.

use crate::{SessionRecord, Store, StoreError};
use pilot_core::TaskId;
use std::collections::HashMap;
use std::sync::Mutex;

/// A simple in-memory store for unit tests.
pub struct MemoryStore {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    kv: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            kv: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionRecord>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn kv_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.kv.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Store for MemoryStore {
    fn get_session(&self, task_id: &TaskId) -> Result<Option<SessionRecord>, StoreError> {
        let key = task_id.to_string();
        Ok(self.lock().get(&key).cloned())
    }

    fn save_session(&self, record: &SessionRecord) -> Result<(), StoreError> {
        self.lock().insert(record.task_id.clone(), record.clone());
        Ok(())
    }

    fn mark_read(&self, task_id: &TaskId, seen_count: i64) -> Result<(), StoreError> {
        let key = task_id.to_string();
        if let Some(record) = self.lock().get_mut(&key) {
            record.seen_count = seen_count;
        }
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<SessionRecord>, StoreError> {
        Ok(self.lock().values().cloned().collect())
    }

    fn delete_session(&self, task_id: &TaskId) -> Result<(), StoreError> {
        let key = task_id.to_string();
        self.lock().remove(&key);
        Ok(())
    }

    fn get_kv(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.kv_lock().get(key).cloned())
    }

    fn set_kv(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.kv_lock().insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete_kv(&self, key: &str) -> Result<(), StoreError> {
        self.kv_lock().remove(key);
        Ok(())
    }

    /// In-memory prefix scan over the kv table. Mirrors what
    /// `SqliteStore::list_workspaces` does so tests using
    /// `MemoryStore` see the same behavior.
    fn list_workspaces(&self) -> Result<Vec<crate::WorkspaceRecord>, StoreError> {
        let kv = self.kv_lock();
        let mut out = Vec::new();
        for (key, value) in kv.iter() {
            if let Some(stripped) = key.strip_prefix("workspace:") {
                out.push(crate::WorkspaceRecord {
                    key: stripped.to_string(),
                    created_at: chrono::Utc::now(),
                    workspace_json: Some(value.clone()),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_save_and_get() {
        let store = MemoryStore::new();
        let tid = TaskId {
            source: "test".into(),
            key: "repo#1".into(),
        };
        let record = SessionRecord {
            task_id: tid.to_string(),
            seen_count: 5,
            last_viewed_at: Some(Utc::now()),
            created_at: Utc::now(),
            session_json: Some("{}".into()),
            metadata: None,
        };
        store.save_session(&record).unwrap();

        let got = store.get_session(&tid).unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().seen_count, 5);
    }

    #[test]
    fn test_list_and_delete() {
        let store = MemoryStore::new();
        let tid = TaskId {
            source: "test".into(),
            key: "repo#1".into(),
        };
        let record = SessionRecord {
            task_id: tid.to_string(),
            seen_count: 0,
            last_viewed_at: None,
            created_at: Utc::now(),
            session_json: None,
            metadata: None,
        };
        store.save_session(&record).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);

        store.delete_session(&tid).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 0);
    }

    #[test]
    fn test_mark_read() {
        let store = MemoryStore::new();
        let tid = TaskId {
            source: "test".into(),
            key: "repo#1".into(),
        };
        let record = SessionRecord {
            task_id: tid.to_string(),
            seen_count: 0,
            last_viewed_at: None,
            created_at: Utc::now(),
            session_json: None,
            metadata: None,
        };
        store.save_session(&record).unwrap();
        store.mark_read(&tid, 10).unwrap();

        let got = store.get_session(&tid).unwrap().unwrap();
        assert_eq!(got.seen_count, 10);
    }
}
