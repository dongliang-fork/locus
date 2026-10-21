<script setup lang="ts">
import { computed, nextTick, ref } from "vue";
import { Check, MessageCircleQuestionMark } from "lucide";
import type { AskQuestion, PendingQuestion, QuestionnaireAnswer } from "../../types";
import { t } from "../../i18n";
import BaseButton from "../ui/BaseButton.vue";
import LucideIcon from "../icons/LucideIcon.vue";
import {
  questionItems, questionDraft, chooseQuestionOption, chooseCustomAnswer,
  questionAnswered, questionnaireAnswer, questionAnswerText,
} from "../../composables/questionnaire";

const props = defineProps<{ question: PendingQuestion }>();
const emit = defineEmits<{ answer: [answer: QuestionnaireAnswer, questionId: string] }>();
const questions = computed(() => questionItems(props.question));
const draft = computed(() => questionDraft(props.question));
const current = computed(() => questions.value[draft.value.index]);
const answer = computed(() => draft.value.answers[current.value.id]);
const multi = computed(() => questions.value.length > 1);
const answeredCount = computed(() => questions.value.filter(isAnswered).length);
const ready = computed(() => answeredCount.value === questions.value.length);
const currentReady = computed(() => questionAnswered(current.value, answer.value));
const inputType = computed(() => current.value.multiSelect ? "checkbox" : "radio");
const inputName = computed(() => `${props.question.questionId}-${current.value.id}`);
const customInput = ref<HTMLTextAreaElement | null>(null);

function isAnswered(item: AskQuestion) { return questionAnswered(item, draft.value.answers[item.id]); }
function isCurrent(index: number) { return !draft.value.review && index === draft.value.index; }
function answerText(item: AskQuestion) { return questionAnswerText(draft.value.answers[item.id]); }
function navigate(index: number) { draft.value.index = index; draft.value.review = false; }
function toggleCustom() {
  chooseCustomAnswer(current.value, answer.value);
  if (answer.value.customActive) void nextTick(() => customInput.value?.focus());
}
function next() {
  if (!currentReady.value || draft.value.submitting) return;
  if (draft.value.index + 1 < questions.value.length) draft.value.index++;
  else draft.value.review = true;
}
function submit() {
  if (!ready.value || draft.value.submitting) return;
  if (multi.value && !draft.value.review) { draft.value.review = true; return; }
  emit("answer", questionnaireAnswer(questions.value, draft.value), props.question.questionId);
}
</script>

<template>
  <form class="ask-user-card" :aria-label="t('ask.formTitle')" @submit.prevent="submit">
    <div class="ask-header">
      <LucideIcon class="ask-header-icon" :icon="MessageCircleQuestionMark" :size="14" aria-hidden="true" />
      <span class="ask-title">{{ t('ask.formTitle') }}</span>
      <span v-if="multi" class="ask-progress" aria-live="polite">{{ t('ask.answered', answeredCount, questions.length) }}</span>
    </div>

    <nav v-if="multi" class="ask-steps" :aria-label="t('ask.navigation')">
      <button v-for="(item, index) in questions" :key="item.id" type="button" class="ask-step"
        :class="{ current: isCurrent(index), done: isAnswered(item) }" :disabled="draft.submitting"
        :aria-current="isCurrent(index) ? 'step' : undefined" :title="item.question"
        :data-question="item.id" @click="navigate(index)">
        <span class="ask-step-mark">
          <LucideIcon v-if="isAnswered(item)" :icon="Check" :size="11" :stroke-width="3" aria-hidden="true" />
          <template v-else>{{ index + 1 }}</template>
        </span>
        <span class="ask-step-label">{{ item.header || t('ask.questionNumber', index + 1) }}</span>
      </button>
    </nav>

    <section v-if="draft.review" class="ask-review" :aria-label="t('ask.review')">
      <div class="ask-review-title">{{ t('ask.review') }}</div>
      <ol class="ask-review-list">
        <li v-for="(item, index) in questions" :key="item.id" class="ask-review-item">
          <div class="ask-review-text">
            <span class="ask-review-question">{{ index + 1 }}. {{ item.question }}</span>
            <span class="ask-review-answer" :class="{ empty: !answerText(item) }">{{ answerText(item) || t('ask.unanswered') }}</span>
          </div>
          <BaseButton class="ask-review-edit" :disabled="draft.submitting" :data-edit-question="item.id" @click="navigate(index)">
            {{ t('common.edit') }}
          </BaseButton>
        </li>
      </ol>
    </section>
    <fieldset v-else class="ask-fields" :disabled="draft.submitting">
      <legend class="ask-question">{{ current.question }}</legend>
      <div v-if="current.options.length" class="ask-hint">{{ current.multiSelect ? t('ask.multiSelect') : t('ask.singleSelect') }}</div>
      <div class="ask-options">
        <label v-for="option in current.options" :key="option.label" class="ask-option"
          :class="{ selected: answer.selected.includes(option.label) }">
          <input class="ask-option-input" :type="inputType" :name="inputName" :value="option.label"
            :checked="answer.selected.includes(option.label)" @change="chooseQuestionOption(current, answer, option.label)" />
          <span class="ask-option-text">
            <span class="ask-option-label">{{ option.label }}</span>
            <span v-if="option.description" class="ask-option-desc">{{ option.description }}</span>
          </span>
        </label>
        <div v-if="current.allowCustom && current.options.length" class="ask-option is-custom" :class="{ selected: answer.customActive }">
          <label class="ask-option-row">
            <input class="ask-option-input" :type="inputType" :name="inputName" value="__custom__"
              :checked="answer.customActive" @change="toggleCustom" />
            <span class="ask-option-text"><span class="ask-option-label">{{ current.customLabel || t('ask.other') }}</span></span>
          </label>
          <textarea v-if="answer.customActive" ref="customInput" v-model="answer.customText" class="ask-custom-input" rows="2"
            :aria-label="current.customLabel || t('ask.customAnswer')" :placeholder="current.customDescription || t('ask.customPlaceholder')" />
        </div>
        <textarea v-else-if="current.allowCustom" ref="customInput" v-model="answer.customText" class="ask-custom-input" rows="3"
          :aria-label="current.customLabel || t('ask.customAnswer')" :placeholder="current.customDescription || t('ask.customPlaceholder')" />
      </div>
    </fieldset>

    <div v-if="draft.error" class="ask-error" role="alert">{{ draft.error }}</div>
    <div class="ask-actions">
      <BaseButton v-if="draft.review || draft.index > 0" size="md" :disabled="draft.submitting"
        @click="draft.review ? navigate(questions.length - 1) : navigate(draft.index - 1)">{{ t('common.back') }}</BaseButton>
      <BaseButton v-if="multi && !draft.review" variant="primary" size="md" :disabled="!currentReady || draft.submitting" @click="next">
        {{ draft.index + 1 === questions.length ? t('ask.review') : t('ask.next') }}
      </BaseButton>
      <BaseButton v-else type="submit" variant="primary" size="md" :disabled="!ready || draft.submitting">
        {{ draft.submitting ? t('ask.submitting') : t('ask.submit') }}
      </BaseButton>
    </div>
  </form>
</template>

<style scoped>
/* The host owns the card frame (ChatView's shared .ask-user-card shell, the
   terminal's inline panel); this component only lays out its content. */
.ask-user-card {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
  color: var(--text-color);
  font-size: 13px;
}

.ask-header {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.ask-header-icon {
  flex: none;
  color: var(--accent-color);
}

/* Eyebrow label: the question below is the card's primary heading. */
.ask-title {
  min-width: 0;
  overflow: hidden;
  color: var(--text-secondary);
  font-size: 12px;
  font-weight: 600;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.ask-progress {
  flex: none;
  margin-left: auto;
  color: var(--text-secondary);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}

.ask-steps {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.ask-step {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  max-width: 100%;
  height: 26px;
  padding: 0 10px 0 4px;
  border: 1px solid var(--border-color);
  border-radius: 6px;
  background: transparent;
  color: var(--text-secondary);
  font: inherit;
  font-size: 12px;
  cursor: pointer;
  transition: background 0.12s ease, border-color 0.12s ease, color 0.12s ease;
}

.ask-step:hover:not(:disabled) {
  border-color: var(--border-strong);
  background: var(--hover-bg);
  color: var(--text-color);
}

.ask-step:focus-visible {
  outline: none;
  box-shadow: 0 0 0 1px var(--accent-color);
}

.ask-step:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.ask-step.current {
  border-color: color-mix(in srgb, var(--accent-color) 60%, var(--border-color));
  background: color-mix(in srgb, var(--accent-color) 12%, transparent);
  color: var(--text-color);
}

.ask-step-mark {
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 18px;
  border-radius: 4px;
  background: var(--hover-bg);
  font-size: 11px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}

.ask-step.done .ask-step-mark {
  background: color-mix(in srgb, var(--accent-color) 18%, transparent);
  color: var(--accent-color);
}

.ask-step.current .ask-step-mark {
  background: var(--accent-color);
  color: #fff;
}

.ask-step-label {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.ask-fields {
  display: flex;
  flex-direction: column;
  min-width: 0;
  margin: 0;
  padding: 0;
  border: 0;
}

/* A floated legend is laid out as an ordinary flex item instead of being
   placed in the fieldset border, so it follows the column's spacing. */
.ask-question {
  float: left;
  width: 100%;
  padding: 0;
  font-size: 14px;
  font-weight: 600;
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.ask-hint {
  margin-top: 2px;
  color: var(--text-secondary);
  font-size: 12px;
}

.ask-options {
  display: flex;
  flex-direction: column;
  gap: 6px;
  max-height: 40vh;
  margin-top: 10px;
  overflow-y: auto;
}

.ask-option {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  min-width: 0;
  padding: 8px 10px;
  border: 1px solid var(--border-color);
  border-radius: 8px;
  cursor: pointer;
  transition: background 0.12s ease, border-color 0.12s ease;
}

.ask-option:hover {
  border-color: var(--border-strong);
  background: var(--hover-bg);
}

.ask-option.selected {
  border-color: color-mix(in srgb, var(--accent-color) 55%, var(--border-color));
  background: color-mix(in srgb, var(--accent-color) 9%, transparent);
}

.ask-option:has(.ask-option-input:focus-visible) {
  box-shadow: inset 0 0 0 1px var(--accent-color);
}

.ask-fields:disabled .ask-option,
.ask-fields:disabled .ask-option-row {
  opacity: 0.6;
  cursor: not-allowed;
}

.ask-option.is-custom {
  flex-direction: column;
  align-items: stretch;
  gap: 0;
  padding: 0;
  cursor: default;
}

.ask-option-row {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 8px 10px;
  cursor: pointer;
}

.ask-option-input {
  appearance: none;
  flex: none;
  display: grid;
  place-content: center;
  width: 14px;
  height: 14px;
  margin: 3px 0 0;
  border: 1px solid color-mix(in srgb, var(--text-secondary) 60%, transparent);
  border-radius: 4px;
  background: var(--input-bg, transparent);
  cursor: inherit;
  transition: background 0.12s ease, border-color 0.12s ease;
}

.ask-option-input[type="radio"] {
  border-radius: 50%;
}

.ask-option-input:focus-visible {
  outline: none;
}

.ask-option-input:checked {
  border-color: var(--accent-color);
  background: var(--accent-color);
}

.ask-option-input[type="checkbox"]:checked::after {
  content: "";
  width: 7px;
  height: 4px;
  border: solid #fff;
  border-width: 0 0 1.5px 1.5px;
  transform: translateY(-1px) rotate(-45deg);
}

.ask-option-input[type="radio"]:checked::after {
  content: "";
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: #fff;
}

.ask-option-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.ask-option-label {
  color: var(--text-color);
  font-size: 13px;
  font-weight: 500;
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.ask-option-desc {
  color: var(--text-secondary);
  font-size: 12px;
  line-height: 1.45;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.ask-custom-input {
  display: block;
  box-sizing: border-box;
  width: 100%;
  min-height: 56px;
  padding: 7px 10px;
  border: 1px solid var(--border-color);
  border-radius: 6px;
  background: var(--input-bg, var(--panel-bg));
  color: var(--text-color);
  font: inherit;
  font-size: 13px;
  line-height: 1.5;
  resize: vertical;
}

.ask-custom-input:focus {
  outline: none;
  border-color: color-mix(in srgb, var(--accent-color) 60%, var(--border-color));
}

.ask-custom-input::placeholder {
  color: var(--text-secondary);
}

/* Indent the nested field to the option text column: 10px padding + 14px
   indicator + 10px gap. */
.ask-option.is-custom .ask-custom-input {
  width: calc(100% - 44px);
  margin: 0 10px 10px 34px;
}

.ask-review {
  display: flex;
  flex-direction: column;
  gap: 8px;
  min-width: 0;
}

.ask-review-title {
  font-size: 14px;
  font-weight: 600;
}

.ask-review-list {
  max-height: 40vh;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  border: 1px solid var(--border-color);
  border-radius: 8px;
  list-style: none;
}

.ask-review-item {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 8px 10px;
}

.ask-review-item + .ask-review-item {
  border-top: 1px solid var(--border-color);
}

.ask-review-text {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.ask-review-question {
  color: var(--text-secondary);
  font-size: 12px;
  line-height: 1.45;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.ask-review-answer {
  color: var(--text-color);
  font-size: 13px;
  font-weight: 500;
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.ask-review-answer.empty {
  color: var(--status-danger-fg);
  font-weight: 400;
}

.ask-review-edit {
  flex: none;
}

.ask-error {
  padding: 6px 10px;
  border-radius: 6px;
  background: var(--status-danger-bg);
  color: var(--status-danger-fg);
  font-size: 12px;
  line-height: 1.45;
  overflow-wrap: anywhere;
}

.ask-actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
}
</style>
