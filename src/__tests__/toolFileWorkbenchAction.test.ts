// @vitest-environment jsdom
import { createApp, nextTick } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import ToolCallBlock from "../components/ToolCallBlock.vue";
import { WORKBENCH_FILE_OPEN_KEY } from "../services/workbenchFile";

const mocks = vi.hoisted(() => ({
  notice: vi.fn(),
  fallbackOpen: vi.fn(),
  focusedWorkspaceRef: { checkoutId: "other-checkout", expectedGeneration: 9 },
}));
vi.mock("../stores/project", () => ({
  useProjectStore: () => ({ workingDir: "F:/Game", extraWorkdirs: {} }),
}));
vi.mock("../stores/workspaceContext", () => ({
  useWorkspaceContextStore: () => ({ focusedWorkspaceRef: mocks.focusedWorkspaceRef }),
}));
vi.mock("../stores/notification", () => ({
  useNotificationStore: () => ({ addNotice: mocks.notice }),
}));
vi.mock("../services/workbenchFile", () => ({
  WORKBENCH_FILE_OPEN_KEY: Symbol("workbench-file-open"),
  openWorkbenchFileTab: mocks.fallbackOpen,
}));
vi.mock("../components/MarkdownRenderer.vue", () => ({ default: { render: () => null } }));
vi.mock("../components/diff/FileDiffViewer.vue", () => ({ default: { render: () => null } }));
vi.mock("../components/tool-block-overrides/toolBlockOverrides", () => ({
  resolveToolBlockOverride: () => null,
}));

afterEach(() => { vi.clearAllMocks(); });

describe("native tool output rendering", () => {
  it("renders legacy writes as summaries and preserves new write metadata", async () => {
    for (const output of [
      JSON.stringify({ type: "update", filePath: "plan.md", content: "NEW_BODY", originalFile: "OLD_BODY" }),
      "Edited plan.md\nKnowledge index: updated",
    ]) {
      const host = document.createElement("div");
      const app = createApp(ToolCallBlock, { initialExpanded: true, toolCall: { id: "write", name: "write", arguments: '{"filePath":"plan.md"}', output, status: "done" } });
      app.mount(host);
      try {
        await nextTick();
        expect(host.textContent).toContain("Edited plan.md");
        expect(host.textContent).not.toContain("NEW_BODY");
        expect(host.textContent).not.toContain("OLD_BODY");
        if (output.includes("Knowledge index")) expect(host.textContent).toContain("Knowledge index: updated");
      } finally { app.unmount(); }
    }
  });

  it("renders native todo snapshots while preserving operation warnings", async () => {
    const output = JSON.stringify({ locusTodoSnapshot: { version: 1, items: [{ content: "Task one", status: "completed", priority: "medium" }], tasks: [{ description: "HIDDEN_SYNC_DETAIL" }] }, locusOutput: "Updated task; dependency warning" });
    const host = document.createElement("div");
    const app = createApp(ToolCallBlock, { initialExpanded: true, toolCall: { id: "task", name: "TaskUpdate", arguments: "{}", output, status: "done" } });
    app.mount(host);
    try {
      await nextTick();
      expect(host.textContent).toContain("Task one");
      expect(host.textContent).toContain("dependency warning");
      expect(host.textContent).not.toContain("HIDDEN_SYNC_DETAIL");
      expect(host.textContent).not.toContain("locusTodoSnapshot");
    } finally { app.unmount(); }
  });

  it("does not hide failed task results", async () => {
    const host = document.createElement("div");
    const app = createApp(ToolCallBlock, { initialExpanded: true, toolCall: { id: "task", name: "TaskUpdate", arguments: "{}", output: "Permission denied", status: "error" } });
    app.mount(host);
    try { await nextTick(); expect(host.textContent).toContain("Permission denied"); }
    finally { app.unmount(); }
  });
});

describe("tool file Workbench actions", () => {
  it.each(["read", "edit", "write"])("opens %s in its containing Workbench using the session checkout", async (name) => {
    const workspaceRef = { checkoutId: "session-checkout", expectedGeneration: 7, expectedMaterializationEpoch: 3 };
    const open = vi.fn().mockResolvedValue(undefined);
    const app = createApp(ToolCallBlock, {
      toolCall: { id: "file-call", name, arguments: JSON.stringify({ filePath: "Assets/Player.cs" }), status: "done" },
      workspaceRef,
    });
    app.provide(WORKBENCH_FILE_OPEN_KEY, open);
    const host = document.createElement("div");
    app.mount(host);
    try {
      host.querySelector<HTMLButtonElement>(".tool-file-preview-action")!.click();
      await nextTick();
      expect(open).toHaveBeenCalledWith(expect.objectContaining({ filePath: "Assets/Player.cs", workspaceRef }));
      expect(mocks.fallbackOpen).not.toHaveBeenCalled();
      expect(mocks.notice).not.toHaveBeenCalled();
    } finally { app.unmount(); }
  });

  it("does not open a different checkout when the session workspace is unavailable", async () => {
    const open = vi.fn();
    const app = createApp(ToolCallBlock, {
      toolCall: { id: "read-call", name: "read", arguments: '{"filePath":"Assets/Player.cs"}', status: "done" },
      workspaceRef: null,
    });
    app.provide(WORKBENCH_FILE_OPEN_KEY, open);
    const host = document.createElement("div");
    app.mount(host);
    try {
      host.querySelector<HTMLButtonElement>(".tool-file-preview-action")!.click();
      await nextTick();
      expect(open).not.toHaveBeenCalled();
      expect(mocks.notice).toHaveBeenCalledWith("error", expect.any(String), expect.objectContaining({ operation: "openWorkbenchFileTab" }));
    } finally { app.unmount(); }
  });
});
