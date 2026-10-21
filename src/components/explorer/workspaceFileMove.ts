import type { WorkspaceRef } from "../../services/project";
import type { WorkbenchReferenceDragData } from "../workbench/workbenchReferenceDrag";

export interface WorkspaceFileDropScope {
  projectId: string;
  workspaceRef: WorkspaceRef;
  root: string;
  directory: string;
}

export function workspaceRelativeEntryPath(root: string, value: string): string | null {
  let path = value.replace(/\\/g, "/").replace(/\/+$/, "");
  const normalizedRoot = root.replace(/\\/g, "/").replace(/\/+$/, "");
  const fold = (text: string) => /^[a-z]:\//i.test(normalizedRoot) ? text.toLowerCase() : text;
  if (fold(path) === fold(normalizedRoot)) path = "";
  else if (fold(path).startsWith(`${fold(normalizedRoot)}/`)) path = path.slice(normalizedRoot.length + 1);
  if (/^(?:\/|[a-z]:)/i.test(path)) return null;
  const parts = path.split("/").filter(part => part && part !== ".");
  if (parts.includes("..") || path.includes("\0")) return null;
  return parts.join("/");
}

export function workspaceFileParent(path: string): string {
  return path.replace(/\\/g, "/").split("/").slice(0, -1).join("/") || ".";
}

export function workspaceMovedPath(path: string, source: string, target: string, isDirectory: boolean): string | null {
  const normalized = path.replace(/\\/g, "/");
  const from = source.replace(/\\/g, "/").replace(/\/+$/, "");
  const to = target.replace(/\\/g, "/").replace(/\/+$/, "");
  const fold = (value: string) => /^[a-z]:\//i.test(from) ? value.toLowerCase() : value;
  if (fold(normalized) === fold(from)) return to;
  return isDirectory && fold(normalized).startsWith(`${fold(from)}/`) ? to + normalized.slice(from.length) : null;
}

export function resolveWorkspaceFileMove(data: WorkbenchReferenceDragData, target: WorkspaceFileDropScope) {
  if (!data.fileSystemMove || data.entries.length === 0 || data.origin.projectId !== target.projectId) return null;
  const from = data.origin.workspaceRef;
  const to = target.workspaceRef;
  if (from.checkoutId !== to.checkoutId || from.expectedGeneration !== to.expectedGeneration
    || from.expectedMaterializationEpoch !== to.expectedMaterializationEpoch) return null;
  const directory = workspaceRelativeEntryPath(target.root, target.directory);
  if (directory === null) return null;
  const fold = (path: string) => /^[a-z]:[/\\]/i.test(target.root) ? path.toLowerCase() : path;
  const sources: string[] = [];
  for (const entry of data.entries) {
    if (entry?.kind !== "file") return null;
    const sourcePath = workspaceRelativeEntryPath(data.origin.workspaceRoot, entry.path);
    if (!sourcePath) return null;
    // One folder dropped into itself or a descendant rejects the whole selection.
    if (entry.isDir && (fold(directory) === fold(sourcePath) || fold(directory).startsWith(`${fold(sourcePath)}/`))) return null;
    sources.push(sourcePath);
  }
  const selected = new Set(sources.map(fold));
  const seen = new Set<string>();
  const sourcePaths = sources.filter((path) => {
    const key = fold(path);
    if (seen.has(key)) return false;
    seen.add(key);
    // A descendant of another dragged folder already moves with that folder.
    const parts = key.split("/");
    if (parts.slice(0, -1).some((_, index) => selected.has(parts.slice(0, index + 1).join("/")))) return false;
    return fold(workspaceFileParent(path)) !== fold(directory || ".");
  });
  return sourcePaths.length ? { sourcePaths, targetDirectory: directory || ".", workspaceRef: { ...to } } : null;
}
