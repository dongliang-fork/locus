import { describe, expect, it } from "vitest";

import {
  isUndeliveredPendingInput,
  mergePendingInputList,
  queuedFollowUpInputs,
  summarizeQueuedFollowUp,
  visiblePendingInputs,
} from "../composables/pendingSessionInputs";
import type { PendingSessionInput } from "../types";

function input(overrides: Partial<PendingSessionInput> = {}): PendingSessionInput {
  return {
    id: "pending-1",
    sessionId: "s1",
    runId: "run-1",
    mergeGroupId: "group-1",
    status: "queued",
    delivery: "immediate",
    text: "update",
    displayText: "update",
    createdAt: 1,
    updatedAt: 1,
    ...overrides,
  };
}

describe("pending session inputs", () => {
  it("orders claim and release by revision instead of arrival", () => {
    const claimed = input({ status: "delivering", revision: 2 });
    const staleAck = input({ status: "queued", revision: 1 });
    const released = input({ status: "queued", revision: 3 });

    expect(mergePendingInputList([claimed], staleAck)).toEqual([claimed]);
    expect(mergePendingInputList([claimed], released)).toEqual([released]);
  });

  it("keeps legacy acknowledgements from reopening sent steering", () => {
    const claimed = input({ status: "delivering" });
    expect(mergePendingInputList([claimed], input({ status: "queued" }))).toEqual([claimed]);
  });

  it("keeps retained inputs visible and editable but not insertable", () => {
    const retained = input({ status: "retained", delivery: "after_run" });
    expect(visiblePendingInputs([retained, input({ id: "done", status: "accepted" })]))
      .toEqual([retained]);

    const summary = summarizeQueuedFollowUp([retained], "run-2");
    expect(summary).toMatchObject({
      isUndelivered: true,
      isInserting: false,
      canEdit: true,
      canInsert: false,
    });
  });

  it("treats an immediate input outside the active run as undelivered", () => {
    const orphan = input({ runId: "old-run" });
    expect(isUndeliveredPendingInput(orphan, "run-2")).toBe(true);
    expect(isUndeliveredPendingInput(orphan, null)).toBe(true);
    expect(isUndeliveredPendingInput(input(), "run-1")).toBe(false);
    // An after-run follow-up of a finished run is still owned by the scheduler.
    expect(isUndeliveredPendingInput(input({ runId: "old-run", delivery: "after_run" }), null))
      .toBe(false);
  });

  it("prefers the active run and otherwise surfaces earlier leftovers", () => {
    const retained = input({ id: "old", runId: "old-run", status: "retained" });
    const current = input({ id: "now", runId: "run-2", delivery: "after_run" });

    expect(queuedFollowUpInputs([retained, current], "run-2")).toEqual([current]);
    expect(queuedFollowUpInputs([retained], "run-2")).toEqual([retained]);
    expect(queuedFollowUpInputs([retained, current], null)).toEqual([retained, current]);
    expect(summarizeQueuedFollowUp([current], "run-2")).toMatchObject({
      canInsert: true,
      isUndelivered: false,
      isInserting: false,
    });
  });
});
