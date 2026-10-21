//! Background session-database maintenance with frontend progress: purging
//! captures parked by the v53 migration, then reclaiming file space. Runs off
//! the startup path so the window appears immediately.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::session::store::SessionStore;
use crate::sqlite_maint::{VacuumOutcome, VacuumProgress, VacuumSpaceShortfall};

pub const SESSION_MAINTENANCE_EVENT: &str = "session-maintenance";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SessionMaintenanceStatus {
    PurgingCaptures { done_rows: u64, total_rows: u64 },
    Compacting { written_bytes: u64, total_bytes: u64 },
    Checkpointing,
    Finished { purged_rows: u64, reclaimed_bytes: u64 },
    InsufficientSpace(VacuumSpaceShortfall),
    Failed { message: String },
}

#[derive(Default)]
pub struct SessionMaintenanceState {
    running: AtomicBool,
    status: Mutex<Option<SessionMaintenanceStatus>>,
}

#[tauri::command]
pub fn get_session_maintenance_status(
    state: State<'_, Arc<SessionMaintenanceState>>,
) -> Option<SessionMaintenanceStatus> {
    state.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Starts maintenance unless it is already running; a running pass checks
/// fragmentation at its end anyway. `report_space_shortfall` is false for
/// passes triggered by session deletion so a full disk is not reported after
/// every delete.
pub fn spawn(app: AppHandle, store: Arc<SessionStore>, report_space_shortfall: bool) {
    let state = match app.try_state::<Arc<SessionMaintenanceState>>() {
        Some(state) => state.inner().clone(),
        None => return,
    };
    if state.running.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        run(&app, &state, &store, report_space_shortfall);
        state.running.store(false, Ordering::Release);
    });
}

fn run(
    app: &AppHandle,
    state: &SessionMaintenanceState,
    store: &SessionStore,
    report_space_shortfall: bool,
) {
    let report = |status: Option<SessionMaintenanceStatus>| {
        *state.status.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
        let _ = app.emit(SESSION_MAINTENANCE_EVENT, status);
    };

    let mut last_percent = None;
    let purged = store.purge_legacy_context_attempts(&mut |done_rows, total_rows| {
        let percent = done_rows.saturating_mul(100) / total_rows.max(1);
        if last_percent != Some(percent) {
            last_percent = Some(percent);
            report(Some(SessionMaintenanceStatus::PurgingCaptures { done_rows, total_rows }));
        }
    });
    let purged_rows = match purged {
        Ok(rows) => rows,
        Err(message) => {
            eprintln!("[Locus] session capture purge failed: {message}");
            report(Some(SessionMaintenanceStatus::Failed { message }));
            return;
        }
    };

    let last_written = Mutex::new(None);
    let outcome = store.vacuum_if_fragmented_with_progress(&|progress| match progress {
        VacuumProgress::Rewriting { written_bytes, total_bytes } => {
            // Report whole percents only; the monitor polls every 400 ms.
            let percent = written_bytes.saturating_mul(100) / total_bytes.max(1);
            let mut last = last_written.lock().unwrap_or_else(|e| e.into_inner());
            if *last != Some(percent) {
                *last = Some(percent);
                report(Some(SessionMaintenanceStatus::Compacting { written_bytes, total_bytes }));
            }
        }
        VacuumProgress::Checkpointing => report(Some(SessionMaintenanceStatus::Checkpointing)),
    });
    match outcome {
        Ok(VacuumOutcome::Reclaimed(reclaimed_bytes)) => {
            eprintln!(
                "[Locus] session db vacuum reclaimed {} MB",
                reclaimed_bytes / (1024 * 1024)
            );
            report(Some(SessionMaintenanceStatus::Finished {
                purged_rows: purged_rows.unwrap_or(0),
                reclaimed_bytes,
            }));
        }
        Ok(VacuumOutcome::NotNeeded) => match purged_rows {
            Some(purged_rows) => report(Some(SessionMaintenanceStatus::Finished {
                purged_rows,
                reclaimed_bytes: 0,
            })),
            None => report(None),
        },
        Ok(VacuumOutcome::InsufficientSpace(shortfall)) => {
            eprintln!("[Locus] session db vacuum skipped: {shortfall}");
            if report_space_shortfall {
                report(Some(SessionMaintenanceStatus::InsufficientSpace(shortfall)));
            } else {
                report(None);
            }
        }
        Err(message) => {
            eprintln!("[Locus] session db vacuum failed: {message}");
            report(Some(SessionMaintenanceStatus::Failed { message }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_to_the_frontend_contract() {
        assert_eq!(
            serde_json::to_value(SessionMaintenanceStatus::PurgingCaptures {
                done_rows: 1,
                total_rows: 2
            })
            .unwrap(),
            serde_json::json!({"phase": "purgingCaptures", "doneRows": 1, "totalRows": 2})
        );
        let shortfall = SessionMaintenanceStatus::InsufficientSpace(VacuumSpaceShortfall {
            reclaimable_bytes: 5,
            database_required_bytes: 4,
            database_available_bytes: 3,
            temp_required_bytes: 2,
            temp_available_bytes: 1,
        });
        assert_eq!(
            serde_json::to_value(shortfall).unwrap(),
            serde_json::json!({
                "phase": "insufficientSpace",
                "reclaimableBytes": 5,
                "databaseRequiredBytes": 4,
                "databaseAvailableBytes": 3,
                "tempRequiredBytes": 2,
                "tempAvailableBytes": 1
            })
        );
        assert_eq!(
            serde_json::to_value(SessionMaintenanceStatus::Checkpointing).unwrap(),
            serde_json::json!({"phase": "checkpointing"})
        );
    }
}
