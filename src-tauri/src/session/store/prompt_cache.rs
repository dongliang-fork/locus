//! Server-usage based prompt-cache diagnostics. Billing remains unchanged: cache
//! writes are only combined with uncached input for the diagnostic calculation.
use super::*;
use crate::commands::SessionCacheDetection;

const ALGORITHM_VERSION: i64 = 2;

#[cfg(test)]
#[path = "prompt_cache_tests.rs"]
mod tests;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptCacheRequest {
    pub key: String,
    pub message_id: Option<String>,
    pub usage_complete: bool,
}

impl PromptCacheRequest {
    pub fn new(key: String, message_id: Option<String>) -> Self {
        Self { key, message_id, usage_complete: true }
    }
}

pub(crate) fn claude_prompt_usage_complete(usage: &serde_json::Value) -> bool {
    ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter().all(|key| usage[*key].as_u64().is_some())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptCacheCheckOutcome {
    pub baseline_tokens: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub excess_input_tokens: u64,
    pub invalidated: bool,
    pub reason: String,
}

// Also used when copying a session snapshot: don't silently drop diagnostic
// provenance, missing-data markers or compaction boundaries during a fork.
#[derive(Debug, Clone)]
pub(super) struct StoredPromptCacheCheck {
    request_key: String,
    request_sequence: i64,
    message_id: String,
    message: String,
    model_id: String,
    provider: String,
    baseline_tokens: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: Option<u64>,
    effective_context_tokens: u64,
    usage_complete: bool,
    ends_context: bool,
    excess_input_tokens: u64,
    invalidated: bool,
    reason: String,
    algorithm_version: i64,
    created_at: i64,
}

fn evaluate(
    previous: Option<&StoredPromptCacheCheck>, model: &str, provider: &str,
    input: u64, read: u64, write: u64, complete: bool,
) -> PromptCacheCheckOutcome {
    let baseline = previous.map_or(0, |p| p.effective_context_tokens);
    let context = input.saturating_add(read).saturating_add(write);
    let growth = context.saturating_sub(baseline);
    let excess = input.saturating_add(write).saturating_sub(growth);
    let (invalidated, reason) = if !complete || model.is_empty() {
        (false, "usage_unavailable")
    } else {
        match previous {
            None => (false, "no_baseline"),
            Some(p) if p.ends_context => (false, "context_reset"),
            Some(p) if !p.usage_complete || p.algorithm_version != ALGORITHM_VERSION => (false, "baseline_unavailable"),
            Some(p) if p.model_id != model => (true, "model_changed"),
            Some(p) if p.provider != provider => (true, "provider_changed"),
            Some(_) if baseline == 0 || context == 0 => (false, "no_baseline"),
            Some(_) if u128::from(excess) * 5 > u128::from(baseline) * 4 => (true, "uncached_prefix_exceeds_threshold"),
            Some(_) => (false, "below_threshold"),
        }
    };
    PromptCacheCheckOutcome {
        baseline_tokens: baseline, input_tokens: input, cache_read_tokens: read,
        excess_input_tokens: excess, invalidated, reason: reason.into(),
    }
}

fn canonical_model(model: &str, provider: &str) -> String {
    if provider == "Claude Code CLI" {
        format!("claude_code/{}", model.trim().strip_prefix("claude_code/").unwrap_or(model.trim()))
    } else { model.trim().to_owned() }
}

impl SessionStore {
    pub(super) fn create_request_cache_check_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_prompt_cache_checks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                request_key TEXT NOT NULL DEFAULT (lower(hex(randomblob(16)))),
                request_sequence INTEGER NOT NULL DEFAULT 0,
                message_id TEXT NOT NULL,
                message TEXT NOT NULL,
                model_id TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT '',
                baseline_tokens INTEGER NOT NULL,
                input_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                cache_write_tokens INTEGER,
                effective_context_tokens INTEGER NOT NULL DEFAULT 0,
                usage_complete INTEGER NOT NULL DEFAULT 0,
                ends_context INTEGER NOT NULL DEFAULT 0,
                excess_input_tokens INTEGER NOT NULL,
                invalidated INTEGER NOT NULL,
                reason TEXT NOT NULL,
                algorithm_version INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                UNIQUE(session_id, request_key)
            );
            CREATE INDEX IF NOT EXISTS idx_session_prompt_cache_checks_session
                ON session_prompt_cache_checks(session_id, id DESC);",
        )
    }

    pub(super) fn migrate_request_cache_checks(conn: &Connection) -> rusqlite::Result<()> {
        if Self::table_has_column(conn, "session_prompt_cache_checks", "request_key")? { return Ok(()); }
        conn.execute_batch("ALTER TABLE session_prompt_cache_checks RENAME TO legacy_prompt_cache_checks;
            DROP INDEX IF EXISTS idx_session_prompt_cache_checks_session;")?;
        Self::create_request_cache_check_schema(conn)?;
        conn.execute_batch(
            "INSERT INTO session_prompt_cache_checks (
                session_id, request_key, request_sequence, message_id, message, model_id,
                baseline_tokens, input_tokens, cache_read_tokens, excess_input_tokens,
                invalidated, reason, created_at, provider
            ) SELECT old.session_id, 'legacy:' || old.id,
                ROW_NUMBER() OVER (PARTITION BY old.session_id ORDER BY old.id),
                old.message_id, old.message, old.model_id, old.baseline_tokens,
                old.input_tokens, old.cache_read_tokens, old.excess_input_tokens,
                old.invalidated, old.reason, old.created_at,
                COALESCE((SELECT provider FROM model_usage_events usage
                    WHERE usage.session_id=old.session_id AND usage.model_id=old.model_id
                    ORDER BY usage.id DESC LIMIT 1), '')
              FROM legacy_prompt_cache_checks old ORDER BY old.id;
             DROP TABLE legacy_prompt_cache_checks;",
        )
    }

    pub(super) fn prompt_cache_checks_with_conn(conn: &Connection, session: &str) -> Result<Vec<StoredPromptCacheCheck>, String> {
        let mut stmt = conn.prepare("SELECT request_key, request_sequence, message_id, message, model_id,
            provider, baseline_tokens, input_tokens, cache_read_tokens, cache_write_tokens,
            effective_context_tokens, usage_complete, ends_context, excess_input_tokens,
            invalidated, reason, algorithm_version, created_at
            FROM session_prompt_cache_checks WHERE session_id=?1 ORDER BY id").map_err(|e| e.to_string())?;
        let rows = stmt.query_map([session], Self::prompt_cache_check_from_row).map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
    }

    fn prompt_cache_check_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPromptCacheCheck> {
        Ok(StoredPromptCacheCheck {
            request_key: row.get(0)?, request_sequence: row.get(1)?, message_id: row.get(2)?,
            message: row.get(3)?, model_id: row.get(4)?, provider: row.get(5)?,
            baseline_tokens: row.get::<_, i64>(6)?.max(0) as u64,
            input_tokens: row.get::<_, i64>(7)?.max(0) as u64,
            cache_read_tokens: row.get::<_, i64>(8)?.max(0) as u64,
            cache_write_tokens: row.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u64),
            effective_context_tokens: row.get::<_, i64>(10)?.max(0) as u64,
            usage_complete: row.get(11)?, ends_context: row.get(12)?,
            excess_input_tokens: row.get::<_, i64>(13)?.max(0) as u64,
            invalidated: row.get(14)?, reason: row.get(15)?, algorithm_version: row.get(16)?, created_at: row.get(17)?,
        })
    }

    pub(super) fn insert_prompt_cache_check(conn: &Connection, session: &str, check: &StoredPromptCacheCheck) -> Result<(), String> {
        conn.execute("INSERT INTO session_prompt_cache_checks (
            session_id, request_key, request_sequence, message_id, message, model_id, provider,
            baseline_tokens, input_tokens, cache_read_tokens, cache_write_tokens, effective_context_tokens,
            usage_complete, ends_context, excess_input_tokens, invalidated, reason, algorithm_version, created_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            params![session, check.request_key, check.request_sequence, check.message_id, check.message, check.model_id,
                check.provider, check.baseline_tokens as i64, check.input_tokens as i64, check.cache_read_tokens as i64,
                check.cache_write_tokens.map(|v| v as i64), check.effective_context_tokens as i64,
                check.usage_complete, check.ends_context, check.excess_input_tokens as i64, check.invalidated,
                check.reason, check.algorithm_version, check.created_at]).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(super) fn prompt_cache_request_exists(conn: &Connection, session: &str, key: &str) -> Result<bool, String> {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM session_prompt_cache_checks WHERE session_id=?1 AND request_key=?2)",
            params![session, key], |row| row.get(0)).map_err(|e| e.to_string())
    }

    pub(super) fn reset_prompt_cache_baseline(conn: &Connection, session: &str) -> Result<(), String> {
        conn.execute("UPDATE session_prompt_cache_checks SET ends_context=1
            WHERE id=(SELECT MAX(id) FROM session_prompt_cache_checks WHERE session_id=?1)", [session])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(super) fn record_request_cache_check(
        conn: &Connection, session: &str, model: &str, provider: &str,
        request: &PromptCacheRequest, input: u64, read: u64, write: u64, created_at: i64,
    ) -> Result<Option<PromptCacheCheckOutcome>, String> {
        if Self::prompt_cache_request_exists(conn, session, &request.key)? { return Ok(None); }
        let previous = conn.query_row("SELECT request_key, request_sequence, message_id, message, model_id,
            provider, baseline_tokens, input_tokens, cache_read_tokens, cache_write_tokens,
            effective_context_tokens, usage_complete, ends_context, excess_input_tokens,
            invalidated, reason, algorithm_version, created_at
            FROM session_prompt_cache_checks WHERE session_id=?1 ORDER BY id DESC LIMIT 1",
            [session], Self::prompt_cache_check_from_row).optional().map_err(|e| e.to_string())?;
        let message = if let Some(id) = request.message_id.as_deref() {
            conn.query_row("SELECT id, content FROM messages WHERE session_id=?1 AND id=?2",
                params![session, id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        } else {
            conn.query_row("SELECT id, content FROM messages WHERE session_id=?1 AND role='user'
                AND tool_call_id IS NULL AND include_in_prompt=1 ORDER BY rowid DESC LIMIT 1", [session],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        }.optional().map_err(|e| e.to_string())?.unwrap_or_default();
        let model = canonical_model(model, provider);
        let outcome = evaluate(previous.as_ref(), &model, provider, input, read, write, request.usage_complete);
        let check = StoredPromptCacheCheck {
            request_key: request.key.clone(), request_sequence: previous.as_ref().map_or(1, |p| p.request_sequence + 1),
            message_id: message.0, message: message.1, model_id: model, provider: provider.into(),
            baseline_tokens: outcome.baseline_tokens, input_tokens: input, cache_read_tokens: read,
            cache_write_tokens: request.usage_complete.then_some(write),
            effective_context_tokens: input.saturating_add(read).saturating_add(write),
            usage_complete: request.usage_complete, ends_context: false, excess_input_tokens: outcome.excess_input_tokens,
            invalidated: outcome.invalidated, reason: outcome.reason.clone(), algorithm_version: ALGORITHM_VERSION, created_at,
        };
        Self::insert_prompt_cache_check(conn, session, &check)?;
        Ok(Some(outcome))
    }

    pub(super) fn record_claude_cache_check(
        conn: &Connection, session: &str, key: String, message_id: String,
        model: &str, usage: &serde_json::Value, created_at: i64,
    ) -> Result<(), String> {
        let count = |name: &str| usage[name].as_u64().unwrap_or(0).min(u32::MAX as u64);
        let mut request = PromptCacheRequest::new(key, Some(message_id));
        request.usage_complete = claude_prompt_usage_complete(usage) && !model.is_empty() && model != "unknown";
        Self::record_request_cache_check(conn, session, model, "Claude Code CLI", &request,
            count("input_tokens"), count("cache_read_input_tokens"), count("cache_creation_input_tokens"), created_at)?;
        Ok(())
    }

    pub fn list_cache_invalidations(&self, session_id: &str) -> Result<Vec<SessionCacheInvalidation>, String> {
        let conn = self.read_conn()?;
        // The same row decoder is used for forks and reports so legacy NULL
        // cache-write data remains unknown rather than being presented as zero.
        Ok(Self::prompt_cache_checks_with_conn(&conn, session_id)?.into_iter().rev()
            .filter(|row| row.invalidated).map(|row| SessionCacheInvalidation {
                event_id: row.request_key, request_sequence: row.request_sequence as u64,
                message_id: row.message_id, message: row.message, model_id: row.model_id,
                baseline_tokens: row.baseline_tokens, input_tokens: row.input_tokens,
                cache_read_tokens: row.cache_read_tokens, cache_write_tokens: row.cache_write_tokens,
                excess_input_tokens: row.excess_input_tokens, reason: row.reason,
                algorithm_version: row.algorithm_version as u32, occurred_at: row.created_at,
            }).collect())
    }

    pub fn cache_detection_summary(&self, session: &str) -> Result<SessionCacheDetection, String> {
        let conn = self.read_conn()?;
        conn.query_row("SELECT
            COALESCE(SUM(algorithm_version=2 AND reason IN ('below_threshold','uncached_prefix_exceeds_threshold','model_changed','provider_changed')),0),
            COALESCE(SUM(algorithm_version=2 AND reason IN ('usage_unavailable','baseline_unavailable')),0),
            COALESCE(SUM(algorithm_version=2 AND reason IN ('no_baseline','context_reset')),0),
            COALESCE(SUM(algorithm_version<2),0)
            FROM session_prompt_cache_checks WHERE session_id=?1", [session], |r| Ok(SessionCacheDetection {
                checked_requests: r.get::<_, i64>(0)? as u64, unavailable_requests: r.get::<_, i64>(1)? as u64,
                baseline_requests: r.get::<_, i64>(2)? as u64, legacy_checks: r.get::<_, i64>(3)? as u64,
            })).map_err(|e| e.to_string())
    }
}
