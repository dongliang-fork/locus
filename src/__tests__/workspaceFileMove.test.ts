import { describe, expect, it } from "vitest";
import { assetWorkspaceReferenceDragData } from "../components/asset/assetWorkspaceDrag";
import type { AssetExplorerNode } from "../composables/useAssetState";
import { workbenchReferenceInternalDragSource } from "../components/workbench/workbenchReferenceDrag";
import { resolveWorkspaceFileMove, workspaceMovedPath, workspaceRelativeEntryPath } from "../components/explorer/workspaceFileMove";

const workspaceRef = { checkoutId: "a", expectedGeneration: 3, expectedMaterializationEpoch: 2 };
const context = { projectId: "p", workspaceRef, workspaceRoot: "F:/Game" };
const target = { projectId: "p", workspaceRef, root: "F:/Game", directory: "Target" };
const data = assetWorkspaceReferenceDragData(context, { kind: "file", name: "Main.cs", path: "Assets/Main.cs", depth: 1 })!;

describe("workspace file moves", () => {
  it("moves filesystem drags while keeping ordinary reference drags copy-only", () => {
    expect(resolveWorkspaceFileMove(data, target)).toEqual({ sourcePaths: ["Assets/Main.cs"], targetDirectory: "Target", workspaceRef });
    expect(workbenchReferenceInternalDragSource(data).allowedOperations).toContain("move");
    expect(resolveWorkspaceFileMove({ ...data, fileSystemMove: undefined }, target)).toBeNull();
    expect(workbenchReferenceInternalDragSource({ ...data, fileSystemMove: undefined }).allowedOperations).toEqual(["copy"]);
  });
  it("supports a drop back at the root and treats an unchanged parent as a no-op", () => {
    expect(resolveWorkspaceFileMove(data, { ...target, directory: "." })?.targetDirectory).toBe(".");
    expect(resolveWorkspaceFileMove(data, { ...target, directory: "assets" })).toBeNull();
  });
  it("does not turn a cross-checkout or stale drag into a move", () => {
    expect(resolveWorkspaceFileMove(data, { ...target, workspaceRef: { ...workspaceRef, checkoutId: "b" } })).toBeNull();
    expect(resolveWorkspaceFileMove(data, { ...target, workspaceRef: { ...workspaceRef, expectedGeneration: 4 } })).toBeNull();
    expect(resolveWorkspaceFileMove(data, { ...target, workspaceRef: { ...workspaceRef, expectedMaterializationEpoch: 3 } })).toBeNull();
  });
  it("rejects root, traversal, self and descendant moves before offering a drop", () => {
    const folder = { ...data, entries: [{ kind: "file" as const, path: "Assets/Foo", isDir: true }] };
    expect(resolveWorkspaceFileMove(folder, { ...target, directory: "Assets/Foo/Bar" })).toBeNull();
    expect(resolveWorkspaceFileMove(folder, { ...target, directory: "Assets/Foo" })).toBeNull();
    expect(resolveWorkspaceFileMove(folder, { ...target, directory: "Assets/Foobar" })).not.toBeNull();
    expect(resolveWorkspaceFileMove({ ...folder, entries: [{ kind: "file", path: ".", isDir: true }] }, target)).toBeNull();
    expect(workspaceRelativeEntryPath("F:/Game", "../other")).toBeNull();
    expect(workspaceRelativeEntryPath("F:/Game", "F:/Elsewhere/a")).toBeNull();
    expect(workspaceRelativeEntryPath("F:/Game", "f:\\game\\Assets\\a.cs")).toBe("Assets/a.cs");
  });
  it("moves a multi-selection once per outermost entry outside the target", () => {
    const many = assetWorkspaceReferenceDragData(context, [
      { kind: "folder", name: "Foo", path: "Assets/Foo", depth: 1 } as AssetExplorerNode,
      { kind: "file", name: "a.cs", path: "Assets/Foo/a.cs", depth: 2 },
      { kind: "file", name: "b.cs", path: "Target/b.cs", depth: 1 },
      { kind: "file", name: "main.cs", path: "assets/main.cs", depth: 1 },
      { kind: "file", name: "Main.cs", path: "Assets/Main.cs", depth: 1 },
    ])!;
    expect(many.entries).toHaveLength(5);
    expect(workbenchReferenceInternalDragSource(many).preview.count).toBe(5);
    expect(resolveWorkspaceFileMove(many, target)?.sourcePaths).toEqual(["Assets/Foo", "assets/main.cs"]);
    expect(resolveWorkspaceFileMove({ ...many, entries: many.entries.slice(2, 3) }, target)).toBeNull();
    expect(resolveWorkspaceFileMove(many, { ...target, directory: "Assets/Foo/Bar" })).toBeNull();
  });
  it("rebases opened descendants of moved folders without matching neighboring names", () => {
    expect(workspaceMovedPath("F:/Game/Assets/Foo/a.cs", "F:/Game/Assets/Foo", "F:/Game/Target/Foo", true))
      .toBe("F:/Game/Target/Foo/a.cs");
    expect(workspaceMovedPath("F:/Game/Assets/Foobar/a.cs", "F:/Game/Assets/Foo", "F:/Game/Target/Foo", true)).toBeNull();
  });
});
