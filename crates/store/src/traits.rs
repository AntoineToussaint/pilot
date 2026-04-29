use chrono::{DateTime, Utc};
use pilot_core::{TaskId, WorkspaceKey};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Backend(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// A persisted session record — full task data + read/unread state.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub task_id: String,
    pub seen_count: i64,
    pub last_viewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Full serialized session data (JSON). Includes task, activity, etc.
    pub session_json: Option<String>,
    /// Arbitrary metadata (for future use).
    pub metadata: Option<String>,
}

/// A persisted workspace record — full v2 workspace data (PR + linked
/// issues + worktree path + activity + read state) serialized as JSON.
/// Lives alongside `SessionRecord` so v1 stays untouched: anything
/// reading sessions still gets sessions, and workspaces are a new
/// surface keyed by `WorkspaceKey`.
#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub key: String,
    pub created_at: DateTime<Utc>,
    /// JSON of `pilot_core::Workspace`. Always populated for v2-written
    /// rows; `None` only for legacy / partially-migrated entries.
    pub workspace_json: Option<String>,
}

/// Abstract storage trait. Implement for SQLite, Postgres, file, etc.
///
/// The kv methods (`get_kv` / `set_kv` / `delete_kv`) are for v2's
/// daemon-side configuration: setup outcomes, ad-hoc preferences,
/// future workspace settings. v1 doesn't use them; default impls
/// behave as a never-stored kv (None / Ok) so existing v1 stores
/// don't need code changes.
pub trait Store: Send + Sync {
    fn get_session(&self, task_id: &TaskId) -> Result<Option<SessionRecord>, StoreError>;
    fn save_session(&self, record: &SessionRecord) -> Result<(), StoreError>;
    fn mark_read(&self, task_id: &TaskId, seen_count: i64) -> Result<(), StoreError>;
    fn list_sessions(&self) -> Result<Vec<SessionRecord>, StoreError>;
    fn delete_session(&self, task_id: &TaskId) -> Result<(), StoreError>;

    /// Read a string value previously set with `set_kv`. Returns
    /// `Ok(None)` for both "never set" and the default impl, so
    /// callers should treat None as "use defaults".
    fn get_kv(&self, _key: &str) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    /// Write a string value. Concrete stores persist it; the default
    /// drops it on the floor (test stubs / read-only stores).
    fn set_kv(&self, _key: &str, _value: &str) -> Result<(), StoreError> {
        Ok(())
    }

    /// Remove a kv entry. Idempotent — missing key is not an error.
    fn delete_kv(&self, _key: &str) -> Result<(), StoreError> {
        Ok(())
    }

    // ── v2 Workspaces ───────────────────────────────────────────────
    //
    // Defaults piggy-back on the kv table (`workspace:<key>` → JSON).
    // Concrete stores can override for native indexes; v1 stores get
    // workspace methods for free without code changes.

    fn get_workspace(
        &self,
        key: &WorkspaceKey,
    ) -> Result<Option<WorkspaceRecord>, StoreError> {
        let kv_key = format!("workspace:{}", key.as_str());
        let Some(json) = self.get_kv(&kv_key)? else {
            return Ok(None);
        };
        Ok(Some(WorkspaceRecord {
            key: key.as_str().to_string(),
            created_at: Utc::now(),
            workspace_json: Some(json),
        }))
    }

    fn save_workspace(&self, record: &WorkspaceRecord) -> Result<(), StoreError> {
        let kv_key = format!("workspace:{}", record.key);
        let json = record.workspace_json.clone().unwrap_or_default();
        self.set_kv(&kv_key, &json)
    }

    fn delete_workspace(&self, key: &WorkspaceKey) -> Result<(), StoreError> {
        self.delete_kv(&format!("workspace:{}", key.as_str()))
    }

    /// List every workspace the store knows about. Default impl
    /// returns empty — concrete stores should override and scan the
    /// kv table for `workspace:*` prefixes.
    fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, StoreError> {
        Ok(Vec::new())
    }
}
