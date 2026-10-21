//! Statistics may lag a live conversation. Only BUSY/LOCKED failures with a
//! known uncommitted transaction are retried; a committed delta is never replayed.
use super::*;

#[derive(Clone)]
pub(crate) struct ModelUsageRecord {
    pub session_id: String,
    pub model_id: String,
    pub provider: String,
    pub request_kind: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model_active_duration_ms: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub priced_rounds: u64,
    pub context_tokens: Option<u32>,
    pub context_limit: Option<u32>,
    pub cache_request: Option<PromptCacheRequest>,
}

pub(super) struct UsageWriteError {
    pub message: String,
    pub retryable: bool,
}
impl From<String> for UsageWriteError {
    fn from(message: String) -> Self {
        Self {
            message,
            retryable: false,
        }
    }
}
impl UsageWriteError {
    pub(super) fn transaction(error: rusqlite::Error, operation: &str) -> Self {
        let retryable = matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        );
        Self {
            message: format!("Failed to {operation} model usage transaction: {error}"),
            retryable,
        }
    }
}

impl ModelUsageRecord {
    fn apply(
        &self,
        writer: &Arc<SessionWriter>,
        best_effort: bool,
    ) -> Result<(TokenUsage, Option<PromptCacheCheckOutcome>), UsageWriteError> {
        SessionStore::record_model_usage_on_conn(
            writer,
            &self.session_id,
            &self.model_id,
            &self.provider,
            &self.request_kind,
            self.input_tokens,
            self.output_tokens,
            self.model_active_duration_ms,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.cost_usd,
            self.priced_rounds,
            self.context_tokens,
            self.context_limit,
            self.cache_request.as_ref(),
            best_effort,
        )
    }
}

impl SessionStore {
    pub(crate) fn record_model_usage_request(&self, record: ModelUsageRecord)
        -> Result<(TokenUsage, Option<PromptCacheCheckOutcome>), String>
    {
        record.apply(&self.conn, false).map_err(|error| error.message)
    }

    /// The caller retains the raw receipt independently of these aggregates.
    /// Retry only a known-uncommitted BUSY/LOCKED result, within this callback:
    /// never replay an old delta after native history reconciliation.
    pub(crate) fn record_model_usage_best_effort(
        &self,
        mut record: ModelUsageRecord,
    ) -> Option<(TokenUsage, Option<PromptCacheCheckOutcome>)> {
        if record.cache_request.is_none() {
            record.cache_request = Some(PromptCacheRequest::new(Uuid::new_v4().to_string(), None));
        }
        for attempt in 0..3 {
            match record.apply(&self.conn, true) {
                Ok(usage) => return Some(usage),
                Err(error) if error.retryable && attempt < 2 => {
                    // apply has released the writer and rolled back the failed
                    // transaction before waiting, so other sessions can proceed.
                    thread::sleep(Duration::from_millis(25 * (attempt + 1)));
                }
                Err(error) => {
                    eprintln!("[SessionStore] usage statistics not saved for session {} (raw Claude usage retained in response metadata): {}", record.session_id, error.message);
                    return None;
                }
            }
        }
        unreachable!()
    }
}

/// Limit only best-effort statistics waits. Restore the previous connection
/// policy before the next session/critical write acquires the writer.
pub(super) struct UsageBusyTimeout<'a> {
    conn: &'a Connection,
    previous_ms: u32,
}
impl<'a> UsageBusyTimeout<'a> {
    pub(super) fn new(conn: &'a Connection) -> Result<Self, String> {
        let previous_ms: u32 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_millis(u64::from(previous_ms.min(100))))
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, previous_ms })
    }
}
impl Drop for UsageBusyTimeout<'_> {
    fn drop(&mut self) {
        let _ = self
            .conn
            .busy_timeout(Duration::from_millis(u64::from(self.previous_ms)));
    }
}
