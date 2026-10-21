import { afterEach, describe, expect, it, vi } from "vitest";
import type { PendingQuestion } from "../types";
import { questionItems, questionDraft, clearQuestionDraft, chooseQuestionOption, chooseCustomAnswer, questionAnswered, questionnaireAnswer } from "../composables/questionnaire";
import { submitQuestionForm, answerQuestion } from "../services/session";
import { buildToolCallArgsSummary } from "../components/toolCallSummary";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("../services/ipc", () => ({ ipcInvoke: mocks.invoke }));
function form(id = "group"): PendingQuestion {
  return { questionId: id, toolCallId: "ask", question: "Targets?", options: [], questions: [
    { id: "q1", header: "Targets", question: "Targets?", multiSelect: true, allowCustom: true, options: [{ label: "A", description: "" }, { label: "B", description: "" }] },
    { id: "q2", header: "Reason", question: "Why?", multiSelect: false, allowCustom: true, options: [] },
  ] };
}
afterEach(() => { for (const id of ["group", "other", "legacy"]) clearQuestionDraft(id); vi.clearAllMocks(); });

describe("unified questionnaire", () => {
  it("keeps every real grouped choice but removes the legacy custom sentinel", () => {
    expect(questionItems(form())[0].options.map((o) => o.label)).toEqual(["A", "B"]);
    const legacy: PendingQuestion = { questionId: "legacy", toolCallId: "ask", question: "Pick", options: [{ label: "A", description: "First" }, { label: "Custom", description: "Type" }] };
    expect(questionItems(legacy)[0]).toMatchObject({ id: "q1", options: [{ label: "A", description: "First" }], allowCustom: true, customLabel: "Custom" });
  });
  it("combines multiple choices with custom input and sends every answer once", () => {
    const pending = form(); const questions = questionItems(pending); const draft = questionDraft(pending);
    chooseQuestionOption(questions[0], draft.answers.q1, "B");
    chooseQuestionOption(questions[0], draft.answers.q1, "A");
    chooseCustomAnswer(questions[0], draft.answers.q1);
    expect(questionAnswered(questions[0], draft.answers.q1)).toBe(false);
    draft.answers.q1.customText = " C ";
    draft.answers.q2.customText = " Reason ";
    expect(questionnaireAnswer(questions, draft)).toEqual({ version: 1, answers: [
      { id: "q1", selected: ["A", "B"], customText: "C" }, { id: "q2", selected: [], customText: "Reason" },
    ] });
  });
  it("single choice and custom input are exclusive without destroying the custom draft", () => {
    const pending = form(); pending.questions![0].multiSelect = false;
    const question = questionItems(pending)[0]; const answer = questionDraft(pending).answers.q1;
    chooseQuestionOption(question, answer, "A"); chooseQuestionOption(question, answer, "B");
    expect(answer.selected).toEqual(["B"]);
    chooseCustomAnswer(question, answer); answer.customText = "saved";
    expect(answer.selected).toEqual([]);
    chooseQuestionOption(question, answer, "A");
    expect(answer.customActive).toBe(false); expect(answer.customText).toBe("saved");
    chooseCustomAnswer(question, answer);
    expect(questionAnswered(question, answer)).toBe(true);
  });
  it("preserves remounted drafts and isolates groups and changed specifications", () => {
    const first = form(); questionDraft(first).answers.q1.selected = ["B"];
    expect(questionDraft(JSON.parse(JSON.stringify(first))).answers.q1.selected).toEqual(["B"]);
    expect(questionDraft(form("other")).answers.q1.selected).toEqual([]);
    first.questions![0].options = [];
    expect(questionDraft(first).answers.q1.selected).toEqual([]);
  });
  it("keeps answers after submission failure, prevents concurrent submits, and retries", async () => {
    const pending = form(); const draft = questionDraft(pending);
    draft.answers.q1.selected = ["A"]; draft.answers.q2.customText = "Because";
    const payload = questionnaireAnswer(questionItems(pending), draft);
    let reject!: (reason: Error) => void;
    mocks.invoke.mockImplementationOnce(() => new Promise((_, fail) => { reject = fail; }));
    const request = submitQuestionForm(pending, payload);
    expect(draft.submitting).toBe(true);
    expect(await submitQuestionForm(pending, payload)).toBe(false);
    expect(mocks.invoke).toHaveBeenCalledTimes(1);
    reject(new Error("Connection lost"));
    expect(await request).toBe(false);
    expect(draft.error).toContain("Connection lost");
    expect(draft.answers.q2.customText).toBe("Because");
    expect(draft.submitting).toBe(false);
    mocks.invoke.mockResolvedValueOnce(undefined);
    expect(await submitQuestionForm(pending, payload)).toBe(true);
    expect(mocks.invoke).toHaveBeenLastCalledWith("answer_question", { questionId: "group", answer: payload });
    expect(questionDraft(pending).answers.q2.customText).toBe("");
  });
  it("does not change string answers used by tool and plan approvals", async () => {
    mocks.invoke.mockResolvedValueOnce(undefined);
    await answerQuestion("confirm", "approve");
    expect(mocks.invoke).toHaveBeenCalledWith("answer_question", { questionId: "confirm", answer: "approve" });
  });
  it("summarizes the whole question group in the tool header", () => {
    expect(buildToolCallArgsSummary("ask_user_question", JSON.stringify({ questions: form().questions }))).toBe("Targets · Reason");
  });
});
