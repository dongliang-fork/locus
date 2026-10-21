import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ipcInvoke } from "./ipc";

export const SESSION_MAINTENANCE_EVENT = "session-maintenance";

/** Background session-database maintenance reported by the backend. */
export type SessionMaintenanceStatus =
  | { phase: "purgingCaptures"; doneRows: number; totalRows: number }
  | { phase: "compacting"; writtenBytes: number; totalBytes: number }
  | { phase: "checkpointing" }
  | { phase: "finished"; purgedRows: number; reclaimedBytes: number }
  | {
    phase: "insufficientSpace";
    reclaimableBytes: number;
    databaseRequiredBytes: number;
    databaseAvailableBytes: number;
    tempRequiredBytes: number;
    tempAvailableBytes: number;
  }
  | { phase: "failed"; message: string };

export function getSessionMaintenanceStatus(): Promise<SessionMaintenanceStatus | null> {
  return ipcInvoke("get_session_maintenance_status");
}

export function listenSessionMaintenance(
  handler: (status: SessionMaintenanceStatus | null) => void,
): Promise<UnlistenFn> {
  return listen<SessionMaintenanceStatus | null>(SESSION_MAINTENANCE_EVENT, (event) => {
    handler(event.payload);
  });
}
