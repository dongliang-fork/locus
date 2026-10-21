// @vitest-environment jsdom
import { createApp, h, nextTick, type App } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import WorkspaceFilePreview from "../components/workbench/WorkspaceFilePreview.vue";
import type { ProjectExplorerFilePreview } from "../types/workbench";

const files = vi.hoisted(() => ({ read: vi.fn(), revision: vi.fn() }));
vi.mock("../services/workspaceExplorer", () => ({
  projectExplorerPreviewFile: files.read,
  workspaceFilePreview: files.read,
  projectExplorerWriteFile: vi.fn(),
  workspaceFileWrite: vi.fn(),
  explorerFileAction: vi.fn(),
  projectExplorerFileRevision: files.revision,
  workspaceFileRevision: files.revision,
  subscribeWorkspaceFileChanges: vi.fn(async () => () => {}),
}));

let app: App | null = null;
afterEach(() => { app?.unmount(); app = null; document.body.replaceChildren(); });

function markdown(path: string): ProjectExplorerFilePreview {
  const text = "# Notes\n";
  return {
    path, name: "notes.md", extension: "md", kind: "text", mimeType: "text/markdown", text,
    size: text.length, contentHash: "v1", totalLines: 1, truncated: false, editable: true,
    revision: { exists: true, size: text.length, modifiedAtNanos: "1", key: "v1" },
  };
}

async function mount(path: string, scoped: boolean): Promise<HTMLElement> {
  files.read.mockResolvedValue(markdown(path));
  files.revision.mockResolvedValue(markdown(path).revision);
  const host = document.createElement("div");
  document.body.appendChild(host);
  app = createApp({ setup: () => () => h(WorkspaceFilePreview, {
    projectId: "project-a", path, active: true,
    workspaceRef: scoped ? { checkoutId: "checkout-a", expectedGeneration: 1 } : null,
  }) });
  app.mount(host);
  for (let i = 0; i < 20; i++) await nextTick();
  return host;
}

describe("Markdown files opened from outside the workspace", () => {
  it("shows a read-only title because rename is workspace-only", async () => {
    const host = await mount("J:/notes/notes.md", true);
    expect(host.querySelector(".document-title-input")).toBeNull();
    expect(host.querySelector(".document-title")?.textContent).toBe("notes");
  });

  it("keeps rename for workspace files and mounted project files", async () => {
    expect((await mount("docs/notes.md", true)).querySelector(".document-title-input")).not.toBeNull();
    app?.unmount(); app = null;
    expect((await mount("J:/notes/notes.md", false)).querySelector(".document-title-input")).not.toBeNull();
  });
});
