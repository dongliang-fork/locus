import { reactive } from "vue";
import { t } from "../../i18n";
import { normalizeAppError } from "../../services/errors";
import {
  selectWorktreeBranch,
  type ManagedWorktree,
  type SelectWorktreeRequest,
  type WorktreeCreationPlan,
  type WorktreePlanProgress,
} from "../../services/worktrees";

export interface WorktreeCreationJob {
  branch: string;
  startedAt: number;
  progress: WorktreePlanProgress;
  status: "running" | "failed";
  error: string;
}

// Creating a worktree can take minutes and outlives the selector popup that
// starts it. Jobs are keyed by the composer that adopts the result, so a
// reopened selector shows the same progress and that composer cannot start a
// second creation or send into the old workspace meanwhile.
const jobs = reactive(new Map<string, WorktreeCreationJob>());

export function worktreeCreationJob(ownerKey: string): WorktreeCreationJob | undefined {
  return jobs.get(ownerKey);
}

export function isWorktreeCreationRunning(ownerKey: string | null | undefined): boolean {
  return !!ownerKey && jobs.get(ownerKey)?.status === "running";
}

export function worktreeSelectionError(cause: unknown): string {
  const message = normalizeAppError(cause).message;
  return message.includes("WORKTREE_NEW_PROJECT_REQUIRED") ? t("worktrees.budget.retry") : message;
}

/**
 * Creates the reviewed worktree, then hands it to `adopt` (normally switching
 * the originating new-session composer). Resolves false when a creation for
 * the owner is already running or this one failed; failures stay on the job.
 */
export async function startWorktreeCreation(
  ownerKey: string,
  request: SelectWorktreeRequest,
  plan: WorktreeCreationPlan,
  allowNewProject: boolean,
  adopt: (item: ManagedWorktree) => Promise<void>,
): Promise<boolean> {
  if (isWorktreeCreationRunning(ownerKey)) return false;
  jobs.set(ownerKey, {
    branch: request.branch,
    startedAt: Date.now(),
    progress: { phase: "checking", files: 0, totalFiles: null, bytes: 0 },
    status: "running",
    error: "",
  });
  const job = jobs.get(ownerKey)!;
  const current = () => jobs.get(ownerKey) === job;
  try {
    const item = await selectWorktreeBranch({ ...request, allowNewProject, expectedStartOid: plan.startOid }, (value) => {
      if (current()) job.progress = value;
    });
    await adopt(item);
    if (current()) jobs.delete(ownerKey);
    return true;
  } catch (cause) {
    if (current()) {
      job.status = "failed";
      job.error = worktreeSelectionError(cause);
    }
    return false;
  }
}

/** Clears a failed job once the user moves on; running jobs are kept. */
export function dismissWorktreeCreation(ownerKey: string): void {
  if (jobs.get(ownerKey)?.status === "failed") jobs.delete(ownerKey);
}

export function resetWorktreeCreationJobsForTests(): void {
  jobs.clear();
}
