use std::{path::Path, sync::Arc};

use crate::config::AppConfig;
use crate::session::store::SessionStore;
use crate::workspace_service::pool::{self, AcquirePoolRequest, PoolAcquisition};
use crate::workspace_service::worktrees::{
    self, CreateWorktreeRequest, ManagedWorktree, WorktreeOperation,
};
use crate::workspace_service::{CheckoutId, ProjectRegistry};
use tauri::State;
use serde::{Deserialize, Serialize};
use crate::workspace_service::WorkspaceRef;
use crate::app_tasks::{AppTaskProgress, AppTaskRegistry};
use worktrees::estimate::{with_creation_progress, Progress, ProgressSink, WorktreePlanProgress};
use worktrees::location::{WorktreePolicy, WorktreeRootSource};
use tauri::ipc::{Channel, JavaScriptChannelId};

/// Resolve directories without coupling physical project slots to runtime
/// concurrency. Callers can still supply an explicit storage budget.
pub(crate) fn worktree_policy(config: &AppConfig, _registry: &ProjectRegistry) -> Result<WorktreePolicy, String> {
    WorktreePolicy::resolve(config.worktree_root(), None)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRootSettings {
    /// Persisted base directory; `None` places worktrees beside the repository.
    settings_root: Option<String>,
    /// `--locus-worktree-root` / `LOCUS_WORKTREE_ROOT` for this process; wins over the setting.
    process_root: Option<String>,
}

fn root_settings(config: &AppConfig) -> Result<WorktreeRootSettings, String> {
    Ok(WorktreeRootSettings {
        settings_root: config.worktree_root().map(|path| path.to_string_lossy().into_owned()),
        process_root: crate::runtime_paths::runtime_worktree_root_from_env()?
            .map(|path| path.to_string_lossy().into_owned()),
    })
}

#[tauri::command]
pub fn get_worktree_root_settings(config: State<'_, Arc<AppConfig>>) -> Result<WorktreeRootSettings, String> {
    root_settings(&config)
}

#[tauri::command]
pub fn set_worktree_root_setting(root: Option<String>, config: State<'_, Arc<AppConfig>>) -> Result<WorktreeRootSettings, String> {
    let root = non_empty(root)
        .map(|value| worktrees::location::normalize_root_setting(Path::new(&value)))
        .transpose()?;
    config.set_worktree_root(root.as_deref())?;
    root_settings(&config)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeDefaults {
    /// Container for new checkouts of this source.
    container: String,
    root_source: WorktreeRootSource,
    max_slots: Option<usize>,
}

#[tauri::command]
pub async fn get_worktree_defaults(
    source_root: String,
    registry: State<'_, Arc<ProjectRegistry>>,
    config: State<'_, Arc<AppConfig>>,
) -> Result<WorktreeDefaults, String> {
    registered_source(&registry, &source_root)?;
    let policy = worktree_policy(&config, &registry)?;
    tauri::async_runtime::spawn_blocking(move || {
        Ok(WorktreeDefaults {
            container: policy.container(Path::new(&source_root))?.to_string_lossy().into(),
            root_source: policy.root_source(),
            max_slots: policy.max_slots,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectWorktreeRequest {
    workspace_ref: WorkspaceRef,
    branch: String,
    create_branch: bool,
    include_dirty: bool,
    #[serde(default)]
    allow_new_project: bool,
    expected_start_oid: Option<String>,
    start_ref: Option<String>,
}

#[tauri::command]
pub async fn plan_worktree_selection(
    request: SelectWorktreeRequest,
    registry: State<'_, Arc<ProjectRegistry>>,
    config: State<'_, Arc<AppConfig>>,
    on_progress: Option<JavaScriptChannelId>,
    webview: tauri::Webview,
) -> Result<worktrees::usage::WorktreeCreationPlan, String> {
    let on_progress: Option<Channel<WorktreePlanProgress>> = on_progress.map(|id| id.channel_on(webview));
    let scope = registry.resolve_workspace_ref(&request.workspace_ref).map_err(|e| e.to_string())?;
    let policy = worktree_policy(&config, &registry)?;
    tauri::async_runtime::spawn_blocking(move || worktrees::usage::selection_plan_with_progress(
        scope.runtime().root(), &request.branch, request.create_branch, request.include_dirty, &policy, request.start_ref.as_deref(),
        &mut Progress::new(&|event| { if let Some(channel) = &on_progress { let _ = channel.send(event); } }),
    )).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_worktree_pool_usage(source_root: String, registry: State<'_, Arc<ProjectRegistry>>) -> Result<worktrees::usage::WorktreePoolUsage, String> {
    registered_source(&registry, &source_root)?;
    tauri::async_runtime::spawn_blocking(move || worktrees::usage::pool_usage(Path::new(&source_root))).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_all_worktree_pool_usage(store: State<'_, Arc<SessionStore>>) -> Result<Vec<worktrees::usage::WorktreeProjectUsage>, String> {
    let checkouts = store.list_visible_workspace_checkouts()?.into_iter()
        .map(|checkout| (checkout.project_id, checkout.root_path)).collect();
    tauri::async_runtime::spawn_blocking(move || worktrees::usage::project_pool_usages(checkouts)).await.map_err(|error| error.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanWorktreeCreationRequest {
    source_root: String,
    start_ref: String,
    include_dirty: bool,
    pool_mode: bool,
    /// Empty uses the configured container (or a reusable pool slot's directory).
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    max_slots: Option<usize>,
}

#[tauri::command]
pub async fn plan_worktree_creation(request: PlanWorktreeCreationRequest, registry: State<'_, Arc<ProjectRegistry>>, config: State<'_, Arc<AppConfig>>, on_progress: Option<JavaScriptChannelId>, webview: tauri::Webview) -> Result<worktrees::usage::WorktreeCreationPlan, String> {
    let on_progress: Option<Channel<WorktreePlanProgress>> = on_progress.map(|id| id.channel_on(webview));
    registered_source(&registry, &request.source_root)?;
    let mut policy = worktree_policy(&config, &registry)?;
    if let Some(max_slots) = request.max_slots {
        policy.max_slots = Some(max_slots);
    }
    let directory = non_empty(request.directory);
    tauri::async_runtime::spawn_blocking(move || worktrees::usage::creation_plan_with_progress(
        Path::new(&request.source_root), &request.start_ref, request.include_dirty, request.pool_mode,
        directory.as_deref().map(Path::new), &policy,
        &mut Progress::new(&|event| { if let Some(channel) = &on_progress { let _ = channel.send(event); } }),
    )).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn list_worktree_branches(
    workspace_ref: WorkspaceRef,
    registry: State<'_, Arc<ProjectRegistry>>,
) -> Result<Vec<worktrees::selector::WorktreeBranchOption>, String> {
    let scope = registry.resolve_workspace_ref(&workspace_ref).map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || worktrees::selector::branches(scope.runtime().root()))
        .await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn select_worktree_branch(
    request: SelectWorktreeRequest,
    registry: State<'_, Arc<ProjectRegistry>>,
    store: State<'_, Arc<SessionStore>>,
    config: State<'_, Arc<AppConfig>>,
    app_tasks: State<'_, Arc<AppTaskRegistry>>,
    on_progress: Option<JavaScriptChannelId>,
    webview: tauri::Webview,
) -> Result<ManagedWorktree, String> {
    let on_progress: Option<Channel<WorktreePlanProgress>> = on_progress.map(|id| id.channel_on(webview));
    let scope = registry.resolve_workspace_ref(&request.workspace_ref).map_err(|e| e.to_string())?;
    let policy = worktree_policy(&config, &registry)?;
    // Creation can outlive the selector that started it; a process-level task
    // keeps it visible in every window until it settles.
    let task = app_tasks.begin("worktree", request.branch.clone(), Some(scope.runtime().root().to_string_lossy().into()));
    let reporter = task.reporter();
    let sink: ProgressSink = Arc::new(move |event: WorktreePlanProgress| {
        reporter.progress(AppTaskProgress { phase: event.phase.into(), completed: event.files, total: event.total_files });
        if let Some(channel) = &on_progress { let _ = channel.send(event); }
    });
    let result = tauri::async_runtime::spawn_blocking(move || {
        let plan = worktrees::usage::selection_plan_with_progress(scope.runtime().root(), &request.branch, request.create_branch, request.include_dirty, &policy, request.start_ref.as_deref(), &mut Progress::new(&*sink))?;
        if request.expected_start_oid.as_ref().is_some_and(|oid| *oid != plan.start_oid) { return Err("Source branch changed; review the worktree creation again".into()); }
        if plan.at_capacity { return Err("Project pool is at capacity; manually recycle a project in settings".into()); }
        if request.allow_new_project && plan.budget.as_ref().is_some_and(|budget| budget.free_bytes.is_some_and(|free| free < budget.estimated_bytes)) {
            return Err("Insufficient disk space for the estimated project size".into());
        }
        sink(WorktreePlanProgress { phase: "creating", files: 0, total_files: None, bytes: 0 });
        with_creation_progress(sink, || worktrees::selector::select_from_ref(
            scope.runtime().root(), &request.branch, request.create_branch, request.include_dirty, &policy, request.allow_new_project,
            request.start_ref.as_deref(),
        ))
    }).await.map_err(|e| e.to_string()).and_then(|result| result)
        .and_then(|record| register_record(&registry, &store, &record).map(|()| record));
    task.finish(result.as_ref().map(|_| ()).map_err(Clone::clone));
    result
}

fn registered_source(registry: &ProjectRegistry, source: &str) -> Result<(), String> {
    registry
        .runtime_for_root(Path::new(source))
        .ok_or("Source workspace must be open in Locus")?;
    Ok(())
}

pub(crate) fn register_record(
    registry: &ProjectRegistry,
    store: &SessionStore,
    record: &ManagedWorktree,
) -> Result<(), String> {
    let runtime = registry.register(&record.root)?;
    if runtime.project_id().as_str() != record.project_id {
        return Err("Registered worktree project identity changed".into());
    }
    store.upsert_workspace_checkout(&crate::session::models::WorkspaceCheckoutRecord {
        checkout_id: runtime.checkout_id().to_string(),
        project_id: runtime.project_id().to_string(),
        root_path: record.root.clone(),
        normalized_root: runtime.normalized_root().into(),
        last_opened_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    })?;
    Ok(())
}

/// An empty destination creates `<container>/worktree-<id>`.
#[tauri::command]
pub async fn create_worktree(
    mut request: CreateWorktreeRequest,
    registry: State<'_, Arc<ProjectRegistry>>,
    store: State<'_, Arc<SessionStore>>,
    config: State<'_, Arc<AppConfig>>,
) -> Result<ManagedWorktree, String> {
    registered_source(&registry, &request.source_root)?;
    let policy = worktree_policy(&config, &registry)?;
    let record = tauri::async_runtime::spawn_blocking(move || {
        if request.destination.trim().is_empty() {
            request.destination = policy
                .new_checkout(Path::new(&request.source_root), "worktree")?
                .to_string_lossy()
                .into();
        }
        worktrees::create(&request)
    })
    .await
    .map_err(|e| e.to_string())??;
    register_record(&registry, &store, &record)?;
    Ok(record)
}

#[tauri::command]
pub async fn list_managed_worktrees(
    source_root: String,
    registry: State<'_, Arc<ProjectRegistry>>,
) -> Result<Vec<ManagedWorktree>, String> {
    registered_source(&registry, &source_root)?;
    tauri::async_runtime::spawn_blocking(move || worktrees::list(Path::new(&source_root)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn discover_worktrees(
    source_root: String,
    registry: State<'_, Arc<ProjectRegistry>>,
) -> Result<Vec<String>, String> {
    registered_source(&registry, &source_root)?;
    tauri::async_runtime::spawn_blocking(move || worktrees::discover(Path::new(&source_root)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn import_worktree(
    source_root: String,
    target_root: String,
    registry: State<'_, Arc<ProjectRegistry>>,
    store: State<'_, Arc<SessionStore>>,
) -> Result<ManagedWorktree, String> {
    registered_source(&registry, &source_root)?;
    let record = tauri::async_runtime::spawn_blocking(move || {
        worktrees::import(Path::new(&source_root), Path::new(&target_root))
    })
    .await
    .map_err(|e| e.to_string())??;
    register_record(&registry, &store, &record)?;
    Ok(record)
}

#[tauri::command]
pub async fn remove_managed_worktree(
    source_root: String,
    checkout_id: String,
    expected_epoch: u64,
    registry: State<'_, Arc<ProjectRegistry>>,
) -> Result<(), String> {
    registered_source(&registry, &source_root)?;
    let record = worktrees::list(Path::new(&source_root))?
        .into_iter()
        .find(|record| record.checkout_id == checkout_id)
        .ok_or("Checkout is outside the source repository")?;
    if !record.managed
        || record.materialization_epoch != expected_epoch
        || record.assignment_id.is_some()
    {
        return Err(
            "Only an unassigned managed checkout with the current epoch may be removed".into(),
        );
    }
    worktrees::ensure_recyclable(&record)?;
    registry
        .retire_managed_checkout(&CheckoutId::new(&checkout_id).map_err(|e| e.to_string())?)
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        worktrees::remove(Path::new(&source_root), &checkout_id, expected_epoch)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_worktree_operations(
    source_root: String,
    registry: State<'_, Arc<ProjectRegistry>>,
) -> Result<Vec<WorktreeOperation>, String> {
    registered_source(&registry, &source_root)?;
    tauri::async_runtime::spawn_blocking(move || worktrees::operations(Path::new(&source_root)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn bind_session_worktree(
    session_id: String,
    checkout_id: String,
    expected_epoch: u64,
    registry: State<'_, Arc<ProjectRegistry>>,
    store: State<'_, Arc<SessionStore>>,
) -> Result<(), String> {
    let id = CheckoutId::new(checkout_id).map_err(|e| e.to_string())?;
    let runtime = registry.activate_persisted_checkout(&id)?;
    if runtime.materialization_epoch() != expected_epoch {
        return Err("Stale materialization epoch".into());
    }
    store.bind_session_checkout(&session_id, id.as_str(), expected_epoch)
}

/// An empty pool root reuses a released compatible slot where it lives,
/// otherwise uses the configured container.
#[tauri::command]
pub async fn acquire_unity_project_slot(
    mut request: AcquirePoolRequest,
    registry: State<'_, Arc<ProjectRegistry>>,
    store: State<'_, Arc<SessionStore>>,
    config: State<'_, Arc<AppConfig>>,
    allow_new_project: Option<bool>,
) -> Result<PoolAcquisition, String> {
    registered_source(&registry, &request.source_root)?;
    let policy = worktree_policy(&config, &registry)?;
    let acquired = tauri::async_runtime::spawn_blocking(move || {
        if request.pool_root.trim().is_empty() {
            request.pool_root = policy
                .pool_root_for_commit(Path::new(&request.source_root), &request.commit)?
                .to_string_lossy()
                .into();
        }
        pool::acquire_with_creation_policy(&request, allow_new_project.unwrap_or(false))
    })
    .await
    .map_err(|e| e.to_string())??;
    register_record(&registry, &store, &acquired.worktree)?;
    Ok(acquired)
}

#[tauri::command]
pub async fn release_unity_project_slot(
    source_root: String,
    checkout_id: String,
    assignment_id: String,
    expected_epoch: u64,
    registry: State<'_, Arc<ProjectRegistry>>,
) -> Result<ManagedWorktree, String> {
    registered_source(&registry, &source_root)?;
    let record = worktrees::list(Path::new(&source_root))?
        .into_iter()
        .find(|record| record.checkout_id == checkout_id)
        .ok_or("Pool slot is outside the source repository")?;
    if !record.pool_slot
        || record.assignment_id.as_deref() != Some(&assignment_id)
        || record.materialization_epoch != expected_epoch
    {
        return Err("Pool assignment or materialization epoch is stale".into());
    }
    worktrees::ensure_recyclable(&record)?;
    registry
        .retire_managed_checkout(&CheckoutId::new(&checkout_id).map_err(|e| e.to_string())?)
        .await?;
    tauri::async_runtime::spawn_blocking(move || {
        pool::release(
            Path::new(&source_root),
            &checkout_id,
            &assignment_id,
            expected_epoch,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}
