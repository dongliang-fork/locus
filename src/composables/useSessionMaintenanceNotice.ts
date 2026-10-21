import { t } from "../i18n";
import {
  getSessionMaintenanceStatus,
  listenSessionMaintenance,
  type SessionMaintenanceStatus,
} from "../services/sessionMaintenance";
import { useNotificationStore } from "../stores/notification";
import type { NotificationLevel } from "../types";
import { formatSize } from "./useGarbageCollection";

const operation = "sessionMaintenance";

export interface SessionMaintenanceNotice {
  level: NotificationLevel;
  message: string;
  sticky: boolean;
  spinner: boolean;
  ttl?: number;
}

function percent(done: number, total: number): number {
  return Math.min(100, Math.floor((done * 100) / Math.max(total, 1)));
}

export function sessionMaintenanceNotice(status: SessionMaintenanceStatus): SessionMaintenanceNotice {
  switch (status.phase) {
    case "purgingCaptures":
      return {
        level: "info",
        sticky: true,
        spinner: true,
        message: t(
          "sessionMaintenance.purging",
          percent(status.doneRows, status.totalRows),
          status.doneRows,
          status.totalRows,
        ),
      };
    case "compacting":
      return {
        level: "info",
        sticky: true,
        spinner: true,
        message: t("sessionMaintenance.compacting", percent(status.writtenBytes, status.totalBytes)),
      };
    case "checkpointing":
      return { level: "info", sticky: true, spinner: true, message: t("sessionMaintenance.checkpointing") };
    case "finished":
      return {
        level: "success",
        sticky: false,
        spinner: false,
        message: status.reclaimedBytes > 0
          ? t("sessionMaintenance.finished", formatSize(status.reclaimedBytes))
          : t("sessionMaintenance.purged"),
      };
    case "insufficientSpace": {
      const tempShort = status.tempAvailableBytes < status.tempRequiredBytes;
      return {
        level: "warning",
        sticky: false,
        spinner: false,
        ttl: 15_000,
        message: t(
          tempShort ? "sessionMaintenance.noTempSpace" : "sessionMaintenance.noDatabaseSpace",
          formatSize(status.reclaimableBytes),
          formatSize(tempShort ? status.tempRequiredBytes : status.databaseRequiredBytes),
          formatSize(tempShort ? status.tempAvailableBytes : status.databaseAvailableBytes),
        ),
      };
    }
    case "failed":
      return { level: "error", sticky: true, spinner: false, message: t("sessionMaintenance.failed", status.message) };
  }
}

/**
 * Mirrors background session-database maintenance into one notice. The
 * backend may start before the window listens, so the current status is
 * fetched once; any event received meanwhile is newer and wins.
 */
export async function watchSessionMaintenance(): Promise<() => void> {
  const notifications = useNotificationStore();
  let receivedEvent = false;
  const show = (status: SessionMaintenanceStatus | null) => {
    if (!status) {
      notifications.clearByOperation(operation);
      return;
    }
    const notice = sessionMaintenanceNotice(status);
    notifications.addNotice(notice.level, notice.message, {
      operation,
      replaceOperation: true,
      sticky: notice.sticky,
      spinner: notice.spinner,
      ttl: notice.ttl,
      skipConsoleLog: notice.spinner,
    });
  };
  const unlisten = await listenSessionMaintenance((status) => {
    receivedEvent = true;
    show(status);
  });
  try {
    const status = await getSessionMaintenanceStatus();
    if (!receivedEvent) show(status);
  } catch (error) {
    console.warn("[session-maintenance] failed to read status", error);
  }
  return unlisten;
}
