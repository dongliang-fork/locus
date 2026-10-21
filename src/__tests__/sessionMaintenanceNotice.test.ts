import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { setLocale } from "../i18n";
import type { SessionMaintenanceStatus } from "../services/sessionMaintenance";

const service = vi.hoisted(() => ({
  handler: null as ((status: SessionMaintenanceStatus | null) => void) | null,
  resolveStatus: null as ((status: SessionMaintenanceStatus | null) => void) | null,
}));

vi.mock("../services/sessionMaintenance", () => ({
  listenSessionMaintenance: vi.fn(async (handler: (status: SessionMaintenanceStatus | null) => void) => {
    service.handler = handler;
    return () => { service.handler = null; };
  }),
  getSessionMaintenanceStatus: vi.fn(() => new Promise((resolve) => {
    service.resolveStatus = resolve;
  })),
}));

import {
  sessionMaintenanceNotice,
  watchSessionMaintenance,
} from "../composables/useSessionMaintenanceNotice";
import { useNotificationStore } from "../stores/notification";

describe("session maintenance notice", () => {
  beforeEach(() => {
    setLocale("zh");
    setActivePinia(createPinia());
  });

  it("formats progress phases as sticky spinners", () => {
    const purge = sessionMaintenanceNotice({ phase: "purgingCaptures", doneRows: 45, totalRows: 100 });
    expect(purge).toMatchObject({ level: "info", sticky: true, spinner: true });
    expect(purge.message).toContain("45%");
    expect(purge.message).toContain("45/100");
    const compact = sessionMaintenanceNotice({ phase: "compacting", writtenBytes: 3, totalBytes: 4 });
    expect(compact.message).toContain("75%");
    expect(sessionMaintenanceNotice({ phase: "checkpointing" }).spinner).toBe(true);
  });

  it("explains which disk blocks compaction", () => {
    const gb = 1024 ** 3;
    const notice = sessionMaintenanceNotice({
      phase: "insufficientSpace",
      reclaimableBytes: 17 * gb,
      databaseRequiredBytes: 7 * gb,
      databaseAvailableBytes: 24 * gb,
      tempRequiredBytes: 7 * gb,
      tempAvailableBytes: 4 * gb,
    });
    expect(notice.level).toBe("warning");
    expect(notice.message).toContain("临时目录");
    expect(notice.message).toContain("17.00 GiB");
    expect(notice.message).toContain("4.00 GiB");
  });

  it("keeps a live event over the older initial status", async () => {
    const watching = watchSessionMaintenance();
    await vi.waitFor(() => expect(service.resolveStatus).not.toBeNull());
    service.handler?.({ phase: "finished", purgedRows: 10, reclaimedBytes: 1024 ** 3 });
    service.resolveStatus?.({ phase: "checkpointing" });
    const release = await watching;
    const notices = useNotificationStore().notices;
    expect(notices).toHaveLength(1);
    expect(notices[0]).toMatchObject({ level: "success", operation: "sessionMaintenance", spinner: false });
    service.handler?.(null);
    expect(useNotificationStore().notices).toHaveLength(0);
    release();
  });
});
