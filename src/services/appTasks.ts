import { ipcInvoke } from "./ipc";
import { getLocusRuntime, type RuntimeUnsubscribe } from "./locusRuntime";

/** Process-level work that no chat session owns, such as creating a worktree. */
export type AppTaskStatus = "running" | "completed" | "failed";

export interface AppTaskProgress {
  /** Kind-specific step; worktree tasks use `worktrees.progress.<phase>`. */
  phase: string;
  completed: number;
  total: number | null;
}

export interface AppTaskInfo {
  taskId: string;
  kind: string;
  title: string;
  detail: string | null;
  status: AppTaskStatus;
  progress: AppTaskProgress | null;
  error: string | null;
  startedAt: number;
}

export const APP_TASK_UPDATED_EVENT = "app-task-updated";

/** Live tasks only; completed and failed tasks are delivered as updates. */
export function listAppTasks(): Promise<AppTaskInfo[]> {
  return ipcInvoke("list_app_tasks");
}

export function subscribeAppTaskUpdates(handler: (task: AppTaskInfo) => void): Promise<RuntimeUnsubscribe> {
  return getLocusRuntime().subscribe<AppTaskInfo>(APP_TASK_UPDATED_EVENT, handler);
}
