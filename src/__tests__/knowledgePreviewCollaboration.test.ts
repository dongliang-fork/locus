// @vitest-environment jsdom
import { createPinia } from "pinia";
import { createApp, defineComponent, h, nextTick, ref } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import KnowledgePreview from "../components/knowledge/KnowledgePreview.vue";
import { KnowledgeEditorWorkspaceSessionStore } from "../components/knowledge/knowledgeEditorWorkspaceSession";
import { useMarkdownEditorViewMode } from "../components/ui/markdownEditorViewMode";
import type { KnowledgeDocument, KnowledgeDocumentEditOperation } from "../types";

vi.mock("../components/ui/BaseMarkdownEditor.vue", async () => {
  const { defineComponent, h } = await import("vue");
  const { Text } = await import("@codemirror/state");
  return {
    default: defineComponent({
      props: {
        modelValue: { type: String, default: "" },
        disabled: Boolean,
        transactionModel: Boolean,
      },
      emits: ["update:modelValue", "documentChange", "shortcutSave"],
      setup(props, { emit }) {
        return () => h("div", { class: "base-markdown-editor" }, [
          h("textarea", {
            class: "base-markdown-editor-textarea",
            value: props.modelValue,
            disabled: props.disabled,
            onInput: (event: Event) => {
              const value = (event.target as HTMLTextAreaElement).value;
              if (props.transactionModel) {
                emit("documentChange", {
                  doc: Text.of(value.replace(/\r\n/g, "\n").split("\n")),
                  changes: null,
                });
                return;
              }
              emit("update:modelValue", value);
            },
          }),
        ]);
      },
    }),
  };
});

function makeDocument(
  body: string,
  modifiedAt = 1,
  id = "design-1",
  path = "combat/core-loop.md",
): KnowledgeDocument {
  return {
    id,
    type: "design",
    path,
    title: "核心循环",
    injectMode: "excerpt",
    effectiveInjectMode: "excerpt",
    readOnly: false,
    aiMaintained: false,
    effectiveAiMaintained: false,
    summary: "summary",
    body,
    maintenanceRules: null,
    effectiveMaintenanceRules: null,
    modifiedAt,
  };
}

async function mountPreview(saveEdits = vi.fn()) {
  useMarkdownEditorViewMode().setMarkdownEditorViewMode("native");
  const document = ref(makeDocument("alpha\nbeta\ngamma"));
  const workspaceRef = ref({ checkoutId: "checkout-a", expectedGeneration: 1 });
  const visible = ref(true);
  const saveLoading = ref(false);
  const updateMeta = vi.fn();
  const sessionStore = new KnowledgeEditorWorkspaceSessionStore();
  const host = documentOwner().createElement("div");
  documentOwner().body.appendChild(host);
  const Root = defineComponent(() => () => visible.value
    ? h(KnowledgePreview, {
        document: document.value,
        loading: false,
        saveLoading: saveLoading.value,
        onUpdateMeta: updateMeta,
        embedded: true,
        workspaceRef: workspaceRef.value,
        sessionStore,
        saveEdits,
      })
    : h("div", { class: "alternate-preview" }));
  const app = createApp(Root);
  app.use(createPinia());
  app.mount(host);
  await nextTick();
  return { app, document, host, saveEdits, sessionStore, visible, workspaceRef, saveLoading, updateMeta };
}

function documentOwner(): Document {
  return window.document;
}

function bodyEditor(host: HTMLElement): HTMLTextAreaElement {
  const editor = host.querySelector<HTMLTextAreaElement>(
    ".document-body .base-markdown-editor-textarea",
  );
  if (!editor) throw new Error("body editor missing");
  return editor;
}

function input(editor: HTMLTextAreaElement | HTMLInputElement, value: string) {
  editor.value = value;
  editor.dispatchEvent(new Event("input", { bubbles: true }));
}

afterEach(() => {
  vi.useRealTimers();
  useMarkdownEditorViewMode().setMarkdownEditorViewMode("rendered");
  documentOwner().body.innerHTML = "";
});

describe("KnowledgePreview collaborative editing", () => {
  it("commits a blurred title once an in-flight body autosave finishes", async () => {
    vi.useFakeTimers();
    let resolveSave!: (document: KnowledgeDocument) => void;
    const mounted = await mountPreview(vi.fn(() => new Promise<KnowledgeDocument>((resolve) => { resolveSave = resolve; })));
    input(bodyEditor(mounted.host), "saved body");
    vi.advanceTimersByTime(800);
    await nextTick();
    const title = mounted.host.querySelector<HTMLInputElement>(".document-title-input")!;
    input(title, "New title");
    title.dispatchEvent(new Event("blur"));
    await nextTick();
    expect(mounted.updateMeta).not.toHaveBeenCalled();
    resolveSave(makeDocument("saved body", 2));
    for (let i = 0; i < 10; i++) await nextTick();
    expect(mounted.updateMeta).toHaveBeenCalledExactlyOnceWith({ newPath: "combat/New title.md" });
    expect(bodyEditor(mounted.host).value).toBe("saved body");
    mounted.app.unmount();
  });

  it("keeps document controls and layout unchanged while a body autosave is in flight", async () => {
    vi.useFakeTimers();
    let resolveSave!: (document: KnowledgeDocument) => void;
    let mounted!: Awaited<ReturnType<typeof mountPreview>>;
    mounted = await mountPreview(vi.fn(() => {
      mounted.saveLoading.value = true;
      return new Promise<KnowledgeDocument>((resolve) => { resolveSave = resolve; });
    }));
    const title = () => mounted.host.querySelector<HTMLInputElement>(".document-title-input")!;
    const addProperty = () => mounted.host.querySelector(".property-add-trigger");
    expect(addProperty()).not.toBeNull();

    input(bodyEditor(mounted.host), "saved body");
    vi.advanceTimersByTime(800);
    await nextTick();
    expect(mounted.saveEdits).toHaveBeenCalledTimes(1);
    expect(title().disabled).toBe(false);
    expect(addProperty()).not.toBeNull();
    expect(mounted.host.querySelector(".editor-footnote")).toBeNull();

    mounted.saveLoading.value = false;
    resolveSave(makeDocument("saved body", 2));
    for (let i = 0; i < 10; i++) await nextTick();
    expect(title().disabled).toBe(false);

    // Other metadata saves leave the title alone; only its own rename makes it
    // read-only, which keeps focus and appearance unchanged.
    mounted.saveLoading.value = true;
    await nextTick();
    expect(title().disabled).toBe(false);
    expect(title().readOnly).toBe(false);
    mounted.saveLoading.value = false;
    await nextTick();
    input(title(), "Renamed");
    title().dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await nextTick();
    expect(mounted.updateMeta).toHaveBeenCalledExactlyOnceWith({ newPath: "combat/Renamed.md" });
    mounted.saveLoading.value = true;
    await nextTick();
    expect(title().readOnly).toBe(true);
    expect(title().disabled).toBe(false);
    mounted.saveLoading.value = false;
    await nextTick();
    expect(title().readOnly).toBe(false);
    mounted.app.unmount();
  });

  it("keeps the property area editable and ordered while metadata saves are in flight", async () => {
    const mounted = await mountPreview();
    const rows = () => mounted.host.querySelectorAll(".property-row").length;
    const addProperty = async (name: string) => {
      mounted.host.querySelector<HTMLButtonElement>(".property-add-trigger")!.click();
      await nextTick();
      input(mounted.host.querySelector<HTMLInputElement>(".property-add input")!, name);
      mounted.host.querySelector(".property-add")!
        .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
      await nextTick();
    };

    await addProperty("owner");
    const first = mounted.updateMeta.mock.calls[0]![0];
    expect(first).toMatchObject({ expectedPropertiesYaml: "" });
    expect(first.propertiesYaml).toContain("owner");
    mounted.saveLoading.value = true;
    await nextTick();
    expect(rows()).toBe(1);
    expect(mounted.host.querySelector(".property-add-trigger")).not.toBeNull();

    // The second edit builds on the first and expects it on disk.
    await addProperty("status");
    const second = mounted.updateMeta.mock.calls[1]![0];
    expect(second.expectedPropertiesYaml).toBe(first.propertiesYaml);
    expect(second.propertiesYaml).toContain("owner");
    expect(second.propertiesYaml).toContain("status");
    expect(rows()).toBe(2);

    mounted.document.value = { ...makeDocument("alpha\nbeta\ngamma", 2), propertiesYaml: second.propertiesYaml };
    mounted.saveLoading.value = false;
    await nextTick();
    expect(rows()).toBe(2);
    mounted.app.unmount();
  });

  it("cancels a queued title with Escape and ignores IME confirmation", async () => {
    const mounted = await mountPreview();
    const title = mounted.host.querySelector<HTMLInputElement>(".document-title-input")!;
    input(title, "输入中");
    title.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", isComposing: true, bubbles: true }));
    await nextTick();
    expect(mounted.updateMeta).not.toHaveBeenCalled();
    mounted.saveLoading.value = true;
    await nextTick();
    title.dispatchEvent(new Event("blur"));
    title.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    mounted.saveLoading.value = false;
    await nextTick();
    expect(mounted.updateMeta).not.toHaveBeenCalled();
    expect(title.value).toBe("core-loop");
    mounted.app.unmount();
  });

  it("restores a dirty document session after the preview is unmounted", async () => {
    const mounted = await mountPreview();
    input(bodyEditor(mounted.host), "alpha\nbeta-local\ngamma");

    mounted.visible.value = false;
    await nextTick();
    expect(mounted.host.querySelector(".alternate-preview")).not.toBeNull();
    mounted.visible.value = true;
    await nextTick();

    expect(bodyEditor(mounted.host).value).toBe("alpha\nbeta-local\ngamma");
    mounted.app.unmount();
  });

  it("isolates the same document id across checkout generations", async () => {
    const mounted = await mountPreview();
    input(bodyEditor(mounted.host), "generation one local draft");

    mounted.workspaceRef.value = { checkoutId: "checkout-a", expectedGeneration: 2 };
    mounted.document.value = makeDocument("generation two remote");
    await nextTick();
    expect(bodyEditor(mounted.host).value).toBe("generation two remote");

    mounted.workspaceRef.value = { checkoutId: "checkout-a", expectedGeneration: 1 };
    mounted.document.value = makeDocument("alpha\nbeta\ngamma");
    await nextTick();
    expect(bodyEditor(mounted.host).value).toBe("generation one local draft");
    mounted.app.unmount();
  });

  it("restores a bounded document session when navigating A to B to A", async () => {
    const mounted = await mountPreview();
    input(bodyEditor(mounted.host), "alpha\nbeta-local\ngamma");
    mounted.document.value = makeDocument(
      "bravo",
      1,
      "design-2",
      "combat/enemies.md",
    );
    await nextTick();
    expect(bodyEditor(mounted.host).value).toBe("bravo");

    mounted.document.value = makeDocument("alpha\nbeta\ngamma", 1);
    await nextTick();
    expect(bodyEditor(mounted.host).value).toBe("alpha\nbeta-local\ngamma");
    mounted.app.unmount();
  });

  it("routes a delayed save response back to its initiating document", async () => {
    vi.useFakeTimers();
    let resolveSave!: (document: KnowledgeDocument) => void;
    const saveEdits = vi.fn(() => new Promise<KnowledgeDocument>((resolve) => {
      resolveSave = resolve;
    }));
    const mounted = await mountPreview(saveEdits);
    input(bodyEditor(mounted.host), "alpha\nbeta-saved\ngamma");
    vi.advanceTimersByTime(800);
    await nextTick();
    expect(saveEdits).toHaveBeenCalledTimes(1);

    mounted.document.value = makeDocument(
      "bravo-current",
      1,
      "design-2",
      "combat/enemies.md",
    );
    await nextTick();
    resolveSave(makeDocument("alpha\nbeta-saved\ngamma", 2));
    await Promise.resolve();
    await nextTick();
    expect(bodyEditor(mounted.host).value).toBe("bravo-current");

    mounted.document.value = makeDocument("alpha\nbeta-saved\ngamma", 2);
    await nextTick();
    expect(bodyEditor(mounted.host).value).toBe("alpha\nbeta-saved\ngamma");
    mounted.app.unmount();
  });

  it("retains an unresolved conflict when navigating A to B to A", async () => {
    const mounted = await mountPreview();
    input(bodyEditor(mounted.host), "alpha\nbeta-local\ngamma");
    mounted.document.value = makeDocument("alpha\nbeta-agent\ngamma", 2);
    await nextTick();
    expect(mounted.host.querySelector(".document-conflict")).not.toBeNull();

    mounted.document.value = makeDocument(
      "bravo",
      1,
      "design-2",
      "combat/enemies.md",
    );
    await nextTick();
    mounted.document.value = makeDocument("alpha\nbeta-agent\ngamma", 2);
    await nextTick();

    expect(bodyEditor(mounted.host).value).toBe("alpha\nbeta-local\ngamma");
    expect(mounted.host.querySelector(".document-conflict")).not.toBeNull();
    mounted.app.unmount();
  });

  it("applies a non-overlapping Agent edit into the active local draft", async () => {
    const mounted = await mountPreview();
    const editor = bodyEditor(mounted.host);
    input(editor, "alpha\nbeta-local\ngamma");
    mounted.document.value = makeDocument("alpha\nbeta\ngamma-agent", 2);
    await nextTick();
    await nextTick();

    expect(bodyEditor(mounted.host).value).toBe("alpha\nbeta-local\ngamma-agent");
    expect(mounted.host.querySelector(".document-conflict")).toBeNull();
    mounted.app.unmount();
  });

  it("pauses autosave for an overlapping edit and resumes after keeping local", async () => {
    vi.useFakeTimers();
    const saveEdits = vi.fn(async (edits: KnowledgeDocumentEditOperation[]) => {
      expect(edits.length).toBeGreaterThan(0);
      return makeDocument("alpha\nbeta-local\ngamma", 3);
    });
    const mounted = await mountPreview(saveEdits);
    input(bodyEditor(mounted.host), "alpha\nbeta-local\ngamma");
    mounted.document.value = makeDocument("alpha\nbeta-agent\ngamma", 2);
    await nextTick();
    await nextTick();

    vi.advanceTimersByTime(800);
    await nextTick();
    expect(saveEdits).not.toHaveBeenCalled();
    expect(mounted.host.querySelector(".document-conflict")).not.toBeNull();

    const buttons = mounted.host.querySelectorAll<HTMLButtonElement>(
      ".document-conflict-actions button",
    );
    buttons[1]!.click();
    await nextTick();
    vi.advanceTimersByTime(800);
    await Promise.resolve();
    await nextTick();
    expect(saveEdits).toHaveBeenCalledTimes(1);
    mounted.app.unmount();
  });
});
