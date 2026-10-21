import { ipcInvoke } from './ipc';
import { readonly, ref } from 'vue';
import type { WorkspaceRef } from './project';

const remoteClaudeCode = ref(false);
export const isRemoteClaudeCode = readonly(remoteClaudeCode);
const modelCatalogScope = ref('');
export const claudeCodeModelCatalogScope = readonly(modelCatalogScope);
const prewarmSettings = ref({ enabled: false, capacity: 1, idleMinutes: 15, scope: '' });
export const claudePrewarmSettings = readonly(prewarmSettings);

function rememberConnection(connection: ClaudeConnection): ClaudeConnection {
  remoteClaudeCode.value = !!connection.remote;
  prewarmSettings.value = { enabled: !!connection.remote && !!connection.prewarm, capacity: connection.poolCapacity ?? 1,
    idleMinutes: connection.idleMinutes ?? 15, scope: JSON.stringify([connection.remote, connection.permissionMode, !!connection.idleCompaction, !!connection.telemetry, connection.autoCompactWindow ?? null]) };
  // A catalog belongs to one CLI configuration, including the server's
  // configured gateway model. Permission/password changes do not affect it.
  modelCatalogScope.value = connection.remote
    ? JSON.stringify([connection.remote, connection.info?.model ?? null])
    : 'local';
  return connection;
}

export interface ClaudeSshConfig {
  target: string;
  port: number | null;
  identityFile: string | null;
  knownHostsFile: string | null;
  cliPath: string | null;
}
export interface ClaudeRemoteInfo {
  platform?: string | null;
  keychainUnlocked?: boolean | null;
  keychainUnlockTested?: boolean;
  pythonInstalled: boolean;
  cliVersion: string | null;
  cliSupported: boolean;
  model: string | null;
  authenticated: boolean;
  home: string | null;
}
export type ClaudePermissionMode = 'auto' | 'bypassPermissions';
export interface ClaudeConnection {
  version: number;
  permissionMode: ClaudePermissionMode;
  idleCompaction?: boolean;
  telemetry?: boolean;
  autoCompactWindow?: number | null;
  prewarm?: boolean;
  poolCapacity?: number;
  idleMinutes?: number;
  remote: ClaudeSshConfig | null;
  authorizedTarget: string | null;
  info: ClaudeRemoteInfo | null;
  hasUnlockPassword?: boolean;
}
export const getClaudeConnection = () => ipcInvoke<ClaudeConnection>('get_claude_connection').then(rememberConnection);
export type ClaudeConnectionPhase = 'disconnected' | 'connecting' | 'starting' | 'ready';
export interface ClaudeSessionConnection {
  phase: ClaudeConnectionPhase;
  target: string;
  port: number | null;
  latencyMs: number | null;
  lastError?: string | null;
  pool: {
    state: 'disabled' | 'idle' | 'warming' | 'ready' | 'waiting' | 'retrying';
    capacity: number;
    ready: number;
    starting: number;
    active: number;
    retained: number;
  };
}
// Local lifecycle snapshot only; this does not probe or open an SSH connection.
export const getClaudeSessionConnection = (sessionId: string | null, workingDir?: string, model?: string) => ipcInvoke<ClaudeSessionConnection | null>('get_claude_session_connection', { sessionId, workingDir, model });
export const setClaudePrewarm = (enabled: boolean, capacity: number, idleMinutes: number) => ipcInvoke<ClaudeConnection>('set_claude_prewarm', { enabled, capacity, idleMinutes }).then(rememberConnection);
export const prewarmClaudeSession = (sessionId: string | null, model: string, workspaceRef: WorkspaceRef, warmExisting: boolean) =>
  ipcInvoke<void>('prewarm_claude_session', { sessionId, model, workspaceRef, warmExisting });
export const connectClaudeSession = (model: string, workspaceRef: WorkspaceRef) =>
  ipcInvoke<void>('connect_claude_session', { model, workspaceRef });
export const setClaudePermissionMode = (mode: ClaudePermissionMode) => ipcInvoke<ClaudeConnection>('set_claude_permission_mode', { mode }).then(rememberConnection);
export const setClaudeRuntimeSettings = (idleCompaction: boolean, telemetry: boolean, autoCompactWindow: number | null) =>
  ipcInvoke<ClaudeConnection>('set_claude_runtime_settings', { idleCompaction, telemetry, autoCompactWindow }).then(rememberConnection);
export const inspectClaudeSsh = (config: ClaudeSshConfig) => ipcInvoke<ClaudeRemoteInfo>('inspect_claude_ssh', { config });
export const authorizeClaudeSsh = (config: ClaudeSshConfig, unlockPassword?: string) => ipcInvoke<ClaudeConnection>('authorize_claude_ssh', { config, unlockPassword: unlockPassword ?? null }).then(rememberConnection);
export const clearClaudeSshUnlockPassword = (config: ClaudeSshConfig) => ipcInvoke<void>('clear_claude_ssh_unlock_password', { config });
export const useLocalClaude = () => ipcInvoke<ClaudeConnection>('use_local_claude').then(rememberConnection);

export interface ClaudeNativeArchive {
  version: number;
  nativeId: string;
  updatedAt: number;
  transferredBytes: number;
  incompleteTail: boolean;
  manifest: Record<string, { size: number; sha256: string }>;
}
export const getClaudeNativeArchive = (sessionId: string) => ipcInvoke<ClaudeNativeArchive | null>('get_claude_native_archive', { sessionId });
export const backupClaudeNativeSession = (sessionId: string) => ipcInvoke<ClaudeNativeArchive | null>('backup_claude_native_session', { sessionId });
export interface ClaudeSessionTransfer {
  source: string;
  target: string;
  targetFingerprint: string;
  nativeId: string;
  archiveDigest: string | null;
  updatedAt: number | null;
  blockedReason: 'running' | 'unauthorized' | 'noBackup' | 'incomplete' | null;
}
export const checkClaudeSessionTransfer = (sessionId: string) => ipcInvoke<ClaudeSessionTransfer | null>('check_claude_session_transfer', { sessionId });
export const restoreClaudeNativeSession = (sessionId: string, transfer: ClaudeSessionTransfer) => ipcInvoke<{ nativeId: string; target: string; transferredBytes: number }>('restore_claude_native_session', {
  sessionId, expectedTarget: transfer.targetFingerprint, expectedNative: transfer.nativeId, expectedArchive: transfer.archiveDigest,
});
