use chrono::Utc;
use pilot_core::TaskId;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

use crate::traits::{SessionRecord, Store, StoreError};

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Backend(e.to_string()))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let conn =
            Connection::open_in_memory().map_err(|e| StoreError::Backend(e.to_string()))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn default_path() -> Result<Self, StoreError> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = std::path::PathBuf::from(home).join(".pilot");
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Backend(e.to_string()))?;
        Self::open(dir.join("state.db"))
    }

    fn migrate(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                task_id       TEXT PRIMARY KEY,
                seen_count    INTEGER NOT NULL DEFAULT 0,
                last_viewed_at TEXT,
                created_at    TEXT NOT NULL,
                session_json  TEXT,
                metadata      TEXT
            );",
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;

        // Add session_json column if upgrading from older schema.
        let _ = conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN session_json TEXT;",
        );

        Ok(())
    }
}

impl Store for SqliteStore {
    fn get_session(&self, task_id: &TaskId) -> Result<Option<SessionRecord>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let key = task_id.to_string();
        let mut stmt = conn
            .prepare(
                "SELECT task_id, seen_count, last_viewed_at, created_at, session_json, metadata
                 FROM sessions WHERE task_id = ?1",
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        let result = stmt
            .query_row([&key], |row| {
                Ok(SessionRecord {
                    task_id: row.get(0)?,
                    seen_count: row.get(1)?,
                    last_viewed_at: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| s.parse().ok()),
                    created_at: row
                        .get::<_, String>(3)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    session_json: row.get(4)?,
                    metadata: row.get(5)?,
                })
            })
            .optional()
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        Ok(result)
    }

    fn save_session(&self, record: &SessionRecord) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (task_id, seen_count, last_viewed_at, created_at, session_json, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(task_id) DO UPDATE SET
                seen_count = excluded.seen_count,
                last_viewed_at = excluded.last_viewed_at,
                session_json = excluded.session_json,
                metadata = excluded.metadata",
            (
                &record.task_id,
                record.seen_count,
                record.last_viewed_at.map(|t| t.to_rfc3339()),
                record.created_at.to_rfc3339(),
                &record.session_json,
                &record.metadata,
            ),
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    fn mark_read(&self, task_id: &TaskId, seen_count: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let key = task_id.to_string();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE sessions SET seen_count = ?1, last_viewed_at = ?2 WHERE task_id = ?3",
            (&seen_count, &now, &key),
        )
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<SessionRecord>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT task_id, seen_count, last_viewed_at, created_at, session_json, metadata
                 FROM sessions",
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRecord {
                    task_id: row.get(0)?,
                    seen_count: row.get(1)?,
                    last_viewed_at: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| s.parse().ok()),
                    created_at: row
                        .get::<_, String>(3)?
                        .parse()
                        .unwrap_or_else(|_| Utc::now()),
                    session_json: row.get(4)?,
                    metadata: row.get(5)?,
                })
            })
            .map_err(|e| StoreError::Backend(e.to_string()))?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|e| StoreError::Backend(e.to_string()))?);
        }
        Ok(records)
    }

    fn delete_session(&self, task_id: &TaskId) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        let key = task_id.to_string();
        conn.execute("DELETE FROM sessions WHERE task_id = ?1", [&key])
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }
}

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
