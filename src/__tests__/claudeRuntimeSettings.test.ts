import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ipcInvoke } from '../services/ipc';
import { claudePrewarmSettings, getClaudeConnection, setClaudeRuntimeSettings, type ClaudeConnection } from '../services/claudeConnection';

vi.mock('../services/ipc', () => ({ ipcInvoke: vi.fn() }));

const saved: ClaudeConnection = {
  version: 1, permissionMode: 'auto', idleCompaction: true, telemetry: false,
  remote: { target: 'user@server', port: null, identityFile: null, knownHostsFile: null, cliPath: null },
  authorizedTarget: 'bound', info: null, prewarm: true,
};

beforeEach(() => vi.clearAllMocks());

describe('Claude runtime settings IPC', () => {
  it('sends the native window and invalidates the remote prewarm scope on edits and reset', async () => {
    vi.mocked(ipcInvoke).mockResolvedValueOnce(saved);
    await getClaudeConnection();
    const originalScope = claudePrewarmSettings.value.scope;
    vi.mocked(ipcInvoke).mockResolvedValueOnce({ ...saved, autoCompactWindow: 350_000 });
    await setClaudeRuntimeSettings(true, false, 350_000);
    expect(ipcInvoke).toHaveBeenLastCalledWith('set_claude_runtime_settings', {
      idleCompaction: true, telemetry: false, autoCompactWindow: 350_000,
    });
    expect(claudePrewarmSettings.value.scope).not.toBe(originalScope);
    expect(claudePrewarmSettings.value.enabled).toBe(true);
    vi.mocked(ipcInvoke).mockResolvedValueOnce({ ...saved, autoCompactWindow: null });
    await setClaudeRuntimeSettings(true, false, null);
    expect(ipcInvoke).toHaveBeenLastCalledWith('set_claude_runtime_settings', {
      idleCompaction: true, telemetry: false, autoCompactWindow: null,
    });
    expect(claudePrewarmSettings.value.scope).toBe(originalScope);
  });

  it('does not invalidate prewarmed processes when saving fails', async () => {
    vi.mocked(ipcInvoke).mockResolvedValueOnce({ ...saved, autoCompactWindow: 200_000 });
    await getClaudeConnection();
    const scope = claudePrewarmSettings.value.scope;
    vi.mocked(ipcInvoke).mockRejectedValueOnce(new Error('save failed'));
    await expect(setClaudeRuntimeSettings(true, false, 350_000)).rejects.toThrow('save failed');
    expect(claudePrewarmSettings.value.scope).toBe(scope);
  });
});
