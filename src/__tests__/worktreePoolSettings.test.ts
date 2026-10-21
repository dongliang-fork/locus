// @vitest-environment jsdom
import { createApp, defineComponent, h, nextTick, reactive, type App } from "vue";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import WorktreePoolSettings from "../components/settings/WorktreePoolSettings.vue";

const mocks = vi.hoisted(() => ({ usage: vi.fn(), rootSettings: vi.fn(), setRoot: vi.fn(), open: vi.fn() }));
const workspace = reactive({ focusedRoot: "", projects: [{ projectId: "one", checkouts: [] as Array<{ root: string }> }, { projectId: "two", checkouts: [] as Array<{ root: string }> }] });
vi.mock("../stores/workspaceContext", () => ({ useWorkspaceContextStore: () => workspace }));
vi.mock("../services/worktrees", () => ({
  getAllWorktreePoolUsage: mocks.usage, getWorktreeRootSettings: mocks.rootSettings, setWorktreeRootSetting: mocks.setRoot,
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: mocks.open }));
vi.mock("../i18n", () => ({ t: (key: string, ...args: unknown[]) => [key, ...args].join(" ") }));
vi.mock("../components/workbench/WorktreeManager.vue", () => ({ default: defineComponent({
  props: ["sourceRoot"], emits: ["close"], setup: (props, { emit }) => () => h("button", { class: "manager-stub", onClick: () => emit("close") }, props.sourceRoot),
}) }));
const projectUsage = [
  { projectId: "one", sourceRoot: "F:/project", usage: { items: [{}, {}], availableProjects: 1, totalBytes: 4096 }, error: null },
  { projectId: "two", sourceRoot: "F:/other", usage: { items: [{}], availableProjects: 0, totalBytes: 2048 }, error: null },
];
let app: App;
let host: HTMLDivElement;
async function settle() { for (let i = 0; i < 12; i++) await Promise.resolve(); await nextTick(); }
beforeEach(() => {
  vi.resetAllMocks(); workspace.focusedRoot = ""; workspace.projects = [{ projectId: "one", checkouts: [] as Array<{ root: string }> }, { projectId: "two", checkouts: [] as Array<{ root: string }> }];
  mocks.usage.mockResolvedValue(projectUsage);
  mocks.rootSettings.mockResolvedValue({ settingsRoot: null, processRoot: null });
  host = document.createElement("div"); document.body.append(host);
});
afterEach(() => { app?.unmount(); host.remove(); });
async function mount() { app = createApp(WorktreePoolSettings); app.mount(host); await settle(); }
function button(label: string) {
  const found = Array.from(host.querySelectorAll<HTMLButtonElement>("button")).find(item => item.textContent?.trim() === label);
  if (!found) throw new Error(`Missing button ${label}`);
  return found;
}

describe("worktree pool settings", () => {
  it("aggregates all added projects even without a focused workspace", async () => {
    await mount(); expect(host.textContent).toContain("worktrees.pool.summary 3 1 6.00 KB");
    expect(host.querySelectorAll("tbody tr")).toHaveLength(2);
    expect(host.textContent).toContain("F:/project"); expect(host.textContent).toContain("F:/other");
    expect(mocks.usage).toHaveBeenCalledExactlyOnceWith();
  });
  it("opens the selected project's manager and refreshes usage on close", async () => {
    await mount(); host.querySelectorAll<HTMLButtonElement>("tbody button")[1]!.click(); await settle();
    expect(document.querySelector(".manager-stub")?.textContent).toBe("F:/other");
    expect(workspace.focusedRoot).toBe("");
    document.querySelector<HTMLButtonElement>(".manager-stub")!.click(); await settle();
    expect(mocks.usage).toHaveBeenCalledTimes(2);
  });
  it("shows an empty catalog without creating or activating a workspace", async () => {
    workspace.projects = []; mocks.usage.mockResolvedValue([]); await mount();
    expect(host.textContent).toContain("worktrees.pool.noProjects");
    expect(host.querySelectorAll("tbody button")).toHaveLength(0);
    expect(host.querySelector<HTMLButtonElement>(".pool-toolbar button")!.disabled).toBe(false);
  });
  it("keeps readable projects visible when another project is unavailable", async () => {
    mocks.usage.mockResolvedValue([projectUsage[0], { projectId: "two", sourceRoot: "F:/missing", usage: null, error: "Directory unavailable" }]);
    await mount(); expect(host.textContent).toContain("worktrees.pool.summary 2 1 4.00 KB");
    expect(host.textContent).toContain("worktrees.pool.partial");
    expect(host.querySelector('[role="alert"]')?.textContent).toBe("Directory unavailable");
    expect(host.querySelectorAll<HTMLButtonElement>("tbody button")[1]!.disabled).toBe(true);
  });
  it("persists a chosen worktree location and restores the default", async () => {
    await mount();
    expect(host.querySelector(".pool-location-path")?.textContent).toBe("worktrees.location.repository");
    mocks.open.mockResolvedValueOnce("E:/Worktrees");
    mocks.setRoot.mockResolvedValueOnce({ settingsRoot: "E:\\Worktrees", processRoot: null });
    button("worktrees.location.change").click(); await settle();
    expect(mocks.setRoot).toHaveBeenCalledExactlyOnceWith("E:/Worktrees");
    expect(host.querySelector(".pool-location-path")?.textContent).toBe("E:\\Worktrees");
    mocks.setRoot.mockResolvedValueOnce({ settingsRoot: null, processRoot: null });
    button("worktrees.location.reset").click(); await settle();
    expect(mocks.setRoot).toHaveBeenLastCalledWith(null);
    expect(host.querySelector(".pool-location-path")?.textContent).toBe("worktrees.location.repository");
  });
  it("keeps the setting unchanged when the picker is dismissed or rejected", async () => {
    await mount();
    mocks.open.mockResolvedValueOnce(null);
    button("worktrees.location.change").click(); await settle();
    expect(mocks.setRoot).not.toHaveBeenCalled();
    mocks.open.mockResolvedValueOnce("E:/Missing");
    mocks.setRoot.mockRejectedValueOnce(new Error("Worktree directory E:/Missing is not an existing directory"));
    button("worktrees.location.change").click(); await settle();
    expect(host.querySelector(".pool-location [role=\"alert\"]")?.textContent).toContain("E:/Missing");
    expect(host.querySelector(".pool-location-path")?.textContent).toBe("worktrees.location.repository");
  });
  it("shows a process override as read-only", async () => {
    mocks.rootSettings.mockResolvedValue({ settingsRoot: "E:/Saved", processRoot: "T:/Run/worktrees" });
    await mount();
    expect(host.querySelector(".pool-location-path")?.textContent).toBe("T:/Run/worktrees");
    expect(button("worktrees.location.change").disabled).toBe(true);
    expect(host.textContent).not.toContain("worktrees.location.reset");
    expect(host.textContent).toContain("worktrees.location.process");
  });
  it("discards stale results after the project catalog changes and ignores focus changes", async () => {
    let resolve!: (value: unknown) => void;
    mocks.usage.mockReturnValueOnce(new Promise(value => { resolve = value; }));
    await mount(); workspace.projects.push({ projectId: "three", checkouts: [] }); await settle();
    resolve([]); await settle();
    expect(host.textContent).toContain("worktrees.pool.summary 3 1 6.00 KB");
    workspace.focusedRoot = "F:/other"; await settle();
    expect(mocks.usage).toHaveBeenCalledTimes(2);
  });
});
