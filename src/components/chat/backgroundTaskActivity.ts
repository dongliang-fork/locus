import { onScopeDispose, reactive } from "vue";
import type { AsyncTaskStatus, AsyncTaskUpdatedEvent } from "../../types";
import type { BackgroundTaskInfo } from "../../services/asyncTasks";
import { subscribeSessionAsyncTaskUpdates } from "../../services/sessionStreamEventHub";

export interface BackgroundTaskFailure {
  /** Session-local public id; matches `BackgroundTaskInfo.localId` and `AsyncTaskUpdatedEvent.taskId`. */
  taskId: string;
  toolName: string;
  description: string | null;
  detail: string;
  failedAt: number;
  /** Process-level task, which has no session-local id to show. */
  source?: "app";
}

type UpdateListener = (update: AsyncTaskUpdatedEvent) => void;

const MAX_FAILURE_SESSIONS = 64;
const MAX_FAILURES_PER_SESSION = 20;
const MAX_DESCRIPTIONS = 512;
const MAX_DETAIL_CHARS = 240;

// Chat surfaces remount on every session switch, so failures the user has not
// looked at yet live here rather than in a single indicator instance.
const unseenFailures = reactive(new Map<string, BackgroundTaskFailure[]>());
const descriptions = new Map<string, string>();
const listeners = new Set<UpdateListener>();
let unsubscribe: (() => void) | undefined;

export function isTerminalTaskStatus(status: AsyncTaskStatus): boolean {
  return status === "completed" || status === "failed" || status === "cancelled";
}

function descriptionKey(sessionId: string, taskId: string) {
  return `${sessionId}\u0000${taskId}`;
}

function failureDetail(output: string): string {
  const line = output
    .split(/\r?\n/)
    .map((value) => value.trim())
    .find((value) => value && !/^<\/?[\w-]+>$/.test(value)) ?? "";
  return line.length > MAX_DETAIL_CHARS ? `${line.slice(0, MAX_DETAIL_CHARS - 1)}…` : line;
}

function recordFailure(sessionId: string, update: AsyncTaskUpdatedEvent) {
  const failure: BackgroundTaskFailure = {
    taskId: update.taskId,
    toolName: update.toolName,
    description: descriptions.get(descriptionKey(sessionId, update.taskId)) ?? null,
    detail: failureDetail(update.output),
    failedAt: Date.now(),
  };
  const previous = unseenFailures.get(sessionId) ?? [];
  const next = [...previous.filter((item) => item.taskId !== failure.taskId), failure];
  unseenFailures.delete(sessionId);
  unseenFailures.set(sessionId, next.slice(-MAX_FAILURES_PER_SESSION));
  while (unseenFailures.size > MAX_FAILURE_SESSIONS) {
    unseenFailures.delete(unseenFailures.keys().next().value as string);
  }
}

function forgetFailure(sessionId: string, taskId: string) {
  const previous = unseenFailures.get(sessionId);
  if (!previous?.some((item) => item.taskId === taskId)) return;
  const next = previous.filter((item) => item.taskId !== taskId);
  if (next.length) unseenFailures.set(sessionId, next);
  else unseenFailures.delete(sessionId);
}

function handleUpdate(update: AsyncTaskUpdatedEvent) {
  const sessionId = update.sessionId.trim();
  if (!sessionId || !update.taskId) return;
  if (update.status === "failed") recordFailure(sessionId, update);
  // A failed task can be resumed under the same id; the old failure is then stale.
  else forgetFailure(sessionId, update.taskId);
  if (isTerminalTaskStatus(update.status)) descriptions.delete(descriptionKey(sessionId, update.taskId));
  for (const listener of [...listeners]) listener({ ...update, sessionId });
}

/**
 * Subscribes to background task lifecycle updates for the current scope. The
 * shared hub subscription stays alive while any indicator is mounted so
 * failures in other sessions are still recorded.
 */
export function useBackgroundTaskUpdates(listener: UpdateListener): void {
  listeners.add(listener);
  unsubscribe ??= subscribeSessionAsyncTaskUpdates(handleUpdate);
  onScopeDispose(() => {
    listeners.delete(listener);
    if (listeners.size) return;
    unsubscribe?.();
    unsubscribe = undefined;
  });
}

/** Keeps task descriptions so a later failure event, which has none, can still be labelled. */
export function rememberTaskDescriptions(sessionId: string, tasks: readonly BackgroundTaskInfo[]): void {
  for (const task of tasks) {
    if (task.sessionId !== sessionId || !task.description) continue;
    const key = descriptionKey(sessionId, task.localId);
    descriptions.delete(key);
    descriptions.set(key, task.description);
  }
  while (descriptions.size > MAX_DESCRIPTIONS) {
    descriptions.delete(descriptions.keys().next().value as string);
  }
}

export function unseenFailureCount(sessionId: string | null): number {
  return sessionId ? unseenFailures.get(sessionId)?.length ?? 0 : 0;
}

/** Returns and clears the failures the user has not seen in this session. */
export function takeUnseenFailures(sessionId: string): BackgroundTaskFailure[] {
  const failures = unseenFailures.get(sessionId) ?? [];
  unseenFailures.delete(sessionId);
  return failures;
}

export function resetBackgroundTaskActivityForTests(): void {
  unseenFailures.clear();
  descriptions.clear();
  listeners.clear();
  unsubscribe?.();
  unsubscribe = undefined;
}
