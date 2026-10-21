use super::SessionStore;
use crate::sqlite_maint::{GarbageCollectionResult, VacuumOutcome, VacuumProgress};

/// Captures parked by the v53 migration until the background purge deletes
/// them. Renaming is O(1); deleting tens of GB inline blocked startup.
pub(crate) const LEGACY_CONTEXT_ATTEMPTS_TABLE: &str = "session_context_attempts_legacy";
/// Each batch frees ~10 MB of captures, so the writer is held only briefly.
const PURGE_BATCH_ROWS: i64 = 64;

impl SessionStore {
    /// Deletes the parked legacy captures in short writer transactions and
    /// drops the table. Returns the number of purged rows, or `None` when
    /// there was nothing to purge.
    pub(crate) fn purge_legacy_context_attempts(
        &self,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> Result<Option<u64>, String> {
        let total = {
            let conn = self.conn.lock()?;
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [LEGACY_CONTEXT_ATTEMPTS_TABLE],
                    |row| row.get(0),
                )
                .map_err(|e| format!("Failed to inspect legacy captures: {e}"))?;
            if !exists {
                return Ok(None);
            }
            // Freeing must not overwrite each deleted page in the WAL.
            conn.execute_batch("PRAGMA secure_delete=OFF;")
                .map_err(|e| format!("Failed to configure capture purge: {e}"))?;
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {LEGACY_CONTEXT_ATTEMPTS_TABLE}"),
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| format!("Failed to count legacy captures: {e}"))? as u64
        };
        let delete_batch = format!(
            "DELETE FROM {LEGACY_CONTEXT_ATTEMPTS_TABLE} WHERE rowid IN
             (SELECT rowid FROM {LEGACY_CONTEXT_ATTEMPTS_TABLE} LIMIT ?1)"
        );
        let mut done = 0u64;
        on_progress(done, total);
        loop {
            let deleted = self
                .conn
                .lock()?
                .execute(&delete_batch, [PURGE_BATCH_ROWS])
                .map_err(|e| format!("Failed to purge legacy captures: {e}"))?;
            if deleted == 0 {
                break;
            }
            done += deleted as u64;
            on_progress(done, total.max(done));
            // Let queued session writes take the writer between batches.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.conn
            .lock()?
            .execute_batch(&format!("DROP TABLE IF EXISTS {LEGACY_CONTEXT_ATTEMPTS_TABLE}"))
            .map_err(|e| format!("Failed to drop legacy captures: {e}"))?;
        Ok(Some(done))
    }

    pub(crate) fn vacuum_if_fragmented_with_progress(
        &self,
        on_progress: &(dyn Fn(VacuumProgress) + Sync),
    ) -> Result<VacuumOutcome, String> {
        let conn = self.conn.lock()?;
        crate::sqlite_maint::vacuum_if_fragmented_with_progress(
            &conn,
            crate::sqlite_maint::VACUUM_MIN_FREE_BYTES,
            crate::sqlite_maint::VACUUM_MIN_FREE_RATIO,
            on_progress,
        )
    }

    pub(crate) fn garbage_collect(&self) -> Result<GarbageCollectionResult, String> {
        let conn = self.conn.try_lock().map_err(|_| {
            "Session database is busy; retry when current operations finish.".to_string()
        })?;
        crate::sqlite_maint::garbage_collect(&conn, &self.db_path)
    }

    pub(crate) fn database_path(&self) -> &std::path::Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::context_export::export_session_context_yaml;
    use crate::session::models::MessageRole;

    #[test]
    fn garbage_collection_preserves_session_content_order_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let id = store
            .create_session("Keep this session", None, None, "chat", None)
            .unwrap();
        store
            .add_message(&id, MessageRole::User, "Keep the question")
            .unwrap();
        store
            .add_message(&id, MessageRole::Assistant, "Keep the answer")
            .unwrap();
        {
            let conn = store.conn.lock().unwrap();
            // Non-contiguous rowids are also used as message-page cursors.
            conn.execute_batch(
                "UPDATE messages SET rowid=rowid*10;
                CREATE TABLE gc_fixture (data BLOB);
                INSERT INTO gc_fixture VALUES(zeroblob(2097152));
                DELETE FROM gc_fixture;",
            )
            .unwrap();
        }
        let rows = || {
            let conn = store.conn.lock().unwrap();
            let mut statement = conn
                .prepare("SELECT rowid, id, role, content FROM messages ORDER BY rowid")
                .unwrap();
            statement
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let export = |filename: &str| {
            let path = dir.path().join(filename);
            export_session_context_yaml(&store, &id, "", None, None, &path).unwrap();
            serde_yaml::from_str::<serde_yaml::Value>(&std::fs::read_to_string(path).unwrap())
                .unwrap()
        };
        let before_rows = rows();
        let before_export = export("before.yaml");
        assert!(store.garbage_collect().unwrap().reclaimed_bytes > 0);
        assert_eq!(rows(), before_rows);
        let after_export = export("after.yaml");
        assert_eq!(before_export["sessions"], after_export["sessions"]);
        assert_eq!(
            after_export["sessions"][0]["context_attempts"].as_str(),
            Some("empty")
        );
        assert_eq!(store.load_session(&id).unwrap().messages.len(), 2);
    }
}
