// @vitest-environment jsdom
import { createApp, h } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import WorkbenchEditorTabs from "../components/workbench/WorkbenchEditorTabs.vue";
import { provideInternalDragController } from "../composables/useInternalDrag";
import { createWorkbenchEditorInput } from "../stores/workbench";
import type { WorkbenchEditorGroup, WorkbenchResourceRef } from "../types/workbench";

vi.mock("../i18n", () => ({ t: (key: string) => key }));
vi.mock("../stores/chat", () => ({ useChatStore: () => ({ streamingSessionIds: new Set(), sessions: [] }) }));
const cleanups: Array<() => void> = [];
afterEach(() => { cleanups.splice(0).reverse().forEach((cleanup) => cleanup()); document.body.replaceChildren(); });

function dirtyDotShown(resource: WorkbenchResourceRef, title: string): boolean {
  const editor = createWorkbenchEditorInput(resource, title, { dirty: true });
  const group: WorkbenchEditorGroup = { paneId: "pane", tabs: [editor], activeEditorId: editor.editorId };
  const host = document.createElement("div");
  document.body.append(host);
  const app = createApp({ setup() {
    const controller = provideInternalDragController();
    cleanups.push(() => controller.dispose());
    return () => h(WorkbenchEditorTabs, { windowId: "main", group, showSingleTab: true });
  } });
  app.mount(host);
  cleanups.push(() => app.unmount());
  const tab = host.querySelector(`[data-workbench-tab-id="${editor.editorId}"]`);
  if (!tab) throw new Error("tab missing");
  return tab.querySelector(".base-tab-dirty") !== null;
}

describe("Workbench tab dirty indicator", () => {
  it("hides the dot for autosaved Markdown knowledge documents", () => {
    expect(dirtyDotShown({ kind: "knowledge", projectId: "p", documentId: "d" }, "user-preference.md")).toBe(false);
  });

  it("keeps the dot for manually saved knowledge CSV tables", () => {
    expect(dirtyDotShown({ kind: "knowledge", projectId: "p", documentId: "d" }, "items.csv")).toBe(true);
  });

  it("keeps the existing rule for workspace files", () => {
    expect(dirtyDotShown({ kind: "workspaceFile", projectId: "p", path: "docs/readme.md" }, "readme.md")).toBe(false);
    expect(dirtyDotShown({ kind: "workspaceFile", projectId: "p", path: "src/main.ts" }, "main.ts")).toBe(true);
  });
});
