import type { WorkbenchEditorGroup, WorkbenchEditorInput } from "../../types/workbench";

function isSessionTab(editor: WorkbenchEditorInput): boolean {
  return editor.resource.kind === "session" || editor.resource.kind === "newSession";
}

/**
 * Session tabs show their workspace name only while one window holds sessions
 * from more than one workspace. Every pane of the window counts.
 */
export function sessionTabWorkspaceLabels(
  groups: Iterable<WorkbenchEditorGroup>,
  workspaceLabel: (editor: WorkbenchEditorInput) => string | null,
): Map<string, string> {
  const labels = new Map<string, string>();
  for (const group of groups) {
    for (const editor of group.tabs) {
      if (!isSessionTab(editor)) continue;
      const label = workspaceLabel(editor);
      if (label) labels.set(editor.editorId, label);
    }
  }
  return new Set(labels.values()).size > 1 ? labels : new Map();
}
