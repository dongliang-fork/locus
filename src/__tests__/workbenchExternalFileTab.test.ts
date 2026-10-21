// @vitest-environment jsdom
import { createApp, h } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import WorkbenchEditorTabs from "../components/workbench/WorkbenchEditorTabs.vue";
import { isExternalWorkspaceFilePath } from "../components/workbench/workbenchExternalFile";
import { provideInternalDragController } from "../composables/useInternalDrag";
import { createWorkbenchEditorInput } from "../stores/workbench";
import type { WorkbenchEditorGroup, WorkbenchResourceRef } from "../types/workbench";

vi.mock("../i18n", () => ({ t: (key: string) => key }));
vi.mock("../stores/chat", () => ({ useChatStore: () => ({ streamingSessionIds: new Set(), sessions: [] }) }));
const cleanups: Array<() => void> = [];
afterEach(() => { cleanups.splice(0).reverse().forEach((cleanup) => cleanup()); document.body.replaceChildren(); });

function tabDetail(resource: WorkbenchResourceRef, title: string): string | null {
  const editor = createWorkbenchEditorInput(resource, title);
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
  return tab.querySelector(".base-tab-detail")?.textContent ?? null;
}

describe("Workbench external file tabs", () => {
  it("treats absolute workspace file paths as outside the workspace", () => {
    expect(isExternalWorkspaceFilePath("J:/glb_motion_analysis/semantic/diagonal_cut.py")).toBe(true);
    expect(isExternalWorkspaceFilePath("J:\\glb\\cut.py")).toBe(true);
    expect(isExternalWorkspaceFilePath("//server/share/cut.py")).toBe(true);
    expect(isExternalWorkspaceFilePath("/home/user/cut.py")).toBe(true);
    expect(isExternalWorkspaceFilePath("src/main.ts")).toBe(false);
    expect(isExternalWorkspaceFilePath("Assets/Scripts/Player.cs")).toBe(false);
  });

  it("marks only external workspace file tabs", () => {
    expect(tabDetail({
      kind: "workspaceFile", projectId: "p", path: "J:/glb_motion_analysis/semantic/diagonal_cut.py",
    }, "diagonal_cut.py")).toBe("workbench.tabs.outsideWorkspace");
    expect(tabDetail({ kind: "workspaceFile", projectId: "p", path: "src/main.ts" }, "main.ts")).toBeNull();
  });
});
