use chrono::{DateTime, Utc};
use pilot_core::WorkspaceKey;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Backend(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// A persisted workspace record — full workspace data (PR + linked
/// issues + worktree path + activity + read state) serialized as JSON,
/// keyed by `WorkspaceKey`.
#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub key: String,
    pub created_at: DateTime<Utc>,
    /// JSON of `pilot_core::Workspace`.
    pub workspace_json: Option<String>,
}

/// Abstract storage trait. Implement for SQLite, Postgres, file, etc.
///
/// The kv methods (`get_kv` / `set_kv` / `delete_kv`) are for
/// daemon-side configuration: setup outcomes, ad-hoc preferences,
/// future workspace settings. Default impls behave as a never-stored
/// kv (None / Ok) so simple stores don't need to implement them.
pub trait Store: Send + Sync {
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

    // ── Workspaces ──────────────────────────────────────────────────
    //
    // Defaults piggy-back on the kv table (`workspace:<key>` → JSON).
    // Concrete stores can override for native indexes; simple kv-only
    // stores get workspace methods for free without overrides.

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
