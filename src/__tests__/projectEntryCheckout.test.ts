import { describe, expect, it } from "vitest";
import {
  isManagedWorktreeCheckout,
  projectEntryCheckout,
  type ProjectContextDescriptor,
  type WorkspaceCheckoutDescriptor,
} from "../services/project";

function checkout(
  checkoutId: string,
  options: { managedWorktree?: boolean; available?: boolean; runtimeWorktree?: boolean } = {},
): WorkspaceCheckoutDescriptor {
  return {
    checkoutId,
    projectId: "project",
    root: `F:/${checkoutId}`,
    normalizedRoot: `f:/${checkoutId}`,
    lastOpenedAt: 1,
    available: options.available,
    managedWorktree: options.managedWorktree,
    runtime: options.runtimeWorktree === undefined ? null : {
      projectId: "project",
      checkoutId,
      root: `F:/${checkoutId}`,
      workspaceGeneration: 1,
      leaseCount: 0,
      detectedServices: [],
      managedWorktree: options.runtimeWorktree,
    },
  };
}

function project(checkouts: WorkspaceCheckoutDescriptor[]): ProjectContextDescriptor {
  return { projectId: "project", detectedServices: [], checkouts };
}

describe("project entry checkout", () => {
  it("presents a managed worktree through its source checkout", () => {
    const source = checkout("source");
    const worktree = checkout("source.worktrees/slot", { managedWorktree: true });
    // Checkouts arrive most-recently-opened first, so the worktree can lead.
    expect(projectEntryCheckout(project([worktree, source]), worktree)).toBe(source);
  });

  it("keeps a source checkout as its own entry", () => {
    const source = checkout("source");
    const other = checkout("other");
    expect(projectEntryCheckout(project([other, source]), source)).toBe(source);
  });

  it("recognises worktrees reported only by the runtime descriptor", () => {
    const worktree = checkout("slot", { runtimeWorktree: true });
    expect(isManagedWorktreeCheckout(worktree)).toBe(true);
    const source = checkout("source");
    expect(projectEntryCheckout(project([worktree, source]), worktree)).toBe(source);
  });

  it("prefers an available source checkout and falls back to the worktree", () => {
    const missing = checkout("missing", { available: false });
    const source = checkout("source");
    const worktree = checkout("slot", { managedWorktree: true });
    expect(projectEntryCheckout(project([worktree, missing, source]), worktree)).toBe(source);
    expect(projectEntryCheckout(project([worktree]), worktree)).toBe(worktree);
    expect(projectEntryCheckout(undefined, worktree)).toBe(worktree);
  });
});
