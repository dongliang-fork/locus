const STORAGE_PREFIX = "locus:session-worktree:";

export interface SessionWorktreePreference {
  checkoutId: string;
  materializationEpoch: number | null;
}

/**
 * The worktree last chosen for a project's sessions. New sessions start there;
 * null means the project's source checkout.
 */
export function lastSessionWorktree(projectId: string): SessionWorktreePreference | null {
  if (typeof window === "undefined" || !projectId) return null;
  try {
    const parsed: unknown = JSON.parse(window.localStorage.getItem(STORAGE_PREFIX + projectId) ?? "null");
    if (!parsed || typeof parsed !== "object") return null;
    const { checkoutId, materializationEpoch } = parsed as Record<string, unknown>;
    if (typeof checkoutId !== "string" || !checkoutId) return null;
    return {
      checkoutId,
      materializationEpoch: typeof materializationEpoch === "number" ? materializationEpoch : null,
    };
  } catch {
    return null;
  }
}

export function rememberSessionWorktree(projectId: string, preference: SessionWorktreePreference | null): void {
  if (typeof window === "undefined" || !projectId) return;
  try {
    if (preference) window.localStorage.setItem(STORAGE_PREFIX + projectId, JSON.stringify(preference));
    else window.localStorage.removeItem(STORAGE_PREFIX + projectId);
  } catch {
    // The preference is a convenience; sessions still start in the source checkout.
  }
}
