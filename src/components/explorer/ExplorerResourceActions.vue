<script setup lang="ts">
import { reactive, ref } from "vue";
import { confirm, save } from "@tauri-apps/plugin-dialog";
import { t } from "../../i18n";
import { useNotificationStore } from "../../stores/notification";
import { useWorkspaceExplorerStore } from "../../stores/workspaceExplorer";
import { normalizeAppError } from "../../services/errors";
import { explorerFileAction } from "../../services/workspaceExplorer";
import { deleteSkillPackage, exportSkillPackage, knowledgeDelete, knowledgeMove, knowledgeRevealTarget } from "../../services/knowledge";
import { showInFolder } from "../../services/unity";
import { isSkillPackageRootDocument, skillPackageIdForDocument } from "../../composables/useKnowledgeState";
import { useExplorerPathDisplay } from "../../composables/useExplorerPathDisplay";
import { knowledgeResourceManagedHint } from "../knowledge/knowledgeResourceActions";
import { resourceName, resourceRelativePath, validResourceName, type ExplorerResourceAction, type ExplorerResourceRename, type ExplorerResourceTarget } from "./explorerResourceActions";
import type { FileHistoryTarget } from "../../services/fileHistory";
import { workspaceMovedPath } from "./workspaceFileMove";

const emit = defineEmits<{
  changed: [target: ExplorerResourceTarget, newPath: string | null];
  history: [target: FileHistoryTarget];
}>();
const notificationStore = useNotificationStore();
const explorerStore = useWorkspaceExplorerStore();
const { movePathPreference } = useExplorerPathDisplay();
const draft = ref<ExplorerResourceRename | null>(null);
function cancel(current = draft.value) {
  if (draft.value === current && !current?.busy) draft.value = null;
}

async function mutate(target: ExplorerResourceTarget, name: string | null) {
  if (target.document && knowledgeResourceManagedHint(target.document)) return;
  await apply(target, name);
  await refresh(target.projectId, !!target.document);
}

async function apply(target: ExplorerResourceTarget, name: string | null) {
  const document = target.document;
  let nextPath: string | null = null;
  if (document) {
    if (!target.workspaceRef) throw new Error(t("development.workspaceUnavailable"));
    if (name !== null) {
      nextPath = document.path.replace(/[^/\\]+$/, name);
      await knowledgeMove({ kind: "document", type: document.type, path: document.path, newPath: nextPath }, target.workspaceRef);
    } else if (isSkillPackageRootDocument(document)) {
      await deleteSkillPackage(skillPackageIdForDocument(document)!, target.workspaceRef);
    } else {
      await knowledgeDelete({ kind: "document", type: document.type, path: document.path }, target.workspaceRef);
    }
  } else {
    const workspaceRef = target.mounted ? null : target.workspaceRef;
    // Scoped file actions require paths relative to the captured checkout root.
    // Project-authorized mounts keep their absolute paths.
    const path = workspaceRef ? resourceRelativePath(target) : target.path;
    nextPath = await explorerFileAction(target.projectId, path, name, workspaceRef);
    if (nextPath) movePathPreference(target.path, nextPath);
  }
  emit("changed", target, nextPath);
}

async function refresh(projectId: string, knowledge: boolean) {
  // A refresh failure must not present an already completed rename as a failed
  // mutation or invite a retry against the old path.
  try {
    if (knowledge) await explorerStore.refreshProjectKnowledge(projectId);
    else await explorerStore.loadProject(projectId, true);
    const mounts = explorerStore.snapshots[projectId]?.nodes.filter((node) => node.nodeKind === "folder" && node.sourcePath) ?? [];
    const results = await Promise.allSettled(mounts.map((node) => explorerStore.loadMount(projectId, node.nodeId, true)));
    const failure = results.find((result) => result.status === "rejected");
    if (failure?.status === "rejected") throw failure.reason;
  } catch (error) { notificationStore.addNotice("warning", normalizeAppError(error).message); }
}

/** Batch copy/delete for a multi-selection; a single target keeps the regular action flow. */
async function runMany(action: "copy" | "delete", targets: ExplorerResourceTarget[]) {
  if (targets.length === 1) return run(action, targets[0]!);
  const managed = (target: ExplorerResourceTarget) => !!target.document && !!knowledgeResourceManagedHint(target.document);
  // Entries inside a selected folder go with it; deleting them first would only fail.
  const folders = action === "delete" ? targets.filter((target) => target.isDirectory && !managed(target)) : [];
  const eligible = targets.filter((target) => !managed(target) && !folders.some((folder) => folder !== target
    && workspaceMovedPath(target.path, folder.path, folder.path, true) !== null));
  if (eligible.length === 1) return run(action, eligible[0]!);
  if (eligible.length === 0) return;
  try {
    if (action === "copy") {
      await navigator.clipboard.writeText(eligible.map(resourceRelativePath).join("\n"));
      notificationStore.addNotice("success", t("knowledge.explorer.relativePathCopied"));
      return;
    }
    const approved = await confirm(t("knowledge.explorer.deleteManyConfirm", eligible.length), {
      title: t("common.confirmDelete"), kind: "warning", okLabel: t("common.delete"), cancelLabel: t("common.cancel"),
    });
    if (!approved) return;
    // Continue past one failed entry so the rest of the selection is still deleted.
    for (const target of eligible) {
      try { await apply(target, null); }
      catch (error) { notificationStore.addNotice("error", normalizeAppError(error).message); }
    }
    const refreshes = new Set(eligible.map((target) => `${target.document ? "k" : "f"}:${target.projectId}`));
    for (const key of refreshes) await refresh(key.slice(2), key.startsWith("k:"));
  } catch (error) { notificationStore.addNotice("error", normalizeAppError(error).message); }
}

async function commit(current: ExplorerResourceRename) {
  if (draft.value !== current || current.busy) return;
  const name = current.name.trim();
  if (!validResourceName(name)) {
    current.error = t("development.invalidFileName");
    notificationStore.addNotice("error", current.error);
    return;
  }
  if (name === resourceName(current.target)) { draft.value = null; return; }
  current.busy = true;
  current.error = "";
  try { await mutate(current.target, name); if (draft.value === current) draft.value = null; }
  catch (error) {
    current.error = normalizeAppError(error).message;
    notificationStore.addNotice("error", current.error);
  }
  finally { current.busy = false; }
}

async function run(action: ExplorerResourceAction, target: ExplorerResourceTarget, rowKey = target.path) {
  if ((action === "rename" || action === "delete") && target.document && knowledgeResourceManagedHint(target.document)) return;
  try {
    if (action === "history") {
      const workspaceRef = target.mounted ? null : target.workspaceRef ?? null;
      emit("history", {
        projectId: target.projectId,
        path: target.document ? `Locus/knowledge/${resourceRelativePath(target)}` : workspaceRef ? resourceRelativePath(target) : target.path,
        workspaceRef: workspaceRef ? { ...workspaceRef } : null,
      });
    } else if (action === "rename") {
      if (draft.value?.busy || target.isDirectory || (target.document && isSkillPackageRootDocument(target.document))) return;
      const current: ExplorerResourceRename = reactive({
        key: rowKey, target, name: resourceName(target), busy: false, error: "",
        submit: () => commit(current), cancel: () => cancel(current),
      });
      draft.value = current;
    } else if (action === "delete") {
      const packageRoot = target.document && isSkillPackageRootDocument(target.document);
      const message = packageRoot ? "knowledge.explorer.deletePackageConfirm"
        : target.isDirectory ? "knowledge.explorer.deleteFolderConfirm" : "knowledge.explorer.deleteDocumentConfirm";
      const approved = await confirm(t(message, resourceName(target)), {
        title: t("common.confirmDelete"), kind: "warning", okLabel: t("common.delete"), cancelLabel: t("common.cancel"),
      });
      if (approved) await mutate(target, null);
    } else if (action === "copy") {
      await navigator.clipboard.writeText(resourceRelativePath(target));
      notificationStore.addNotice("success", t("knowledge.explorer.relativePathCopied"));
    } else if (action === "reveal" && target.workspaceRef) {
      if (target.document) await knowledgeRevealTarget({ kind: "document", docType: target.document.type, path: target.document.path }, target.workspaceRef);
      else await showInFolder(target.workspaceRef, target.path);
    } else if (action === "export" && target.document && target.workspaceRef) {
      const packageId = skillPackageIdForDocument(target.document);
      if (!packageId || target.document.externalSource?.locator?.startsWith("external://")) return;
      const path = await save({ defaultPath: `${packageId.replace(/[/\\]/g, "-")}.zip`, filters: [{ name: t("knowledge.skillPackage.archiveFilter"), extensions: ["zip"] }] });
      if (path) await exportSkillPackage(packageId, path, target.workspaceRef);
    }
  } catch (error) { notificationStore.addNotice("error", normalizeAppError(error).message); }
}
defineExpose({ run, runMany, draft, cancel });
</script>

<template></template>
