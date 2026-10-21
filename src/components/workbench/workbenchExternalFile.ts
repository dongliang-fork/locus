import type { WorkbenchEditorInput } from "../../types/workbench";

/**
 * Workspace file tabs keep files inside the workspace as relative paths; an
 * absolute path means the file was opened from outside the workspace, such as
 * a chat file reference to another drive.
 */
export function isExternalWorkspaceFilePath(path: string): boolean {
  return /^(?:[a-z]:[\\/]|[\\/])/i.test(path.trim());
}

export function isExternalWorkspaceFileEditor(editor: WorkbenchEditorInput): boolean {
  return editor.resource.kind === "workspaceFile"
    && isExternalWorkspaceFilePath(editor.resource.path);
}
