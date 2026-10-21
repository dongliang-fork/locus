// @vitest-environment jsdom
import { EditorState } from "@codemirror/state";
import { createApp, defineComponent, h, nextTick, provide } from "vue";
import { describe, expect, it } from "vitest";
import { captureMarkdownEditorSelection } from "../components/ui/markdown-editor/markdownEditorSelection";
import { knowledgeSelectionReference } from "../components/knowledge/knowledgeSelectionReference";
import { buildUserMessageDraft } from "../composables/chatMessageDraft";
import { userMessageKnowledgeQuoteEntries } from "../composables/chatUserMessageDisplay";
import {
  createDocumentSelectionHub,
  DOCUMENT_SELECTION_HUB_KEY,
  fileSelectionReference,
  liveSelectionQuoteForCheckout,
  selectionLocationLabel,
  useDocumentSelectionReporter,
  type DocumentSelectionHub,
  type DocumentSelectionInput,
  type DocumentSelectionSnapshot,
} from "../composables/documentSelection";

function select(doc: string, from: number, to: number) {
  return captureMarkdownEditorSelection(EditorState.create({ doc, selection: { anchor: from, head: to } }));
}

function snapshot(overrides: Partial<DocumentSelectionSnapshot> = {}): DocumentSelectionSnapshot {
  return {
    id: "owner:1",
    ownerId: "owner",
    checkoutId: "checkout-a",
    knowledgePath: "design/boss.md",
    filePath: "F:/Project/Locus/knowledge/design/boss.md",
    name: "boss",
    location: "boss.md:9",
    content: "F:/Project/Locus/knowledge/design/boss.md:9\n```markdown\nrepeat\n```",
    ...overrides,
  };
}

describe("document selection references", () => {
  it("cites physical file lines, unsaved drafts and fenced backticks", () => {
    const doc = "---\ntitle: a\n---\n# Hero\nfirst line\nuse ```js``` here\nlast";
    const from = doc.indexOf("first");
    const to = doc.indexOf("last");
    const content = fileSelectionReference("F:\\Project\\docs\\Hero.md", select(doc, from, to), { draftLabel: "draft" });
    expect(content).toBe("F:/Project/docs/Hero.md:5-6 (draft)\n````markdown\nfirst line\nuse ```js``` here\n\n````");
    expect(selectionLocationLabel("F:\\Project\\docs\\Hero.md", content)).toBe("Hero.md:5-6");
  });

  it("labels knowledge excerpts whose lines are offset by frontmatter", () => {
    const body = "# Boss\nrepeat\nother";
    const raw = "---\nid: boss\n---\n\n" + body;
    const source = { path: "F:\\Project\\Locus\\knowledge\\design\\boss.md", content: raw };
    const from = body.indexOf("repeat");
    const content = knowledgeSelectionReference(source, "body", body, select(body, from, from + 6), {
      draft: "draft", unavailable: "unavailable",
    });
    expect(selectionLocationLabel(source.path, content)).toBe("boss.md:6");
  });
});

describe("live selection quotes", () => {
  it("uses the knowledge path only for conversations in the owning checkout", () => {
    expect(liveSelectionQuoteForCheckout(snapshot(), "checkout-a")).toMatchObject({
      id: "owner:1", path: "design/boss.md", source: "knowledge", location: "boss.md:9",
    });
    expect(liveSelectionQuoteForCheckout(snapshot(), "checkout-b")).toMatchObject({
      path: "F:/Project/Locus/knowledge/design/boss.md", source: "file",
    });
    expect(liveSelectionQuoteForCheckout(snapshot({ knowledgePath: undefined, checkoutId: null }), null)).toMatchObject({
      path: "F:/Project/Locus/knowledge/design/boss.md", source: "file",
    });
  });

  it("restores sent file quotes without turning them into knowledge references", () => {
    const metadata = encodeURIComponent(JSON.stringify({ path: "F:/Project/docs/Hero.md", name: "Hero.md", source: "file" }));
    const content = `解释\n\n<locus-knowledge-quotes>\nintro\n\n<locus-knowledge-quote data="${metadata}">\nF:/Project/docs/Hero.md:5\n\`\`\`markdown\nfirst line\n\`\`\`\n</locus-knowledge-quote>\n</locus-knowledge-quotes>`;
    expect(userMessageKnowledgeQuoteEntries(content)).toEqual([{
      path: "F:/Project/docs/Hero.md",
      name: "Hero.md",
      content: "F:/Project/docs/Hero.md:5\n```markdown\nfirst line\n```",
      source: "file",
    }]);
    const draft = buildUserMessageDraft({ id: "m", role: "user", content, createdAt: 1 });
    expect(draft.text).toBe("解释");
    expect(draft.knowledgeQuotes).toEqual([{
      path: "F:/Project/docs/Hero.md",
      name: "Hero.md",
      content: "F:/Project/docs/Hero.md:5\n```markdown\nfirst line\n```",
      source: "file",
    }]);
  });
});

describe("document selection hub", () => {
  function mountReporter(hub: DocumentSelectionHub) {
    let reporter!: ReturnType<typeof useDocumentSelectionReporter>;
    const Child = defineComponent({
      setup() {
        reporter = useDocumentSelectionReporter("test");
        return () => null;
      },
    });
    const Root = defineComponent({
      setup() {
        provide(DOCUMENT_SELECTION_HUB_KEY, hub);
        return () => h(Child);
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    return { app, reporter };
  }

  const input = (content: string): DocumentSelectionInput => ({
    checkoutId: "checkout-a", filePath: "F:/a.md", name: "a.md", location: "a.md:1", content,
  });

  it("keeps the latest selection, drops stale async builds and clears only its own selection", async () => {
    const hub = createDocumentSelectionHub();
    const first = mountReporter(hub);
    const second = mountReporter(hub);

    let resolveSlow!: (value: DocumentSelectionInput) => void;
    const slow = first.reporter.update(() => new Promise((resolve) => { resolveSlow = resolve; }));
    await first.reporter.update(() => input("fast"));
    resolveSlow(input("slow"));
    await slow;
    expect(hub.current.value?.content).toBe("fast");

    await second.reporter.update(() => input("other"));
    const otherId = hub.current.value?.id;
    await first.reporter.update(null);
    expect(hub.current.value?.id).toBe(otherId);

    second.app.unmount();
    await nextTick();
    expect(hub.current.value).toBeNull();
    first.app.unmount();
  });

  it("treats failed or empty builds as no selection", async () => {
    const hub = createDocumentSelectionHub();
    const { app, reporter } = mountReporter(hub);
    await reporter.update(() => input("kept"));
    await reporter.update(() => { throw new Error("source changed"); });
    expect(hub.current.value).toBeNull();
    await reporter.update(() => input("   "));
    expect(hub.current.value).toBeNull();
    app.unmount();
  });
});
