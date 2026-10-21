// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import ts from "typescript";
import { computed, nextTick, ref } from "vue";
import { describe, expect, it, vi } from "vitest";
import { workspaceFileParent } from "../components/explorer/workspaceFileMove";
import { validResourceName } from "../components/explorer/explorerResourceActions";

// Exercise production handlers with backend IO and unrelated panes isolated.
const source = readFileSync("src/components/workbench/DevelopmentWorkbench.vue", "utf8")
  .split('<script setup lang="ts">')[1]!.split("</script>")[0]!;
const parsed = ts.createSourceFile("workbench.ts", source, ts.ScriptTarget.Latest, true);
const names = ["treeFileCreateTarget", "beginCreateWorkspaceDocument", "submitInlineCreate", "cancelInlineCreate",
  "appendWorkspaceFileCreate", "openExplorerBackgroundContextMenu"];
const code = ts.transpileModule(parsed.statements.filter(statement => ts.isFunctionDeclaration(statement)
  && statement.name && names.includes(statement.name.text)).map(statement => statement.getText(parsed)).join("\n"),
{ compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;

function fixture() {
  const workspaceRef = { checkoutId: "worktree", expectedGeneration: 4, expectedMaterializationEpoch: 2 };
  const checkout = { projectId: "p", checkoutId: "worktree", root: "F:/worktree", runtime: {} };
  const mode = ref("files"), inlineCreate = ref<any>(null), contextMenu = ref<any>(null);
  const input = { focus: vi.fn(), select: vi.fn() };
  const state = { entries: [] as any[], toggle: vi.fn(), refresh: vi.fn().mockResolvedValue(undefined) };
  const create = vi.fn().mockImplementation(async (_ref, directory, name) => directory === "." ? name : `${directory}/${name}`);
  const createFolder = vi.fn().mockImplementation(async (_ref, directory, name) => directory === "." ? name : `${directory}/${name}`);
  const open = vi.fn().mockResolvedValue(undefined), notice = vi.fn();
  let api: any;
  const deps = {
    nextTick, inlineCreate, contextMenu, workspaceFileParent, validResourceName,
    projectCheckout: vi.fn(() => checkout), navigationCheckout: vi.fn(() => checkout),
    workspaceNavigation: { getMode: () => mode.value }, checkoutWorkspaceRef: () => ({ ...workspaceRef }),
    contextCreateTarget: computed(() => contextMenu.value ? api.treeFileCreateTarget(contextMenu.value.item) : null),
    expanded: ref(new Set()), workspaceFileStates: { worktree: state }, inlineCreateInput: ref(input),
    treeItems: ref([]), workspaceTreeRef: ref(null), workspaceCreateDocument: create, workspaceCreateFolder: createFolder,
    openWorkbenchResourceFromWorkspaceTree: open, notificationStore: { addNotice: notice },
    normalizeAppError: (error: Error) => error, t: (key: string) => key, explorerStore: { applyOperations: vi.fn() },
    activeResource: ref({ projectId: "p" }), workspaceContextStore: {}, visibleProjects: ref([]), resetSessionMultiSelection: vi.fn(),
    resetFileMultiSelection: vi.fn(),
  };
  api = new Function(...Object.keys(deps), `${code}\nreturn { ${names.join(", ")} };`)(...Object.values(deps));
  const item = (path?: string, folder = false) => ({ key: path ?? "root", treeRow: { depth: 0, expanded: true }, meta: {
    kind: path ? "workspaceFile" : "project", projectId: "p", checkoutId: path ? "worktree" : undefined,
    workspaceFile: path ? { node: { path, kind: folder ? "folder" : "file" } } : undefined,
  } });
  return { api, deps, mode, item, inlineCreate, contextMenu, state, create, createFolder, open, notice, workspaceRef };
}

describe("workspace file creation", () => {
  it("uses the context checkout and the clicked folder or file's parent", () => {
    const f = fixture();
    expect(f.api.treeFileCreateTarget(f.item())).toMatchObject({ directory: ".", workspaceRef: f.workspaceRef });
    expect(f.api.treeFileCreateTarget(f.item("Docs", true)).directory).toBe("Docs");
    expect(f.api.treeFileCreateTarget(f.item("Docs/a.md")).directory).toBe("Docs");
    expect(f.api.treeFileCreateTarget(f.item("a.md")).directory).toBe(".");
    expect(f.api.treeFileCreateTarget({ meta: { kind: "newSession" } })).toBeNull();
    f.mode.value = "sessions";
    expect(f.api.treeFileCreateTarget(f.item())).toBeNull();
  });

  it("opens a root menu only in the file tree, leaving toolbar and row menus alone", () => {
    const f = fixture();
    const root = document.createElement("aside");
    root.innerHTML = '<div class="development-explorer-toolbar"></div><div class="development-tree"><div data-tree-key="file"></div><div class="development-inline-create-row"></div></div>';
    for (const selector of [".development-explorer-toolbar", "[data-tree-key]", ".development-inline-create-row", ".development-tree"]) {
      const event = new MouseEvent("contextmenu", { cancelable: true });
      Object.defineProperty(event, "target", { value: root.querySelector(selector) });
      f.api.openExplorerBackgroundContextMenu(event);
      expect(!!f.contextMenu.value).toBe(selector === ".development-tree");
      expect(event.defaultPrevented).toBe(selector === ".development-tree");
    }
  });

  it.each([["document", "notes", "notes.md"], ["document", "notes.MD", "notes.MD"], ["csv", "data", "data.csv"]])(
    "creates %s, refreshes the list and opens the new file", async (kind, name, fileName) => {
      const f = fixture();
      f.contextMenu.value = { item: f.item("Docs", true) };
      f.state.entries.push({ node: { path: "Docs", kind: "folder" }, treeRow: { expanded: false } });
      await f.api.beginCreateWorkspaceDocument(kind);
      expect(f.state.toggle).toHaveBeenCalledWith("Docs");
      expect(f.contextMenu.value).toBeNull();
      f.inlineCreate.value.name = name;
      await Promise.all([f.api.submitInlineCreate(), f.api.submitInlineCreate()]);
      expect(f.create).toHaveBeenCalledExactlyOnceWith(f.workspaceRef, "Docs", fileName);
      expect(f.state.refresh).toHaveBeenCalledWith(`Docs/${fileName}`, "upsert");
      expect(f.open).toHaveBeenCalledWith({ resource: { kind: "asset", projectId: "p", path: `Docs/${fileName}` },
        title: fileName, checkoutId: "worktree" }, { preview: false, pinned: true });
      expect(f.inlineCreate.value).toBeNull();
    },
  );

  it("creates a folder inside the clicked folder, refreshes the list and opens nothing", async () => {
    const f = fixture();
    f.contextMenu.value = { item: f.item("Docs/a.md") };
    await f.api.beginCreateWorkspaceDocument("folder");
    expect(f.inlineCreate.value).toMatchObject({ kind: "folder", fileTarget: { directory: "Docs" } });
    f.inlineCreate.value.name = "Sub.md";
    await Promise.all([f.api.submitInlineCreate(), f.api.submitInlineCreate()]);
    expect(f.createFolder).toHaveBeenCalledExactlyOnceWith(f.workspaceRef, "Docs", "Sub.md");
    expect(f.create).not.toHaveBeenCalled();
    expect(f.state.refresh).toHaveBeenCalledWith("Docs/Sub.md", "upsert");
    expect(f.open).not.toHaveBeenCalled();
    expect(f.deps.explorerStore.applyOperations).not.toHaveBeenCalled();
    expect(f.inlineCreate.value).toBeNull();
  });

  it("keeps the folder name after a failed create but not after a failed refresh", async () => {
    const f = fixture();
    f.contextMenu.value = { item: f.item() };
    await f.api.beginCreateWorkspaceDocument("folder");
    f.inlineCreate.value.name = "Art";
    f.createFolder.mockRejectedValueOnce(new Error("already exists"));
    await f.api.submitInlineCreate();
    expect(f.inlineCreate.value).toMatchObject({ kind: "folder", name: "Art" });
    expect(f.notice).toHaveBeenCalledWith("error", "already exists");
    f.state.refresh.mockRejectedValueOnce(new Error("refresh failed"));
    await f.api.submitInlineCreate();
    expect(f.inlineCreate.value).toBeNull();
    expect(f.createFolder).toHaveBeenCalledTimes(2);
    expect(f.notice).toHaveBeenCalledWith("error", "refresh failed");
  });

  it("places the naming row inside the requested folder and only in its checkout", async () => {
    const f = fixture();
    f.contextMenu.value = { item: f.item("Docs", true) };
    await f.api.beginCreateWorkspaceDocument("document");
    const rows = [f.item("Docs", true), f.item("Docs/a.md"), f.item("Other", true)];
    f.api.appendWorkspaceFileCreate(rows, "other-checkout", 0);
    expect(rows).toHaveLength(3);
    f.api.appendWorkspaceFileCreate(rows, "worktree", 0);
    expect(rows[1]!.meta).toMatchObject({ kind: "inlineCreate", inlineCreateDepth: 1 });
    f.api.cancelInlineCreate();
    expect(f.inlineCreate.value).toBeNull();
    expect(f.create).not.toHaveBeenCalled();
  });

  it("preserves the name on a collision but never retries a successful creation after an open failure", async () => {
    const f = fixture();
    f.contextMenu.value = { item: f.item() };
    await f.api.beginCreateWorkspaceDocument("csv");
    f.inlineCreate.value.name = "data";
    f.create.mockRejectedValueOnce(new Error("already exists"));
    await f.api.submitInlineCreate();
    expect(f.inlineCreate.value.name).toBe("data");
    expect(f.notice).toHaveBeenCalledWith("error", "already exists");
    f.open.mockRejectedValueOnce(new Error("open failed"));
    await f.api.submitInlineCreate();
    expect(f.inlineCreate.value).toBeNull();
    expect(f.create).toHaveBeenCalledTimes(2);
  });

  it("rejects path traversal before requesting a create", async () => {
    const f = fixture();
    f.contextMenu.value = { item: f.item() };
    await f.api.beginCreateWorkspaceDocument("document");
    f.inlineCreate.value.name = "../outside";
    await f.api.submitInlineCreate();
    expect(f.create).not.toHaveBeenCalled();
    expect(f.inlineCreate.value.name).toBe("../outside");
    expect(f.notice).toHaveBeenCalledWith("error", "development.invalidFileName");
  });
});
