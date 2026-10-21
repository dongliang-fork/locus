//! Process-level background work that no chat session owns, such as
//! preparing a worktree. Live entries are listed by every surface and each
//! change is broadcast, so the background task indicator in any window shows
//! the work after the surface that started it has closed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

pub(crate) const APP_TASK_UPDATED_EVENT: &str = "app-task-updated";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppTaskProgress {
    /// Kind-specific step name; the frontend localizes it per task kind.
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AppTaskStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppTaskInfo {
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub detail: Option<String>,
    pub status: AppTaskStatus,
    pub progress: Option<AppTaskProgress>,
    pub error: Option<String>,
    pub started_at: i64,
}

type Emit = Box<dyn Fn(&AppTaskInfo) + Send + Sync>;

pub(crate) struct AppTaskRegistry {
    tasks: Mutex<BTreeMap<String, AppTaskInfo>>,
    emit: Emit,
}

impl Default for AppTaskRegistry {
    fn default() -> Self {
        Self::new(|_| {})
    }
}

impl AppTaskRegistry {
    pub(crate) fn new(emit: impl Fn(&AppTaskInfo) + Send + Sync + 'static) -> Self {
        Self {
            tasks: Mutex::default(),
            emit: Box::new(emit),
        }
    }

    /// Live tasks in start order. Terminal tasks are only broadcast.
    pub(crate) fn list(&self) -> Vec<AppTaskInfo> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let mut result: Vec<_> = tasks.values().cloned().collect();
        result.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.task_id.cmp(&b.task_id)));
        result
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        kind: &str,
        title: impl Into<String>,
        detail: Option<String>,
    ) -> AppTaskGuard {
        let info = AppTaskInfo {
            task_id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            title: title.into(),
            detail,
            status: AppTaskStatus::Running,
            progress: None,
            error: None,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
        };
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(info.task_id.clone(), info.clone());
        (self.emit)(&info);
        AppTaskGuard {
            reporter: AppTaskReporter {
                registry: Arc::clone(self),
                task_id: info.task_id,
            },
            finished: false,
        }
    }

    fn update(&self, task_id: &str, progress: AppTaskProgress) {
        let info = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            let Some(task) = tasks.get_mut(task_id) else {
                return;
            };
            if task.progress.as_ref() == Some(&progress) {
                return;
            }
            task.progress = Some(progress);
            task.clone()
        };
        (self.emit)(&info);
    }

    fn finish(&self, task_id: &str, result: Result<(), String>) {
        let Some(mut info) = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(task_id)
        else {
            return;
        };
        match result {
            Ok(()) => info.status = AppTaskStatus::Completed,
            Err(error) => {
                info.status = AppTaskStatus::Failed;
                info.error = Some(error);
            }
        }
        (self.emit)(&info);
    }
}

/// Cheap clonable progress sink for one live task.
#[derive(Clone)]
pub(crate) struct AppTaskReporter {
    registry: Arc<AppTaskRegistry>,
    task_id: String,
}

impl AppTaskReporter {
    pub(crate) fn progress(&self, progress: AppTaskProgress) {
        self.registry.update(&self.task_id, progress);
    }
}

/// Owns the task's lifetime. Dropping it without [`finish`](Self::finish),
/// including on panic or a cancelled future, reports the task as failed so a
/// live entry can never be stranded.
pub(crate) struct AppTaskGuard {
    reporter: AppTaskReporter,
    finished: bool,
}

impl AppTaskGuard {
    pub(crate) fn reporter(&self) -> AppTaskReporter {
        self.reporter.clone()
    }

    pub(crate) fn finish(mut self, result: Result<(), String>) {
        self.finished = true;
        self.reporter.registry.finish(&self.reporter.task_id, result);
    }
}

impl Drop for AppTaskGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.reporter
                .registry
                .finish(&self.reporter.task_id, Err("Task was interrupted".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording() -> (Arc<AppTaskRegistry>, Arc<Mutex<Vec<AppTaskInfo>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let registry = Arc::new(AppTaskRegistry::new(move |info| {
            sink.lock().unwrap().push(info.clone())
        }));
        (registry, events)
    }

    fn progress(completed: u64) -> AppTaskProgress {
        AppTaskProgress {
            phase: "creating".into(),
            completed,
            total: Some(10),
        }
    }

    #[test]
    fn lists_live_tasks_and_broadcasts_only_changed_progress() {
        let (registry, events) = recording();
        let task = registry.begin("worktree", "feat/a", Some("F:/repo".into()));
        let reporter = task.reporter();
        reporter.progress(progress(3));
        reporter.progress(progress(3));
        reporter.progress(progress(4));
        let live = registry.list();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].progress, Some(progress(4)));
        assert_eq!(events.lock().unwrap().len(), 3); // begin + two distinct updates
        task.finish(Ok(()));
        assert!(registry.list().is_empty());
        let last = events.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.status, AppTaskStatus::Completed);
        // A late update after completion is ignored.
        reporter.progress(progress(9));
        assert_eq!(events.lock().unwrap().len(), 4);
    }

    #[test]
    fn failure_and_dropped_guard_are_reported_as_failed() {
        let (registry, events) = recording();
        registry.begin("worktree", "a", None).finish(Err("boom".into()));
        drop(registry.begin("worktree", "b", None));
        assert!(registry.list().is_empty());
        let events = events.lock().unwrap();
        let failed: Vec<_> = events
            .iter()
            .filter(|info| info.status == AppTaskStatus::Failed)
            .map(|info| (info.title.as_str(), info.error.as_deref()))
            .collect();
        assert_eq!(failed, [("a", Some("boom")), ("b", Some("Task was interrupted"))]);
    }
}
