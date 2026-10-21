// @vitest-environment jsdom
import { createApp, defineComponent, h, nextTick, reactive, ref, type App, type Component } from "vue";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { t } from "../i18n";
import ResourceFileMenuItems from "../components/explorer/ResourceFileMenuItems.vue";
import ExplorerResourceActions from "../components/explorer/ExplorerResourceActions.vue";
import WorkspaceTree from "../components/explorer/WorkspaceTree.vue";
import AssetDirectoryList from "../components/asset/AssetDirectoryList.vue";
import AssetLegacyExplorer from "../components/asset/AssetLegacyExplorer.vue";
import AssetSearchResults from "../components/asset/AssetSearchResults.vue";
import KnowledgeExplorer from "../components/knowledge/KnowledgeExplorer.vue";
import { explorerFileKey, explorerKnowledgeKey, useExplorerPathDisplay } from "../composables/useExplorerPathDisplay";
import { resourceRelativePath, type ExplorerResourceRename, type ExplorerResourceTarget } from "../components/explorer/explorerResourceActions";
import type { KnowledgeDocumentSummary } from "../types";

const mocks = vi.hoisted(() => ({
  fileAction: vi.fn(), knowledgeMove: vi.fn(), knowledgeDelete: vi.fn(), confirm: vi.fn(), notice: vi.fn(),
  refresh: vi.fn(), load: vi.fn(), copy: vi.fn(), loadMount: vi.fn(),
  snapshots: {} as Record<string, { nodes: { nodeId: string; nodeKind: string; sourcePath: string }[] }>,
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ confirm: mocks.confirm, save: vi.fn() }));
vi.mock("../stores/notification", () => ({ useNotificationStore: () => ({ addNotice: mocks.notice }) }));
vi.mock("../stores/workspaceExplorer", () => ({ useWorkspaceExplorerStore: () => ({
  refreshProjectKnowledge: mocks.refresh, loadProject: mocks.load, snapshots: mocks.snapshots, loadMount: mocks.loadMount,
}) }));
vi.mock("../services/workspaceExplorer", () => ({ explorerFileAction: mocks.fileAction }));
vi.mock("../services/knowledge", () => ({ knowledgeMove: mocks.knowledgeMove, knowledgeDelete: mocks.knowledgeDelete,
  deleteSkillPackage: vi.fn(), exportSkillPackage: vi.fn(), knowledgeRevealTarget: vi.fn(),
}));
vi.mock("../composables/useKnowledgeState", () => ({
  isSkillPackageRootDocument: (document: KnowledgeDocumentSummary) => document.externalSource?.provider === "package" && document.path.endsWith("/SKILL.md"),
  skillPackageIdForDocument: (document: KnowledgeDocumentSummary) => document.externalSource?.sourceId,
}));

const workspaceRef = { checkoutId: "checkout-a", expectedGeneration: 7, expectedMaterializationEpoch: 2 };
const file: ExplorerResourceTarget = { projectId: "project-a", root: "F:/Project", path: "F:/Project/docs/design.md", workspaceRef };
const document: KnowledgeDocumentSummary = {
  id: "doc-a", type: "design", path: "combat/design.md", title: "Combat", injectMode: "excerpt",
  effectiveInjectMode: "excerpt", readOnly: false, aiMaintained: false, effectiveAiMaintained: false, modifiedAt: 1,
};
const knowledge: ExplorerResourceTarget = { ...file, path: document.path, document };
const apps: App[] = [];
function mount(component: Component, props: Record<string, unknown> = {}) {
  const host = window.document.createElement("div");
  window.document.body.appendChild(host);
  // Exercise the same renderless controller + inline tree editor used by the workbench.
  const root = component === ExplorerResourceActions ? defineComponent({
    setup(_, { expose }) {
      const actions = ref<InstanceType<typeof ExplorerResourceActions> | null>(null);
      expose({ run: async (...args: Parameters<InstanceType<typeof ExplorerResourceActions>["run"]>) => {
        await actions.value!.run(...args);
        await nextTick();
      }, runMany: async (...args: Parameters<InstanceType<typeof ExplorerResourceActions>["runMany"]>) => {
        await actions.value!.runMany(...args);
        await nextTick();
      } });
      return () => h("div", [
        h(WorkspaceTree, { resourceRename: actions.value?.draft, rowTabIndex: 0,
          items: [{ key: actions.value?.draft?.key ?? file.path,
            treeRow: { key: actions.value?.draft?.key ?? file.path, name: "design.md", kind: "file", depth: 0, dragEnabled: true } }],
        }),
        h(ExplorerResourceActions, { ...props, ref: actions }),
      ]);
    },
  }) : component;
  const app = createApp(root, component === ExplorerResourceActions ? {} : props);
  const instance = app.mount(host);
  apps.push(app);
  return { host, instance: instance as unknown as InstanceType<typeof ExplorerResourceActions> };
}
async function flush() { for (let i = 0; i < 10; i++) await nextTick(); }
function button(host: Element, label: string) {
  const found = [...host.querySelectorAll<HTMLButtonElement>("button")].find((item) => item.textContent?.trim() === label);
  expect(found, label).toBeDefined();
  return found!;
}
function submit(name: string) {
  const input = window.document.querySelector<HTMLInputElement>(".resource-rename-input")!;
  input.value = name;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
}
beforeEach(() => {
  vi.clearAllMocks();
  delete mocks.snapshots["project-a"];
  localStorage.removeItem("locus:explorerFullPaths");
  window.dispatchEvent(new StorageEvent("storage", { key: "locus:explorerFullPaths" }));
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: mocks.copy } });
  mocks.fileAction.mockResolvedValue("F:/Project/docs/renamed.md");
  mocks.knowledgeMove.mockResolvedValue({});
});
afterEach(() => { apps.splice(0).forEach((app) => app.unmount()); window.document.body.innerHTML = ""; });

describe("explorer resource menus", () => {
  const node = { kind: "file", path: "docs/design.md", name: "design.md", depth: 0 };
  it.each([
    ["directory", AssetDirectoryList, { items: [node], loaded: true, loading: false, hasMore: false, emptyLabel: "" }],
    ["legacy tree", AssetLegacyExplorer, { tree: [node], isPathExpanded: (): boolean => true }],
    ["workspace tree", AssetLegacyExplorer, { tree: [node], isPathExpanded: (): boolean => true, workspaceStyle: true }],
    ["search", AssetSearchResults, { results: [{ ...node, kind: "md", root: "workspace", isDirectory: false }], query: "design", searching: false, hasFallback: false, truncated: false }],
  ] as const)("edits in the %s row without opening or dragging the file", async (_, component, props) => {
    const selected = vi.fn(), dragged = vi.fn(), submitRename = vi.fn();
    const rename = reactive<ExplorerResourceRename>({ key: node.path, target: file, name: node.name,
      busy: false, error: "", submit: submitRename, cancel: vi.fn() });
    const list = mount(component, { ...props, workingDir: file.root, selectedPath: node.path,
      resourceRename: rename, dragEnabled: true, onSelect: selected, onDragPointerDown: dragged });
    await flush();
    const input = list.host.querySelector<HTMLInputElement>(".resource-rename-input")!;
    expect(input.value).toBe(node.name);
    expect(input.closest("button")).toBeNull();
    input.dispatchEvent(new Event("pointerdown", { bubbles: true }));
    input.click();
    submit("renamed.md");
    await flush();
    expect(rename.name).toBe("renamed.md");
    expect(submitRename).toHaveBeenCalledTimes(1);
    expect(selected).not.toHaveBeenCalled();
    expect(dragged).not.toHaveBeenCalled();
  });

  it("offers directory path and delete actions without directory rename", () => {
    const action = vi.fn();
    const menu = mount(ResourceFileMenuItems, { target: { ...file, path: "F:/Project/docs", isDirectory: true }, onAction: action });
    button(menu.host, t("knowledge.explorer.openInFileSystem")).click();
    expect(action).toHaveBeenLastCalledWith("reveal");
    button(menu.host, t("knowledge.explorer.copyRelativePath")).click();
    expect(action).toHaveBeenLastCalledWith("copy");
    expect([...menu.host.querySelectorAll("button")].some(item => item.textContent?.trim() === t("knowledge.explorer.rename"))).toBe(false);
    button(menu.host, t("knowledge.explorer.delete")).click();
    expect(action).toHaveBeenLastCalledWith("delete");
    const mounted = mount(ResourceFileMenuItems, { target: { ...file, path: "G:/shared/docs", isDirectory: true, mounted: true }, onAction: action });
    expect(mounted.host.querySelector("button.danger")).toBeNull();
  });
  it("changes only the rendered file label, preserving the file used by list actions", async () => {
    const selected = vi.fn();
    const nodes = ["design.md", "other.md"].map((name) => ({ kind: "file", path: `docs/${name}`, name, depth: 1 }));
    const list = mount(AssetDirectoryList, { workingDir: file.root, items: nodes, loading: false, loaded: true, hasMore: false, emptyLabel: "", selectedPath: null, onSelect: selected });
    useExplorerPathDisplay().togglePath(explorerFileKey(file.path));
    await flush();
    const rows = list.host.querySelectorAll<HTMLButtonElement>(".adl-row");
    expect(rows[0]!.textContent).toContain(file.path);
    expect(rows[1]!.textContent).not.toContain("F:/Project");
    rows[0]!.click();
    expect(selected).toHaveBeenCalledWith(nodes[0]);
    expect(nodes[0]!.name).toBe("design.md");
  });

  it("renames through the knowledge list menu even when that document displays its full path", async () => {
    const renamed = vi.fn();
    useExplorerPathDisplay().togglePath(explorerKnowledgeKey(file.root, document.id));
    const list = mount(KnowledgeExplorer, {
      workingDir: file.root, tree: [{ kind: "document", type: "design", path: "design/combat/design.md", name: "design.md", depth: 0, document }],
      activeType: "design", rootDirectoryConfigs: {}, externalDirectorySources: {}, folderStats: {}, selectedPath: null,
      isPathExpanded: () => true, rootContentsLoaded: () => true, hasMoreRootDocuments: () => false, rootDocumentsLoading: () => false,
      hasMoreFolderDocuments: () => false, folderDocumentsLoaded: () => true, folderDocumentsLoading: () => false,
      loading: false, searchQuery: "", searchResults: [], searching: false, onRenameDocument: renamed,
    });
    await flush();
    const row = list.host.querySelector<HTMLElement>(".workspace-tree-row-shell")!;
    expect(row.textContent).toContain("design/combat/design.md");
    row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    await flush();
    button(window.document.body, t("knowledge.explorer.rename")).click();
    await flush();
    const input = list.host.querySelector<HTMLInputElement>(".kx-rename-input")!;
    expect(input.value).toBe("design.md");
    input.value = "renamed.md";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await flush();
    expect(renamed).toHaveBeenCalledWith("combat/design.md", "renamed.md", "design");
  });

  it("offers the same actions for ordinary files and knowledge documents", () => {
    const fileMenu = mount(ResourceFileMenuItems, { target: file });
    const knowledgeMenu = mount(ResourceFileMenuItems, { target: knowledge });
    const labels = (host: Element) => [...host.querySelectorAll("button")].map((item) => item.textContent?.trim());
    expect(labels(fileMenu.host)).toEqual(labels(knowledgeMenu.host));
    for (const label of ["knowledge.explorer.rename", "knowledge.explorer.copyRelativePath", "knowledge.explorer.openInFileSystem", "knowledge.explorer.delete"])
      expect(button(fileMenu.host, t(label)).disabled).toBe(false);
  });

  it("toggles only the selected document and persists across list instances", async () => {
    const menu = mount(ResourceFileMenuItems, { target: knowledge });
    button(menu.host, t("development.showFullPath")).click();
    await flush();
    const { showsFullPath } = useExplorerPathDisplay();
    expect(showsFullPath(explorerKnowledgeKey(file.root, document.id))).toBe(true);
    expect(showsFullPath(explorerKnowledgeKey(file.root, "doc-b"))).toBe(false);
    expect(showsFullPath(explorerFileKey(file.path))).toBe(false);
    window.dispatchEvent(new StorageEvent("storage", { key: "locus:explorerFullPaths" }));
    const secondMenu = mount(ResourceFileMenuItems, { target: knowledge });
    button(secondMenu.host, t("development.showFileName")).click();
    await flush();
    expect(showsFullPath(explorerKnowledgeKey(file.root, document.id))).toBe(false);
  });

  it.each(["plugin://sample", "external://sample"])("retains managed-source restrictions for %s", (locator) => {
    const target = { ...knowledge, document: { ...document, externalSource: { provider: "package", locator } } };
    const menu = mount(ResourceFileMenuItems, { target });
    expect(button(menu.host, t("knowledge.explorer.rename")).disabled).toBe(true);
    expect(button(menu.host, t("knowledge.explorer.delete")).disabled).toBe(true);
    expect(button(menu.host, t("knowledge.explorer.copyRelativePath")).disabled).toBe(false);
  });

  it("renames in the captured checkout, retaining path display preference", async () => {
    const changed = vi.fn();
    useExplorerPathDisplay().togglePath(explorerFileKey(file.path));
    const actions = mount(ExplorerResourceActions, { onChanged: changed });
    await actions.instance.run("rename", file);
    const input = actions.host.querySelector<HTMLInputElement>(".workspace-tree-row .resource-rename-input")!;
    expect(input.value).toBe("design.md");
    expect(window.document.activeElement).toBe(input);
    expect(input.closest("button")).toBeNull();
    expect(window.document.querySelector("[role=dialog], .resource-rename-backdrop")).toBeNull();
    expect(input.selectionEnd).toBe("design".length);
    submit("renamed.md");
    await flush();
    expect(mocks.fileAction).toHaveBeenCalledWith("project-a", "docs/design.md", "renamed.md", workspaceRef);
    expect(changed).toHaveBeenCalledWith(file, "F:/Project/docs/renamed.md");
    expect(useExplorerPathDisplay().showsFullPath(explorerFileKey("F:/Project/docs/renamed.md"))).toBe(true);
    expect(useExplorerPathDisplay().showsFullPath(explorerFileKey(file.path))).toBe(false);
    expect(window.document.querySelector("[role=dialog]")).toBeNull();
  });

  it("keeps knowledge identity and parent directory when renaming", async () => {
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("rename", knowledge);
    submit("renamed.md");
    await flush();
    expect(mocks.knowledgeMove).toHaveBeenCalledWith({ kind: "document", type: "design", path: "combat/design.md", newPath: "combat/renamed.md" }, workspaceRef);
    expect(mocks.fileAction).not.toHaveBeenCalled();
    expect(mocks.refresh).toHaveBeenCalledWith(file.projectId);
  });

  it("cancels with Escape without submitting on blur and restores row focus", async () => {
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("rename", file);
    const input = actions.host.querySelector<HTMLInputElement>("input")!;
    input.value = "cancelled.md";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    input.dispatchEvent(new Event("blur"));
    await flush();
    expect(mocks.fileAction).not.toHaveBeenCalled();
    expect(actions.host.querySelector("input")).toBeNull();
    expect(window.document.activeElement).toBe(actions.host.querySelector(".workspace-tree-row"));
  });

  it("submits on blur and ignores duplicate submissions while saving", async () => {
    let resolve!: (path: string) => void;
    mocks.fileAction.mockReturnValueOnce(new Promise<string>(done => { resolve = done; }));
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("rename", file);
    const input = actions.host.querySelector<HTMLInputElement>("input")!;
    input.value = "renamed.md";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.blur();
    await flush();
    expect(input.readOnly).toBe(true);
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    input.dispatchEvent(new Event("blur"));
    expect(mocks.fileAction).toHaveBeenCalledTimes(1);
    resolve("F:/Project/docs/renamed.md");
    await flush();
    expect(actions.host.querySelector("input")).toBeNull();
  });

  it("keeps IME confirmation inside the editor and accepts the subsequent Enter", async () => {
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("rename", file);
    const input = actions.host.querySelector<HTMLInputElement>("input")!;
    input.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    submit("事件大纲.md");
    await flush();
    expect(mocks.fileAction).not.toHaveBeenCalled();
    input.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", isComposing: true, bubbles: true }));
    expect(mocks.fileAction).not.toHaveBeenCalled();
    submit("事件大纲.md");
    await flush();
    expect(mocks.fileAction).toHaveBeenCalledWith(file.projectId, "docs/design.md", "事件大纲.md", workspaceRef);
  });

  it("forces mounted lists to refresh and closes a successful rename even if refresh fails", async () => {
    mocks.snapshots["project-a"] = { nodes: [{ nodeId: "mount", nodeKind: "folder", sourcePath: "F:/Project/docs" }] };
    mocks.loadMount.mockRejectedValueOnce(new Error("Listing unavailable"));
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("rename", file);
    submit("renamed.md");
    await flush();
    expect(mocks.loadMount).toHaveBeenCalledWith("project-a", "mount", true);
    expect(mocks.fileAction).toHaveBeenCalledTimes(1);
    expect(window.document.querySelector("[role=dialog]")).toBeNull();
    expect(mocks.notice).toHaveBeenCalledWith("warning", "Listing unavailable");
  });

  it("rejects path traversal and keeps a failed rename available for correction", async () => {
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("rename", file);
    submit("../oops.md");
    await flush();
    expect(mocks.fileAction).not.toHaveBeenCalled();
    expect(window.document.querySelector("[role=alert]")?.textContent).toContain(t("development.invalidFileName"));
    mocks.fileAction.mockRejectedValueOnce(new Error("Target already exists"));
    submit("duplicate.md");
    await flush();
    expect(window.document.querySelector("[role=alert]")?.textContent).toContain("Target already exists");
    expect(window.document.querySelector<HTMLInputElement>(".resource-rename-input")?.value).toBe("duplicate.md");
  });

  it("does not delete when confirmation is cancelled", async () => {
    mocks.confirm.mockResolvedValue(false);
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("delete", file);
    expect(mocks.fileAction).not.toHaveBeenCalled();
    expect(mocks.knowledgeDelete).not.toHaveBeenCalled();
  });

  it.each([
    ["G:/story-notes", "G:/story-notes/.png.md", ".png.md"],
    ["G:\\story-notes\\", "g:/story-notes/人物/主角.md", "人物/主角.md"],
    ["F:/worktrees/story", "F:/worktrees/story/docs/design.md", "docs/design.md"],
    ["F:/Project", "docs/design.md", "docs/design.md"],
  ])("deletes workspace documents relative to %s", async (root, path, relativePath) => {
    mocks.confirm.mockResolvedValue(true);
    mocks.fileAction.mockResolvedValueOnce(null);
    const changed = vi.fn();
    const target = { ...file, root, path };
    const actions = mount(ExplorerResourceActions, { onChanged: changed });

    await actions.instance.run("delete", target);

    expect(mocks.fileAction).toHaveBeenCalledExactlyOnceWith(file.projectId, relativePath, null, workspaceRef);
    expect(changed).toHaveBeenCalledWith(target, null);
    expect(target.path).toBe(path);
    expect(mocks.load).toHaveBeenCalledWith(file.projectId, true);
    expect(mocks.knowledgeDelete).not.toHaveBeenCalled();
    expect(mocks.notice).not.toHaveBeenCalled();
  });

  it("deletes workspace folders after a folder confirmation", async () => {
    mocks.confirm.mockResolvedValue(true);
    mocks.fileAction.mockResolvedValueOnce(null);
    const actions = mount(ExplorerResourceActions);

    await actions.instance.run("delete", { ...file, path: "F:/Project/docs", isDirectory: true });

    expect(mocks.confirm.mock.calls[0]![0]).toBe(t("knowledge.explorer.deleteFolderConfirm", "docs"));
    expect(mocks.fileAction).toHaveBeenCalledExactlyOnceWith(file.projectId, "docs", null, workspaceRef);
  });

  it("deletes a selected folder once together with entries selected inside it", async () => {
    mocks.confirm.mockResolvedValue(true);
    mocks.fileAction.mockResolvedValue(null);
    const actions = mount(ExplorerResourceActions);

    await actions.instance.runMany("delete", [
      { ...file, path: "F:/Project/docs", isDirectory: true },
      file,
      { ...file, path: "F:/Project/docs/sub", isDirectory: true },
      { ...file, path: "F:/Project/docs-old/readme.md" },
    ]);

    expect(mocks.confirm.mock.calls[0]![0]).toBe(t("knowledge.explorer.deleteManyConfirm", 2));
    expect(mocks.fileAction.mock.calls.map(call => call[1])).toEqual(["docs", "docs-old/readme.md"]);
    expect(mocks.notice).not.toHaveBeenCalled();
  });

  it.each([
    { ...file, path: "G:/shared/design.md", mounted: true },
    { ...file, mounted: true },
    { ...file, workspaceRef: null },
  ])("preserves absolute paths for project-authorized deletion: $path", async (target) => {
    mocks.confirm.mockResolvedValue(true);
    mocks.fileAction.mockResolvedValueOnce(null);
    const actions = mount(ExplorerResourceActions);

    await actions.instance.run("delete", target);

    expect(mocks.fileAction).toHaveBeenCalledExactlyOnceWith(file.projectId, target.path, null, null);
    expect(mocks.notice).not.toHaveBeenCalled();
  });

  it("copies scoped paths and preserves external mount paths", async () => {
    const actions = mount(ExplorerResourceActions);
    await actions.instance.run("copy", knowledge);
    expect(mocks.copy).toHaveBeenCalledWith("design/combat/design.md");
    expect(resourceRelativePath(file)).toBe("docs/design.md");
    expect(resourceRelativePath({ ...file, path: "F:/Project-other/design.md" })).toBe("F:/Project-other/design.md");
  });

  it("requests workbench history without mounting a floating dialog", async () => {
    const history = vi.fn();
    const actions = mount(ExplorerResourceActions, { onHistory: history });
    await actions.instance.run("history", file);
    expect(history).toHaveBeenLastCalledWith({ projectId: file.projectId, path: "docs/design.md", workspaceRef });
    await actions.instance.run("history", { ...file, mounted: true, path: "G:/Shared/note.md" });
    expect(history).toHaveBeenLastCalledWith({ projectId: file.projectId, path: "G:/Shared/note.md", workspaceRef: null });
    expect(window.document.querySelector("dialog")).toBeNull();
    expect(mocks.fileAction).not.toHaveBeenCalled();
  });
});
