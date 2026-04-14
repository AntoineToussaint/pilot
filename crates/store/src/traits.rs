use chrono::{DateTime, Utc};
use pilot_core::TaskId;

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

/// Abstract storage trait. Implement for SQLite, Postgres, file, etc.
pub trait Store: Send + Sync {
    fn get_session(&self, task_id: &TaskId) -> Result<Option<SessionRecord>, StoreError>;
    fn save_session(&self, record: &SessionRecord) -> Result<(), StoreError>;
    fn mark_read(&self, task_id: &TaskId, seen_count: i64) -> Result<(), StoreError>;
    fn list_sessions(&self) -> Result<Vec<SessionRecord>, StoreError>;
    fn delete_session(&self, task_id: &TaskId) -> Result<(), StoreError>;
}
