<script setup lang="ts">
import { computed, onUnmounted, ref } from "vue";
import { t } from "../../i18n";
import type { WorktreePlanProgress } from "../../services/worktrees";
import { formatWorktreeBytes } from "../../utils/worktreeDiskUsage";
import BaseButton from "../ui/BaseButton.vue";

const props = defineProps<{
  progress: WorktreePlanProgress;
  cancellable?: boolean;
  /** Start of the tracked operation; defaults to when this view appeared. */
  startedAt?: number;
  branch?: string;
  /** The operation continues when this view closes. */
  background?: boolean;
}>();
const emit = defineEmits<{ cancel: [] }>();
const started = props.startedAt ?? Date.now();
const elapsedSeconds = () => Math.max(0, Math.floor((Date.now() - started) / 1000));
const seconds = ref(elapsedSeconds());
const timer = setInterval(() => { seconds.value = elapsedSeconds(); }, 1000);
onUnmounted(() => clearInterval(timer));
const checkout = computed(() => props.progress.phase === "creating" && !!props.progress.totalFiles);
const percent = computed(() => props.progress.totalFiles
  ? Math.min(100, Math.floor((props.progress.files / props.progress.totalFiles) * 100))
  : 0);
</script>

<template>
  <div class="worktree-progress" role="status" aria-live="polite">
    <div class="progress-heading"><span>{{ t(`worktrees.progress.${progress.phase}`) }}</span><span>{{ t('worktrees.progress.elapsed', seconds) }}</span></div>
    <span v-if="branch" class="progress-branch" :title="branch">{{ branch }}</span>
    <progress :value="progress.totalFiles ? progress.files : undefined" :max="progress.totalFiles || 1" :aria-label="t(`worktrees.progress.${progress.phase}`)" />
    <span v-if="checkout" class="progress-detail">{{ t('worktrees.progress.checkedOut', percent, progress.files.toLocaleString(), progress.totalFiles!.toLocaleString()) }}</span>
    <span v-else-if="progress.files > 0" class="progress-detail">{{ t('worktrees.progress.files', progress.files.toLocaleString(), formatWorktreeBytes(progress.bytes)) }}</span>
    <span v-if="background" class="progress-hint">{{ t('worktrees.progress.background') }}</span>
    <BaseButton v-if="cancellable" @click="emit('cancel')">{{ t('common.cancel') }}</BaseButton>
  </div>
</template>

<style scoped>
.worktree-progress { display: flex; flex-direction: column; gap: 6px; padding: 10px 8px; border-top: 1px solid var(--border-color); color: var(--text-secondary); font-size: 11px; }
.progress-heading { display: flex; justify-content: space-between; gap: 8px; font-variant-numeric: tabular-nums; }
.progress-branch { overflow: hidden; color: var(--text-color); font-family: var(--font-mono-identifier); text-overflow: ellipsis; white-space: nowrap; }
progress { width: 100%; height: 3px; accent-color: var(--accent-color); }
.progress-detail { font-variant-numeric: tabular-nums; }
.progress-hint { line-height: 1.45; }
.worktree-progress > :deep(button) { align-self: flex-start; }
</style>
