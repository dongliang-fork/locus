use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tokio::sync::Notify;

#[derive(Debug, Clone)]
struct WaitingJob {
    ticket: u64,
    scope_key: String,
}

#[derive(Default)]
struct SchedulerState {
    active_jobs: usize,
    active_scopes: HashSet<String>,
    poisoned_scopes: HashSet<String>,
    waiting: VecDeque<WaitingJob>,
}

pub struct CompileScheduler {
    state: Mutex<SchedulerState>,
    notify: Notify,
    next_ticket: AtomicU64,
}

static SCHEDULER: OnceLock<Arc<CompileScheduler>> = OnceLock::new();

pub fn initialize() {
    let _ = SCHEDULER.set(Arc::new(CompileScheduler {
        state: Mutex::new(SchedulerState::default()),
        notify: Notify::new(),
        next_ticket: AtomicU64::new(0),
    }));
}

pub async fn acquire(scope_key: String) -> Result<CompileJobPermit, String> {
    acquire_inner(scope_key, false).await
}

/// Lifecycle control requests may recover a poisoned scope. They still
/// serialize behind its active job and earlier requests.
pub async fn acquire_control(scope_key: String) -> Result<CompileJobPermit, String> {
    acquire_inner(scope_key, true).await
}

async fn acquire_inner(
    scope_key: String,
    allow_poisoned: bool,
) -> Result<CompileJobPermit, String> {
    let scheduler = SCHEDULER
        .get()
        .cloned()
        .ok_or_else(|| "compile scheduler is not initialized".to_string())?;
    acquire_with_scheduler(scheduler, scope_key, allow_poisoned).await
}

async fn acquire_with_scheduler(
    scheduler: Arc<CompileScheduler>,
    scope_key: String,
    allow_poisoned: bool,
) -> Result<CompileJobPermit, String> {
    let ticket = scheduler.next_ticket.fetch_add(1, Ordering::Relaxed) + 1;
    {
        let mut state = scheduler
            .state
            .lock()
            .map_err(|error| format!("compile scheduler lock poisoned: {error}"))?;
        if !allow_poisoned && state.poisoned_scopes.contains(&scope_key) {
            return Err("compile scope is recovering from a timed-out request".to_string());
        }
        state.waiting.push_back(WaitingJob {
            ticket,
            scope_key: scope_key.clone(),
        });
    }
    let mut registration = WaitingRegistration {
        scheduler: Arc::clone(&scheduler),
        ticket,
        armed: true,
    };

    loop {
        // Register the waiter before inspecting state. `Notify::notify_waiters`
        // does not retain a permit, so enabling first closes the lost-wakeup
        // window between the admission check and `.await`.
        let notified = scheduler.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let admitted = {
            let mut state = scheduler
                .state
                .lock()
                .map_err(|error| format!("compile scheduler lock poisoned: {error}"))?;
            if !allow_poisoned && state.poisoned_scopes.contains(&scope_key) {
                return Err("compile scope is recovering from a timed-out request".to_string());
            }
            let position = state
                .waiting
                .iter()
                .position(|waiting| waiting.ticket == ticket)
                .ok_or_else(|| "compile queue registration disappeared".to_string())?;
            let earlier_same_scope = state
                .waiting
                .iter()
                .take(position)
                .any(|waiting| waiting.scope_key == scope_key);
            if !state.active_scopes.contains(&scope_key) && !earlier_same_scope {
                state.waiting.remove(position);
                state.active_jobs += 1;
                state.active_scopes.insert(scope_key.clone());
                true
            } else {
                false
            }
        };
        if admitted {
            registration.armed = false;
            return Ok(CompileJobPermit {
                scheduler: Arc::clone(&scheduler),
                scope_key,
            });
        }
        notified.await;
    }
}

pub fn mark_scope_poisoned(scope_key: &str) {
    let Some(scheduler) = SCHEDULER.get() else {
        return;
    };
    if let Ok(mut state) = scheduler.state.lock() {
        state.poisoned_scopes.insert(scope_key.to_string());
    }
    scheduler.notify.notify_waiters();
}

pub fn clear_scope_poisoned(scope_key: &str) {
    let Some(scheduler) = SCHEDULER.get() else {
        return;
    };
    if let Ok(mut state) = scheduler.state.lock() {
        state.poisoned_scopes.remove(scope_key);
    }
    scheduler.notify.notify_waiters();
}

pub fn clear_all_poisoned() {
    let Some(scheduler) = SCHEDULER.get() else {
        return;
    };
    if let Ok(mut state) = scheduler.state.lock() {
        state.poisoned_scopes.clear();
    }
    scheduler.notify.notify_waiters();
}

struct WaitingRegistration {
    scheduler: Arc<CompileScheduler>,
    ticket: u64,
    armed: bool,
}

impl Drop for WaitingRegistration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.scheduler.state.lock() {
            state
                .waiting
                .retain(|waiting| waiting.ticket != self.ticket);
        }
        self.scheduler.notify.notify_waiters();
    }
}

pub struct CompileJobPermit {
    scheduler: Arc<CompileScheduler>,
    scope_key: String,
}

impl Drop for CompileJobPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.scheduler.state.lock() {
            state.active_jobs = state.active_jobs.saturating_sub(1);
            state.active_scopes.remove(&self.scope_key);
        }
        self.scheduler.notify.notify_waiters();
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileSchedulerMetrics {
    pub active_jobs: usize,
    pub waiting_jobs: usize,
    pub active_scopes: usize,
    pub poisoned_scopes: usize,
}

pub fn metrics() -> CompileSchedulerMetrics {
    let Some(scheduler) = SCHEDULER.get() else {
        return CompileSchedulerMetrics {
            active_jobs: 0,
            waiting_jobs: 0,
            active_scopes: 0,
            poisoned_scopes: 0,
        };
    };
    let state = scheduler.state.lock().ok();
    CompileSchedulerMetrics {
        active_jobs: state.as_ref().map(|state| state.active_jobs).unwrap_or(0),
        waiting_jobs: state.as_ref().map(|state| state.waiting.len()).unwrap_or(0),
        active_scopes: state
            .as_ref()
            .map(|state| state.active_scopes.len())
            .unwrap_or(0),
        poisoned_scopes: state
            .as_ref()
            .map(|state| state.poisoned_scopes.len())
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scheduler() -> Arc<CompileScheduler> {
        Arc::new(CompileScheduler {
            state: Mutex::new(SchedulerState::default()),
            notify: Notify::new(),
            next_ticket: AtomicU64::new(0),
        })
    }

    async fn wait_for_waiting_jobs(scheduler: &CompileScheduler, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if scheduler
                    .state
                    .lock()
                    .map(|state| state.waiting.len() == expected)
                    .unwrap_or(false)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiting job count");
    }

    #[test]
    fn waiting_registration_drop_releases_queue_slot() {
        let scheduler = Arc::new(CompileScheduler {
            state: Mutex::new(SchedulerState {
                active_jobs: 0,
                active_scopes: HashSet::new(),
                poisoned_scopes: HashSet::new(),
                waiting: VecDeque::from([WaitingJob {
                    ticket: 7,
                    scope_key: "scope".to_string(),
                }]),
            }),
            notify: Notify::new(),
            next_ticket: AtomicU64::new(7),
        });
        drop(WaitingRegistration {
            scheduler: Arc::clone(&scheduler),
            ticket: 7,
            armed: true,
        });
        assert!(scheduler.state.lock().unwrap().waiting.is_empty());
    }

    #[tokio::test]
    async fn different_scopes_run_in_parallel_and_same_scope_stays_ordered() {
        let scheduler = test_scheduler();
        let left = acquire_with_scheduler(Arc::clone(&scheduler), "left".to_string(), false)
            .await
            .expect("left permit");
        let right =
            acquire_with_scheduler(Arc::clone(&scheduler), "right".to_string(), false)
                .await
                .expect("right permit");

        let scheduler_for_waiter = Arc::clone(&scheduler);
        let same_scope = tokio::spawn(async move {
            acquire_with_scheduler(scheduler_for_waiter, "left".to_string(), false).await
        });
        wait_for_waiting_jobs(&scheduler, 1).await;
        assert_eq!(scheduler.state.lock().unwrap().active_jobs, 2);
        drop(left);
        let same_scope = tokio::time::timeout(std::time::Duration::from_secs(1), same_scope)
            .await
            .expect("same scope wakes")
            .expect("same scope task")
            .expect("same scope permit");
        drop(same_scope);
        drop(right);
    }

    #[tokio::test]
    async fn independent_scopes_are_not_limited_by_a_global_job_budget() {
        let scheduler = test_scheduler();
        let mut permits = Vec::new();
        for index in 0..16 {
            permits.push(tokio::time::timeout(std::time::Duration::from_secs(1),
                acquire_with_scheduler(Arc::clone(&scheduler), format!("scope-{index}"), false))
                .await.expect("independent scope starts immediately").unwrap());
        }
        assert_eq!(scheduler.state.lock().unwrap().active_jobs, 16);
        drop(permits);
        assert_eq!(scheduler.state.lock().unwrap().active_jobs, 0);
    }

    #[tokio::test]
    async fn same_scope_queue_has_no_capacity_rejection_and_cancellation_cleans_up() {
        let scheduler = test_scheduler();
        let active = acquire_with_scheduler(Arc::clone(&scheduler), "scope".into(), false).await.unwrap();
        let mut waiters = Vec::new();
        for _ in 0..80 {
            let scheduler = Arc::clone(&scheduler);
            waiters.push(tokio::spawn(async move {
                acquire_with_scheduler(scheduler, "scope".into(), false).await
            }));
        }
        wait_for_waiting_jobs(&scheduler, 80).await;
        for waiter in waiters { waiter.abort(); let _ = waiter.await; }
        wait_for_waiting_jobs(&scheduler, 0).await;
        drop(active);
        assert_eq!(scheduler.state.lock().unwrap().active_jobs, 0);
    }

    #[tokio::test]
    async fn lifecycle_recovery_keeps_poisoned_scope_protection() {
        let scheduler = test_scheduler();
        scheduler.state.lock().unwrap().poisoned_scopes.insert("scope".into());
        assert!(acquire_with_scheduler(Arc::clone(&scheduler), "scope".into(), false).await.is_err());
        let control = acquire_with_scheduler(Arc::clone(&scheduler), "scope".into(), true).await.unwrap();
        assert_eq!(scheduler.state.lock().unwrap().active_jobs, 1);
        drop(control);
    }
}
