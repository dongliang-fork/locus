import type { WorkspaceRef } from "../../services/project";
import type { AssetSearchResult } from "../../types";
import type { AssetExplorerNode } from "../../composables/useAssetState";
import type {
  WorkbenchReferenceDragData,
  WorkbenchReferenceDragEntry,
} from "../workbench/workbenchReferenceDrag";

export interface AssetWorkspaceDragContext {
  projectId: string;
  workspaceRef: WorkspaceRef;
  workspaceRoot: string;
}

export type AssetWorkspaceDragEntry = AssetExplorerNode | AssetSearchResult;

function entryName(entry: AssetWorkspaceDragEntry): string {
  return entry.name.trim() || entry.path.split("/").filter(Boolean).pop() || entry.path;
}

function isDirectory(entry: AssetWorkspaceDragEntry): boolean {
  return "depth" in entry ? entry.kind === "folder" : entry.isDirectory === true;
}

export function assetWorkspaceReferenceEntry(
  entry: AssetWorkspaceDragEntry,
): WorkbenchReferenceDragEntry | null {
  const path = entry.path.trim().replace(/\\/g, "/").replace(/\/+$/g, "");
  if (!path) return null;
  return {
    kind: "file",
    path,
    isDir: isDirectory(entry),
    name: entryName(entry),
    typeLabel: "typeLabel" in entry ? entry.typeLabel : undefined,
  };
}

export function assetWorkspaceReferenceDragData(
  context: AssetWorkspaceDragContext,
  entry: AssetWorkspaceDragEntry | AssetWorkspaceDragEntry[],
): WorkbenchReferenceDragData | null {
  const normalizedProjectId = context.projectId.trim();
  const normalizedRoot = context.workspaceRoot.trim();
  const entries = Array.isArray(entry) ? entry : [entry];
  const references = entries.map(assetWorkspaceReferenceEntry);
  if (!normalizedProjectId || !normalizedRoot || references.length === 0
    || references.some((reference) => !reference)) return null;
  return {
    version: 1,
    fileSystemMove: entries.every((candidate) => !("isSubAsset" in candidate && candidate.isSubAsset)),
    origin: {
      projectId: normalizedProjectId,
      workspaceRef: context.workspaceRef,
      workspaceRoot: normalizedRoot,
    },
    entries: references as WorkbenchReferenceDragEntry[],
  };
}
