// @vitest-environment jsdom
import { createApp, nextTick, reactive, h } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import AskUserCard from "../components/chat/AskUserCard.vue";
import type { PendingQuestion } from "../types";
import { clearQuestionDraft, questionDraft } from "../composables/questionnaire";

function form(id = "card"): PendingQuestion {
  return { questionId: id, toolCallId: "ask", question: "Targets?", options: [], questions: [
    { id: "q1", question: "Targets?", header: "Targets", options: [{ label: "A", description: "First" }, { label: "B", description: "Second" }], multiSelect: true, allowCustom: true },
    { id: "q2", question: "Mode?", header: "Mode", options: [{ label: "X", description: "" }, { label: "Y", description: "" }], multiSelect: false, allowCustom: false },
  ] };
}
const cleanup: (() => void)[] = [];
afterEach(() => { cleanup.splice(0).forEach((fn) => fn()); for (const id of ["card", "second", "legacy"]) clearQuestionDraft(id); });
function mount(question = form()) {
  const state = reactive({ question }); const onAnswer = vi.fn(); const host = document.createElement("div");
  document.body.append(host);
  const app = createApp({ setup: () => () => h(AskUserCard, { question: state.question, onAnswer }) });
  app.mount(host); cleanup.push(() => { app.unmount(); host.remove(); });
  return { state, host, onAnswer };
}
async function click(host: HTMLElement, selector: string) { host.querySelector<HTMLElement>(selector)!.click(); await nextTick(); }

describe("AskUserCard unified form", () => {
  it("collects multi-select and single-select questions, permits review/edit, and submits once", async () => {
    const { host, onAnswer } = mount();
    await click(host, 'input[value="A"]'); await click(host, 'input[value="B"]');
    expect(onAnswer).not.toHaveBeenCalled();
    await click(host, 'input[value="__custom__"]');
    const custom = host.querySelector<HTMLTextAreaElement>("textarea")!;
    custom.value = "C"; custom.dispatchEvent(new Event("input", { bubbles: true })); await nextTick();
    await click(host, ".ask-actions button:last-child");
    expect(host.textContent).toContain("Mode?");
    expect(host.querySelector('input[type="checkbox"]')).toBeNull();
    await click(host, 'input[value="X"]'); await click(host, 'input[value="Y"]');
    expect(onAnswer).not.toHaveBeenCalled();
    await click(host, ".ask-actions button:last-child");
    expect(host.querySelector(".ask-review")).not.toBeNull();
    expect(host.textContent).toContain("A, B, C");
    await click(host, '[data-edit-question="q1"]');
    expect(host.querySelector<HTMLInputElement>('input[value="A"]')!.checked).toBe(true);
    expect(host.querySelector<HTMLTextAreaElement>("textarea")!.value).toBe("C");
    await click(host, 'input[value="B"]');
    await click(host, '[data-question="q2"]');
    expect(host.querySelector<HTMLInputElement>('input[value="Y"]')!.checked).toBe(true);
    await click(host, ".ask-actions button:last-child");
    await click(host, 'button[type="submit"]');
    expect(onAnswer).toHaveBeenCalledExactlyOnceWith({ version: 1, answers: [
      { id: "q1", selected: ["A"], customText: "C" }, { id: "q2", selected: ["Y"], customText: "" },
    ] }, "card");
  });
  it("blocks partial submissions and locks the entire form while submitting", async () => {
    const pending = form(); const { host, onAnswer } = mount(pending);
    host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    expect(onAnswer).not.toHaveBeenCalled();
    expect(host.querySelector<HTMLButtonElement>(".ask-actions button")!.disabled).toBe(true);
    questionDraft(pending).submitting = true; await nextTick();
    expect(host.querySelector<HTMLFieldSetElement>("fieldset")!.disabled).toBe(true);
    expect([...host.querySelectorAll<HTMLButtonElement>("button")].every((button) => button.disabled)).toBe(true);
    questionDraft(pending).submitting = false; questionDraft(pending).error = "Retry the request"; await nextTick();
    expect(host.querySelector('[role="alert"]')!.textContent).toBe("Retry the request");
  });
  it("uses the same submit flow for legacy single questions and their custom option", async () => {
    const { host, onAnswer } = mount({ questionId: "legacy", toolCallId: "ask", question: "Pick", options: [{ label: "A", description: "First" }, { label: "Custom", description: "Type" }] });
    expect(host.querySelectorAll('input[type="radio"]')).toHaveLength(2);
    await click(host, 'input[value="A"]'); expect(onAnswer).not.toHaveBeenCalled();
    await click(host, 'button[type="submit"]');
    expect(onAnswer).toHaveBeenCalledWith({ version: 1, answers: [{ id: "q1", selected: ["A"], customText: "" }] }, "legacy");
  });
  it("restores drafts when changing groups and does not leak answers into another group", async () => {
    const { state, host } = mount();
    await click(host, 'input[value="B"]');
    state.question = form("second"); await nextTick();
    expect(host.querySelector<HTMLInputElement>('input[value="B"]')!.checked).toBe(false);
    state.question = form(); await nextTick();
    expect(host.querySelector<HTMLInputElement>('input[value="B"]')!.checked).toBe(true);
  });
});
