// @vitest-environment jsdom
import { createApp, h } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import WorkbenchEditorTabs from "../components/workbench/WorkbenchEditorTabs.vue";
import { sessionTabWorkspaceLabels } from "../components/workbench/workbenchTabWorkspace";
import { provideInternalDragController } from "../composables/useInternalDrag";
import { createWorkbenchEditorInput } from "../stores/workbench";
import type { WorkbenchEditorGroup, WorkbenchEditorInput } from "../types/workbench";

vi.mock("../i18n", () => ({ t: (key: string) => key }));
vi.mock("../stores/chat", () => ({ useChatStore: () => ({ streamingSessionIds: new Set(), sessions: [] }) }));
const cleanups: Array<() => void> = [];
afterEach(() => { cleanups.splice(0).reverse().forEach((cleanup) => cleanup()); document.body.replaceChildren(); });

const workspaceNames: Record<string, string> = { unity: "DustEcho-6.5", locus: "locus" };
const byProject = (editor: WorkbenchEditorInput) => workspaceNames[editor.resource.projectId] ?? null;

function session(projectId: string, sessionId: string): WorkbenchEditorInput {
  return createWorkbenchEditorInput({ kind: "session", projectId, sessionId }, sessionId);
}

function group(paneId: string, tabs: WorkbenchEditorInput[]): WorkbenchEditorGroup {
  return { paneId, tabs, activeEditorId: tabs[0]?.editorId ?? null };
}

describe("session tab workspace labels", () => {
  it("names every session tab once sessions from two workspaces share a window", () => {
    const unity = session("unity", "c1010");
    const locus = session("locus", "merge");
    const draft = createWorkbenchEditorInput({ kind: "newSession", projectId: "unity" }, "New chat");
    const labels = sessionTabWorkspaceLabels([group("left", [unity, draft]), group("right", [locus])], byProject);
    expect(Object.fromEntries(labels)).toEqual({
      [unity.editorId]: "DustEcho-6.5",
      [draft.editorId]: "DustEcho-6.5",
      [locus.editorId]: "locus",
    });
  });

  it("keeps plain titles while every session belongs to one workspace", () => {
    const labels = sessionTabWorkspaceLabels([group("left", [session("unity", "a")]), group("right", [session("unity", "b")])], byProject);
    expect(labels.size).toBe(0);
  });

  it("ignores non-session tabs when deciding and labelling", () => {
    const file = createWorkbenchEditorInput({ kind: "workspaceFile", projectId: "locus", path: "src/main.ts" }, "main.ts");
    const labels = sessionTabWorkspaceLabels([group("main", [session("unity", "a"), file])], byProject);
    expect(labels.size).toBe(0);
    const mixed = sessionTabWorkspaceLabels([group("main", [session("unity", "a"), session("locus", "b"), file])], byProject);
    expect(mixed.has(file.editorId)).toBe(false);
  });

  it("renders the workspace label after the title", () => {
    const editor = session("unity", "Check C1010");
    const host = document.createElement("div");
    document.body.append(host);
    const app = createApp({ setup() {
      const controller = provideInternalDragController();
      cleanups.push(() => controller.dispose());
      return () => h(WorkbenchEditorTabs, { windowId: "main", group: group("pane", [editor]), showSingleTab: true,
        workspaceLabel: () => "DustEcho-6.5" });
    } });
    app.mount(host);
    cleanups.push(() => app.unmount());
    const tab = host.querySelector<HTMLElement>(`[data-workbench-tab-id="${editor.editorId}"]`)!;
    expect(tab.querySelector(".base-tab-title")?.textContent).toBe("Check C1010");
    expect(tab.querySelector(".base-tab-detail")?.textContent).toBe("DustEcho-6.5");
    expect(tab.title).toBe("Check C1010\nDustEcho-6.5");
  });
});
