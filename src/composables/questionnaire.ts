import { reactive } from "vue";
import type { AskQuestion, PendingQuestion, QuestionnaireAnswer } from "../types";

export interface QuestionDraftAnswer {
  selected: string[];
  customActive: boolean;
  customText: string;
}
export interface QuestionDraft {
  index: number;
  review: boolean;
  answers: Record<string, QuestionDraftAnswer>;
  submitting: boolean;
  error: string;
}

export function questionItems(pending: PendingQuestion): AskQuestion[] {
  if (pending.questions?.length) return pending.questions;
  const custom = pending.options[pending.options.length - 1];
  return [{
    id: "q1", question: pending.question, header: "", options: pending.options.slice(0, -1),
    multiSelect: false, allowCustom: true,
    customLabel: custom?.label, customDescription: custom?.description,
  }];
}

// Retain unfinished answers when a chat moves between panes or is remounted.
// UUID group keys prevent cross-session mixing; successful submissions evict
// their entry. Bound abandoned/cancelled drafts instead of retaining them forever.
const drafts = new Map<string, { signature: string; draft: QuestionDraft }>();
export function questionDraft(pending: PendingQuestion): QuestionDraft {
  const questions = questionItems(pending);
  const signature = JSON.stringify(questions);
  const cached = drafts.get(pending.questionId);
  if (cached?.signature === signature) return cached.draft;
  const draft = reactive<QuestionDraft>({
    index: 0, review: false, submitting: false, error: "",
    answers: Object.fromEntries(questions.map((q) => [q.id, { selected: [], customActive: q.options.length === 0, customText: "" }])),
  });
  drafts.set(pending.questionId, { signature, draft });
  if (drafts.size > 64) drafts.delete(drafts.keys().next().value!);
  return draft;
}
export function clearQuestionDraft(questionId: string): void { drafts.delete(questionId); }

export function chooseQuestionOption(question: AskQuestion, answer: QuestionDraftAnswer, label: string): void {
  if (question.multiSelect) {
    answer.selected = answer.selected.includes(label) ? answer.selected.filter((value) => value !== label) : [...answer.selected, label];
  } else {
    answer.selected = [label];
    answer.customActive = false;
  }
}
export function chooseCustomAnswer(question: AskQuestion, answer: QuestionDraftAnswer): void {
  answer.customActive = question.options.length === 0 || !answer.customActive;
  if (answer.customActive && !question.multiSelect) answer.selected = [];
}
export function questionAnswered(question: AskQuestion, answer: QuestionDraftAnswer): boolean {
  const custom = answer.customActive && answer.customText.trim().length > 0;
  if (answer.customActive && !custom) return false;
  if (custom && !question.allowCustom) return false;
  if (answer.selected.some((label) => !question.options.some((o) => o.label === label))) return false;
  const count = answer.selected.length + Number(custom);
  return question.multiSelect ? count > 0 : count === 1;
}
export function questionnaireAnswer(questions: AskQuestion[], draft: QuestionDraft): QuestionnaireAnswer {
  if (!questions.every((q) => questionAnswered(q, draft.answers[q.id]))) throw new Error("Answer every question before submitting");
  return { version: 1, answers: questions.map((q) => {
    const answer = draft.answers[q.id];
    return { id: q.id, selected: q.options.filter((o) => answer.selected.includes(o.label)).map((o) => o.label), customText: answer.customActive ? answer.customText.trim() : "" };
  }) };
}
export function questionAnswerText(answer: QuestionDraftAnswer): string {
  return [...answer.selected, ...(answer.customActive && answer.customText.trim() ? [answer.customText.trim()] : [])].join(", ");
}
