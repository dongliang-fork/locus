import { onBeforeUnmount, ref } from "vue";
import { useInternalDropTarget } from "./useInternalDrag";
import { moveWorkspaceFile } from "../services/workspaceExplorer";
import { useNotificationStore } from "../stores/notification";
import { normalizeAppError } from "../services/errors";
import { WORKBENCH_REFERENCE_INTERNAL_DRAG_TYPE, type WorkbenchReferenceDragData } from "../components/workbench/workbenchReferenceDrag";
import { resolveWorkspaceFileMove, type WorkspaceFileDropScope } from "../components/explorer/workspaceFileMove";
import "../styles/workspace-file-drop.css";

interface FileDropTarget extends WorkspaceFileDropScope { element: HTMLElement }

export function useWorkspaceFileDrop(options: {
  root: () => HTMLElement | null;
  resolve: (hit: Element) => FileDropTarget | null;
  expand?: (target: FileDropTarget) => void;
}) {
  const notifications = useNotificationStore();
  const busy = ref(false);
  let highlighted: HTMLElement | null = null;
  let expandTimer: ReturnType<typeof setTimeout> | undefined;
  onBeforeUnmount(() => { clearTimeout(expandTimer); highlighted?.classList.remove("is-workspace-file-drop-target"); });
  useInternalDropTarget<WorkbenchReferenceDragData, { target: FileDropTarget; move: NonNullable<ReturnType<typeof resolveWorkspaceFileMove>> }>({
    id: `workspace-file-move:${crypto.randomUUID()}`,
    root: options.root,
    priority: 30,
    accepts: source => !busy.value && source.payload.type === WORKBENCH_REFERENCE_INTERNAL_DRAG_TYPE
      && (source.payload.data as WorkbenchReferenceDragData).fileSystemMove === true,
    resolve: ({ hit, source }) => {
      const target = options.resolve(hit);
      const move = target && resolveWorkspaceFileMove(source.payload.data, target);
      return target && move ? { key: `${target.workspaceRef.checkoutId}:${move.targetDirectory}`, operation: "move",
        intent: { target, move } } : null;
    },
    onTargetChange: decision => {
      highlighted?.classList.remove("is-workspace-file-drop-target");
      clearTimeout(expandTimer);
      highlighted = decision?.intent.target.element ?? null;
      highlighted?.classList.add("is-workspace-file-drop-target");
      if (decision && options.expand) expandTimer = setTimeout(() => options.expand?.(decision.intent.target), 650);
    },
    drop: async ({ decision }) => {
      busy.value = true;
      try {
        const { workspaceRef, sourcePaths, targetDirectory } = decision.intent.move;
        // Move sequentially so one failed entry does not cancel the rest of a multi-selection.
        for (const sourcePath of sourcePaths) {
          try { await moveWorkspaceFile(workspaceRef, sourcePath, targetDirectory); }
          catch (error) { notifications.addNotice("error", normalizeAppError(error).message); }
        }
      } finally { busy.value = false; }
    },
  });
}
