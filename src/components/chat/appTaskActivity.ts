import { computed, onScopeDispose, reactive, ref, type ComputedRef } from "vue";
import { listAppTasks, subscribeAppTaskUpdates, type AppTaskInfo } from "../../services/appTasks";
import type { RuntimeUnsubscribe } from "../../services/locusRuntime";

const MAX_FAILURES = 20;

// One subscription shared by every mounted indicator. Chat surfaces remount on
// each session switch, so live tasks and unseen failures live here.
const live = reactive(new Map<string, AppTaskInfo>());
const failures = ref<AppTaskInfo[]>([]);
const sortedTasks = computed(() => [...live.values()].sort((a, b) => a.startedAt - b.startedAt));
const failureCount = computed(() => failures.value.length);
let consumers = 0;
let generation = 0;
let unsubscribe: RuntimeUnsubscribe | undefined;

function apply(task: AppTaskInfo) {
  if (task.status === "running") {
    live.set(task.taskId, task);
    return;
  }
  live.delete(task.taskId);
  if (task.status === "failed") {
    failures.value = [...failures.value.filter((item) => item.taskId !== task.taskId), task].slice(-MAX_FAILURES);
  }
}

async function connect(current: number): Promise<void> {
  // Updates that race the snapshot are replayed over it: terminal ones always,
  // running ones only for tasks the (possibly older) snapshot does not know.
  let log: AppTaskInfo[] | null = [];
  try {
    const release = await subscribeAppTaskUpdates((task) => {
      if (current !== generation) return;
      if (log) log.push(task);
      else apply(task);
    });
    if (current !== generation) {
      release();
      return;
    }
    unsubscribe = release;
  } catch (cause) {
    console.error("Failed to subscribe to app task updates", cause);
  }
  try {
    const tasks = await listAppTasks();
    if (current !== generation) return;
    live.clear();
    for (const task of tasks) live.set(task.taskId, task);
  } catch (cause) {
    console.error("Failed to list app tasks", cause);
  } finally {
    if (current === generation) {
      const pending = log ?? [];
      log = null;
      for (const task of pending) {
        if (task.status !== "running" || !live.has(task.taskId)) apply(task);
      }
    }
  }
}

function retain() {
  consumers += 1;
  if (consumers === 1) void connect(generation);
}

function release() {
  consumers = Math.max(0, consumers - 1);
  if (consumers > 0) return;
  generation += 1;
  unsubscribe?.();
  unsubscribe = undefined;
}

export function useAppTasks(): { tasks: ComputedRef<AppTaskInfo[]>; failureCount: ComputedRef<number> } {
  retain();
  onScopeDispose(release);
  return { tasks: sortedTasks, failureCount };
}

/** Returns and clears failures the user has not seen yet. */
export function takeAppTaskFailures(): AppTaskInfo[] {
  const taken = failures.value;
  failures.value = [];
  return taken;
}

export function resetAppTaskActivityForTests(): void {
  generation += 1;
  consumers = 0;
  unsubscribe = undefined;
  live.clear();
  failures.value = [];
}
