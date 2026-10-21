<script setup lang="ts">
import { ref } from "vue";
import CollabView from "../CollabView.vue";
import type { ModelOption } from "../../types";
import type { WorkspaceRef } from "../../services/project";

// Full collaboration page: the branch list sits beside the graph inside the
// page instead of in the workbench secondary column.
const props = defineProps<{
  workingDir: string;
  workspaceRef: WorkspaceRef | null;
  /** The page owns keyboard focus and Git refresh priority. */
  active: boolean;
  /** The page is on screen; hidden pages drop their branch list. */
  visible: boolean;
  selectedModelId: string;
  selectedAgentId: string;
  models: ModelOption[];
  /** Header slot for branch-list actions; the page renders its own row when absent. */
  toolbarTarget?: HTMLElement | null;
  activateEditor?: () => Promise<void>;
}>();
const emit = defineEmits<{ selectModel: [id: string] }>();
const sidebarTarget = ref<HTMLElement | null>(null);
const localToolbarTarget = ref<HTMLElement | null>(null);
</script>

<template>
  <div class="workbench-collab-page">
    <aside class="workbench-collab-page-sidebar">
      <div v-if="!props.toolbarTarget" ref="localToolbarTarget" class="workbench-collab-page-toolbar" />
      <div ref="sidebarTarget" class="workbench-collab-page-branches" />
    </aside>
    <CollabView
      :working-dir="props.workingDir"
      :workspace-ref="props.workspaceRef"
      :is-active="props.active"
      :selected-model-id="props.selectedModelId"
      :selected-agent-id="props.selectedAgentId"
      :models="props.models"
      :sidebar-target="props.visible ? sidebarTarget : null"
      :sidebar-toolbar-target="props.visible ? props.toolbarTarget ?? localToolbarTarget : null"
      :activate-editor="props.activateEditor"
      @select-model="emit('selectModel', $event)"
    />
  </div>
</template>

<style scoped>
.workbench-collab-page {
  display: flex;
  flex: 1;
  width: 100%;
  height: 100%;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
  /* CollabView hides its side panel against this same breakpoint. */
  container: workbench-collab-page / inline-size;
}

.workbench-collab-page-sidebar {
  display: flex;
  flex: 0 0 260px;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
  border-right: 1px solid var(--border-color);
  background: var(--sidebar-bg);
}

/* Without a repository there is no branch list, so give the page the full width. */
.workbench-collab-page-sidebar:has(> .workbench-collab-page-branches:empty) {
  display: none;
}

.workbench-collab-page-toolbar {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  flex: 0 0 34px;
  gap: 4px;
  padding: 0 7px;
  border-bottom: 1px solid var(--border-color);
}

.workbench-collab-page-branches {
  display: flex;
  flex: 1;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
}

/* Narrow pages show only the history graph column; keep after the base sidebar rule. */
@container workbench-collab-page (max-width: 900px) {
  .workbench-collab-page-sidebar {
    display: none;
  }
}
</style>
