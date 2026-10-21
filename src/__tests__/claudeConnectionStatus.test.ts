import { effectScope, ref, type Ref } from 'vue';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useClaudeConnectionStatus } from '../composables/useClaudeConnectionStatus';
import * as service from '../services/claudeConnection';
import * as cliGate from '../services/claudeCodeCli';

vi.mock('../services/claudeCodeCli', () => ({ claudeCodeCliEnabled: ref(true) }));
vi.mock('../services/claudeConnection', () => ({
  getClaudeSessionConnection: vi.fn(),
  isRemoteClaudeCode: ref(true),
  claudePrewarmSettings: ref({ enabled: true, capacity: 2, idleMinutes: 15, scope: 'ssh-a' }),
}));
const ready: service.ClaudeSessionConnection = { phase: 'ready', target: 'user@server', port: null, latencyMs: 42,
  pool: { state: 'ready', capacity: 2, ready: 2, starting: 0, active: 1, retained: 3 } };
const scopes: ReturnType<typeof effectScope>[] = [];
function harness() {
  const scope = effectScope();
  scopes.push(scope);
  return { ...scope.run(useClaudeConnectionStatus)!, stop: () => scope.stop() };
}
const settle = async () => { await Promise.resolve(); await Promise.resolve(); };
const settings = service.claudePrewarmSettings as Ref<{ enabled: boolean; capacity: number; idleMinutes: number; scope: string }>;
beforeEach(() => {
  vi.useFakeTimers();
  (cliGate.claudeCodeCliEnabled as unknown as Ref<boolean>).value = true;
  (service.isRemoteClaudeCode as Ref<boolean>).value = true;
  settings.value = { enabled: true, capacity: 2, idleMinutes: 15, scope: 'ssh-a' };
  vi.mocked(service.getClaudeSessionConnection).mockReset().mockResolvedValue(ready);
});
afterEach(() => { scopes.splice(0).forEach(scope => scope.stop()); vi.useRealTimers(); });

describe('shared remote Claude process status', () => {
  it('shares health, latency and pool counts across panes using one global snapshot', async () => {
    const left = harness(), right = harness();
    await settle();
    expect(service.getClaudeSessionConnection).toHaveBeenCalledExactlyOnceWith(null);
    expect(left.status).toBe(right.status);
    expect(left.phase).toBe(right.phase);
    expect(left.status.value).toEqual(ready);
    const third = harness();
    expect(third.phase.value).toBe('ready');
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(1);
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue({ ...ready, latencyMs: 85, pool: { ...ready.pool, active: 2 } });
    await vi.advanceTimersByTimeAsync(1000);
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(2);
    for (const pane of [left, right, third]) {
      expect(pane.visible.value).toBe(true);
      expect(pane.status.value?.latencyMs).toBe(85);
      expect(pane.status.value?.pool.active).toBe(2);
    }
  });

  it('closing the first pane keeps the detached poller alive until the last pane closes', async () => {
    const first = harness(), second = harness();
    await settle();
    first.stop();
    expect(second.phase.value).toBe('ready');
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue({ ...ready, phase: 'disconnected', latencyMs: null });
    await vi.advanceTimersByTimeAsync(1000);
    expect(second.phase.value).toBe('disconnected');
    second.stop();
    const count = vi.mocked(service.getClaudeSessionConnection).mock.calls.length;
    await vi.advanceTimersByTimeAsync(3000);
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(count);
  });

  it('clears stale health in every pane on IPC failure and recovers together', async () => {
    const left = harness(), right = harness();
    await settle();
    vi.mocked(service.getClaudeSessionConnection).mockRejectedValueOnce(new Error('IPC unavailable'));
    await vi.advanceTimersByTimeAsync(1000);
    for (const pane of [left, right]) {
      expect(pane.visible.value).toBe(true);
      expect(pane.phase.value).toBe('unknown');
      expect(pane.status.value).toBeNull();
      expect(pane.error.value).toBe('IPC unavailable');
    }
    await vi.advanceTimersByTimeAsync(1000);
    expect(left.phase.value).toBe('ready');
    expect(right.phase.value).toBe('ready');
    expect(right.error.value).toBe('');
  });

  it('shares background startup errors throughout retries and clears them on recovery or a server change', async () => {
    const error = 'Claude plugin validation failed.\nregister.js:116: invalid helper';
    const failed = { ...ready, phase: 'disconnected' as const, lastError: error,
      pool: { ...ready.pool, state: 'retrying' as const, ready: 0 } };
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue(failed);
    const left = harness(), right = harness();
    await settle();
    expect(left.error.value).toBe(error);
    expect(right.error.value).toBe(error);
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue({ ...failed, phase: 'starting' });
    await vi.advanceTimersByTimeAsync(1000);
    expect(left.error.value).toBe(error);
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue(ready);
    await vi.advanceTimersByTimeAsync(1000);
    expect(right.error.value).toBe('');
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue(failed);
    await vi.advanceTimersByTimeAsync(1000);
    expect(left.error.value).toBe(error);
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue({ ...ready, target: 'server-b' });
    settings.value = { ...settings.value, scope: 'ssh-b' };
    expect(left.error.value).toBe('');
    await settle();
    expect(right.error.value).toBe('');
  });

  it('hides all icons and stops polling in local mode, then resumes once for all panes', async () => {
    const left = harness(), right = harness();
    await settle();
    (service.isRemoteClaudeCode as Ref<boolean>).value = false;
    for (const pane of [left, right]) {
      expect(pane.visible.value).toBe(false);
      expect(pane.status.value).toBeNull();
    }
    const count = vi.mocked(service.getClaudeSessionConnection).mock.calls.length;
    await vi.advanceTimersByTimeAsync(3000);
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(count);
    (service.isRemoteClaudeCode as Ref<boolean>).value = true;
    await settle();
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(count + 1);
    expect(right.phase.value).toBe('ready');
  });

  it('hides remote status and stops polling while the experimental provider is off', async () => {
    const pane = harness();
    await settle();
    (cliGate.claudeCodeCliEnabled as unknown as Ref<boolean>).value = false;
    expect(pane.visible.value).toBe(false);
    expect(pane.status.value).toBeNull();
    const count = vi.mocked(service.getClaudeSessionConnection).mock.calls.length;
    await vi.advanceTimersByTimeAsync(3000);
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(count);
  });

  it('ignores late snapshots from a previous server for all panes', async () => {
    let old!: (value: service.ClaudeSessionConnection) => void;
    vi.mocked(service.getClaudeSessionConnection).mockImplementationOnce(() => new Promise(resolve => { old = resolve; }));
    const left = harness(), right = harness();
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue({ ...ready, target: 'server-b' });
    settings.value = { ...settings.value, scope: 'ssh-b' };
    await settle();
    old({ ...ready, target: 'server-a' });
    await settle();
    expect(left.status.value?.target).toBe('server-b');
    expect(right.status.value?.target).toBe('server-b');
  });

  it('does not overlap slow reads or let a disposed poller overwrite a new one', async () => {
    let old!: (value: service.ClaudeSessionConnection) => void;
    vi.mocked(service.getClaudeSessionConnection).mockImplementationOnce(() => new Promise(resolve => { old = resolve; }));
    const first = harness(), second = harness();
    await vi.advanceTimersByTimeAsync(5000);
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(1);
    first.stop(); second.stop();
    const next = harness();
    await settle();
    old({ ...ready, phase: 'connecting' });
    await settle();
    expect(next.phase.value).toBe('ready');
    await vi.advanceTimersByTimeAsync(1000);
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(3);
  });

  it('refreshes pool setting changes immediately but not equivalent configuration reads', async () => {
    const pane = harness();
    await settle();
    settings.value = { ...settings.value };
    await settle();
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(1);
    vi.mocked(service.getClaudeSessionConnection).mockResolvedValue({ ...ready, pool: { ...ready.pool, capacity: 3 } });
    settings.value = { ...settings.value, capacity: 3 };
    await settle();
    expect(service.getClaudeSessionConnection).toHaveBeenCalledTimes(2);
    expect(pane.status.value?.pool.capacity).toBe(3);
  });
});
