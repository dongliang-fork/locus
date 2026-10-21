import { computed, effectScope, onScopeDispose, readonly, shallowRef, watch, type EffectScope } from 'vue';
import {
  claudePrewarmSettings, getClaudeSessionConnection, isRemoteClaudeCode,
  type ClaudeSessionConnection,
} from '../services/claudeConnection';
import { claudeCodeCliEnabled } from '../services/claudeCodeCli';
import { normalizeAppError } from '../services/errors';

// All composers in this renderer share one process-level snapshot and poller.
// Focus, selected model and conversation identity never own connection health.
const status = shallowRef<ClaudeSessionConnection | null>(null);
const pollError = shallowRef('');
const error = computed(() => status.value?.lastError || pollError.value);
const phase = computed(() => status.value?.phase ?? 'unknown');
const visible = computed(() => claudeCodeCliEnabled.value && isRemoteClaudeCode.value);
const shared = { status: readonly(status), visible, phase, error };
let consumers = 0;
let polling: EffectScope | undefined;

function startPolling() {
  // Detached from the first composer's lifetime: closing it must not stop
  // updates for other panes that still display the same process state.
  polling = effectScope(true);
  polling.run(() => {
    let generation = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const clear = () => { generation++; clearTimeout(timer); timer = undefined; };
    watch([() => visible.value, () => JSON.stringify(claudePrewarmSettings.value)], () => {
      clear();
      status.value = null;
      pollError.value = '';
      if (!visible.value) return;
      const version = generation;
      async function refresh() {
        try {
          // Null session + no model/cwd requests the configured server's whole
          // process pool. This reads local state; it never opens SSH or sends input.
          const result = await getClaudeSessionConnection(null);
          if (version === generation) { status.value = result; pollError.value = ''; }
        } catch (error) {
          // Invalidate health, latency and pool counts for every pane together.
          if (version === generation) { status.value = null; pollError.value = normalizeAppError(error).message; }
        } finally {
          if (version === generation) timer = setTimeout(refresh, 1000);
        }
      }
      void refresh();
    }, { immediate: true, flush: 'sync' });
    onScopeDispose(() => { clear(); status.value = null; pollError.value = ''; });
  });
}

export function useClaudeConnectionStatus() {
  if (consumers++ === 0) startPolling();
  onScopeDispose(() => {
    if (--consumers === 0) {
      polling?.stop();
      polling = undefined;
    }
  });
  return shared;
}
