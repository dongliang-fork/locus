// @vitest-environment jsdom
import { undo, undoDepth } from "@codemirror/commands";
import { EditorView } from "@codemirror/view";
import { createApp, h, nextTick, ref, type App } from "vue";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import WorkspaceFilePreview from "../components/workbench/WorkspaceFilePreview.vue";
import type { ProjectExplorerFilePreview } from "../types/workbench";

const files = vi.hoisted(() => ({ read: vi.fn(), write: vi.fn(), rename: vi.fn(), revision: vi.fn() }));
vi.mock("../services/workspaceExplorer", () => ({
  projectExplorerPreviewFile: files.read,
  workspaceFilePreview: files.read,
  projectExplorerWriteFile: files.write,
  workspaceFileWrite: files.write,
  explorerFileAction: files.rename,
  projectExplorerFileRevision: files.revision,
  workspaceFileRevision: files.revision,
  subscribeWorkspaceFileChanges: vi.fn(async () => () => {}),
}));

let app: App | null = null;
let disk: ProjectExplorerFilePreview;
const original = "# Notes\n\nPreserve this paragraph.\n";
const workspaceRef = { checkoutId: "checkout-a", expectedGeneration: 7 };
function preview(path = "F:/Game/notes.md", text = original, hash = "v1"): ProjectExplorerFilePreview {
  return {
    path, name: path.split("/").pop()!, extension: path.split(".").pop()!,
    kind: "text", mimeType: "text/markdown", text, size: text.length,
    contentHash: hash, totalLines: text.split("\n").length, truncated: false, editable: true,
    workspaceRelativePath: path.replace("F:/Game/", ""),
    revision: { exists: true, size: text.length, modifiedAtNanos: hash, key: hash },
  };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function flush() { for (let i = 0; i < 20; i++) await nextTick(); }
function insert(view: EditorView, text: string) {
  view.dispatch({ changes: { from: view.state.doc.length, insert: text },
    selection: { anchor: view.state.doc.length + text.length }, userEvent: "input.type" });
}
async function mountEditor(scoped = true, updateParent = true) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const path = ref(scoped ? disk.workspaceRelativePath! : disk.path);
  const scope = ref(scoped ? workspaceRef : null);
  const dirty = ref(false);
  const changed = vi.fn((value: string) => { if (updateParent) path.value = value; });
  const editor = ref<InstanceType<typeof WorkspaceFilePreview> | null>(null);
  app = createApp({ setup: () => () => h(WorkspaceFilePreview, {
    ref: editor, projectId: "project-a", path: path.value, workspaceRef: scope.value, active: true,
    onPathChange: changed, onDirtyChange: (value: boolean) => { dirty.value = value; },
  }) });
  app.mount(host);
  await flush();
  const getView = () => EditorView.findFromDOM(host.querySelector<HTMLElement>(".cm-editor")!)!;
  const input = () => host.querySelector<HTMLInputElement>(".document-title-input")!;
  const changeTitle = (name: string, event: "Enter" | "blur" | "compose" = "Enter") => {
    input().value = name;
    input().dispatchEvent(new Event("input", { bubbles: true }));
    input().dispatchEvent(event === "blur" ? new Event("blur")
      : new KeyboardEvent("keydown", { key: "Enter", bubbles: true, isComposing: event === "compose" }));
  };
  const scroller = host.querySelector<HTMLElement>(".document-scroller")!;
  scroller.style.overflowY = "auto";
  return { host, path, scope, dirty, editor, changed, getView, input, changeTitle, scroller };
}

beforeEach(() => {
  vi.resetAllMocks();
  disk = preview();
  files.read.mockImplementation(async (...args: unknown[]) => {
    const path = args.find(value => typeof value === "string" && value !== "project-a");
    if (typeof args[1] === "object" && path !== disk.workspaceRelativePath) throw Error(`Workspace path must be relative: ${path}`);
    if (path !== disk.path && path !== disk.workspaceRelativePath) throw Error(`Path not found: ${path}`);
    return { ...disk };
  });
  files.write.mockImplementation(async (...args: unknown[]) => {
    const [path, text] = (args[0] === "project-a" ? args.slice(1) : args) as [string, string];
    if (args[0] !== "project-a" && path !== disk.workspaceRelativePath) throw Error(`Workspace path must be relative: ${path}`);
    if (path !== disk.path && path !== disk.workspaceRelativePath) throw Error(`Stale write: ${path}`);
    disk = preview(disk.path, text, `${disk.contentHash}-saved`);
    return { ...disk };
  });
  files.rename.mockImplementation(async (_project: string, path: string, name: string, scope: unknown) => {
    if (scope && path !== disk.workspaceRelativePath) throw Error(`Workspace path must be relative: ${path}`);
    if (path !== disk.path && path !== disk.workspaceRelativePath) throw Error(`Stale rename: ${path}`);
    disk = preview(disk.path.replace(/[^/]+$/, () => name), disk.text!, disk.contentHash!);
    return disk.path;
  });
  files.revision.mockImplementation(async () => disk.revision);
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", {
    configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }),
  });
});
afterEach(() => { app?.unmount(); app = null; document.body.replaceChildren(); vi.useRealTimers(); });

describe("Markdown document title rename", () => {
  it("keeps the draft, selection, scroll and undo when the parent receives a new path before IPC completes", async () => {
    const mounted = await mountEditor();
    const view = mounted.getView();
    insert(view, "draft");
    const state = view.state;
    mounted.scroller.scrollTop = 240;
    const pending = deferred<string>();
    files.rename.mockImplementationOnce(async () => {
      disk = preview("F:/Game/新标题.md");
      mounted.path.value = disk.path;
      return pending.promise;
    });
    mounted.changeTitle("新标题");
    await flush();
    expect(files.rename).toHaveBeenCalledWith("project-a", "notes.md", "新标题.md", workspaceRef);
    // Read-only keeps focus and appearance while the rename is pending.
    expect(mounted.input().readOnly).toBe(true);
    expect(mounted.input().disabled).toBe(false);
    expect(files.read).toHaveBeenCalledTimes(1);
    await mounted.editor.value!.refreshIfChanged();
    expect(files.revision).not.toHaveBeenCalled();
    pending.resolve(disk.path);
    await flush();
    expect(mounted.getView()).toBe(view);
    expect(view.state.doc).toBe(state.doc);
    expect(view.state.selection.eq(state.selection)).toBe(true);
    expect(mounted.scroller.scrollTop).toBe(240);
    expect(mounted.dirty.value).toBe(true);
    expect(mounted.input().value).toBe("新标题");
    expect(mounted.input().readOnly).toBe(false);
    expect(mounted.changed).toHaveBeenCalledWith("新标题.md");
    await expect(mounted.editor.value!.saveFile()).resolves.toBe(true);
    expect(files.write.mock.lastCall?.[0]).toBe("新标题.md");
    expect(disk.text).toBe(`${original}draft`);
    expect(undoDepth(view.state)).toBe(1);
    expect(undo(view)).toBe(true);
    expect(view.state.doc.toString()).toBe(original);
    expect(mounted.host.textContent).not.toContain("Path not found");
  });

  it("waits for an in-flight autosave and writes later input only to the new path", async () => {
    const mounted = await mountEditor();
    const view = mounted.getView();
    const pending = deferred<ProjectExplorerFilePreview>();
    files.write.mockReturnValueOnce(pending.promise);
    insert(view, "submitted");
    await vi.waitFor(() => expect(files.write).toHaveBeenCalledOnce(), { timeout: 2000 });
    mounted.changeTitle("renamed", "blur");
    await flush();
    expect(files.rename).not.toHaveBeenCalled();
    insert(view, " continued");
    disk = preview(disk.path, `${original}submitted`, "v2");
    pending.resolve({ ...disk });
    await flush();
    expect(files.rename).toHaveBeenCalledOnce();
    expect(mounted.input().value).toBe("renamed");
    await expect(mounted.editor.value!.saveFile()).resolves.toBe(true);
    expect(files.write.mock.lastCall?.[0]).toBe("renamed.md");
    expect(files.write.mock.lastCall?.[2]).toBe("v2");
    expect(disk.text).toBe(`${original}submitted continued`);
  });

  it.each(["resolve", "reject"])("ignores an old-path read that settles after rename (%s)", async (result) => {
    const mounted = await mountEditor();
    const old = { ...disk };
    const pending = deferred<ProjectExplorerFilePreview>();
    files.revision.mockResolvedValueOnce({ ...disk.revision, key: "external" });
    files.read.mockReturnValueOnce(pending.promise);
    const refreshing = mounted.editor.value!.refreshIfChanged();
    await flush();
    mounted.changeTitle("new");
    await flush();
    if (result === "resolve") pending.resolve(old);
    else pending.reject(Error("Path not found: notes.md"));
    await refreshing;
    await flush();
    expect(mounted.input().value).toBe("new");
    expect(mounted.host.querySelector(".workspace-file-preview-inline-error")).toBeNull();
    expect(mounted.host.querySelector(".workspace-file-preview-conflict")).toBeNull();
    expect(mounted.getView().state.doc.toString()).toBe(original);
  });

  it("preserves failed renames for retry and does not submit Enter and blur twice", async () => {
    const mounted = await mountEditor();
    insert(mounted.getView(), "draft");
    const pending = deferred<string>();
    files.rename.mockReturnValueOnce(pending.promise);
    mounted.changeTitle("taken");
    mounted.input().dispatchEvent(new Event("blur"));
    await flush();
    expect(files.rename).toHaveBeenCalledOnce();
    pending.reject(Error("Target already exists"));
    await flush();
    expect(disk.path).toBe("F:/Game/notes.md");
    expect(mounted.input().value).toBe("taken");
    expect(mounted.host.textContent).toContain("Target already exists");
    expect(mounted.changed).not.toHaveBeenCalled();
    mounted.changeTitle("available", "blur");
    await flush();
    expect(files.rename.mock.lastCall?.[1]).toBe("notes.md");
    expect(disk.path).toBe("F:/Game/available.md");
    expect(mounted.getView().state.doc.toString()).toBe(`${original}draft`);
    expect(mounted.host.textContent).not.toContain("Target already exists");
  });

  it("keeps using the committed path when the parent has not acknowledged the rename", async () => {
    const mounted = await mountEditor(true, false);
    mounted.changeTitle("first");
    await flush();
    expect(mounted.path.value).toBe("notes.md");
    mounted.changeTitle("second");
    await flush();
    expect(files.rename.mock.lastCall?.[1]).toBe("first.md");
    expect(mounted.input().value).toBe("second");
    expect(files.read).toHaveBeenCalledOnce();
  });

  it("preserves the Markdown extension for mounted files and treats dollar signs literally", async () => {
    disk = preview("F:/Game/notes.markdown");
    const mounted = await mountEditor(false);
    mounted.changeTitle("price$&.markdown");
    await flush();
    expect(files.rename).toHaveBeenCalledWith("project-a", "F:/Game/notes.markdown", "price$&.markdown", null);
    expect(mounted.path.value).toBe("F:/Game/price$&.markdown");
    expect(mounted.input().value).toBe("price$&");
  });

  it("validates names and does not rename on IME confirmation or Escape", async () => {
    const mounted = await mountEditor();
    mounted.changeTitle("输入中", "compose");
    await flush();
    expect(files.rename).not.toHaveBeenCalled();
    mounted.input().dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await flush();
    expect(mounted.input().value).toBe("notes");
    for (const name of ["", "../bad", "CON", "a:b", "a?"]) {
      mounted.changeTitle(name);
      await flush();
      expect(mounted.host.querySelector(".document-title-error")).not.toBeNull();
    }
    expect(files.rename).not.toHaveBeenCalled();
  });

  it("does not apply a late rename response to another workspace", async () => {
    const mounted = await mountEditor();
    const pending = deferred<string>();
    files.rename.mockReturnValueOnce(pending.promise);
    mounted.changeTitle("renamed");
    await flush();
    disk = preview("F:/Game/other.md", "# Other workspace");
    mounted.scope.value = { checkoutId: "checkout-b", expectedGeneration: 9 };
    mounted.path.value = disk.workspaceRelativePath!;
    await flush();
    pending.resolve("F:/Game/renamed.md");
    await flush();
    expect(mounted.input().value).toBe("other");
    expect(mounted.getView().state.doc.toString()).toBe("# Other workspace");
    expect(mounted.changed).not.toHaveBeenCalled();
  });
});
