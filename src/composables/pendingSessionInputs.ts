import type { PendingSessionInput } from "../types";

export function pendingInputDelivery(input: PendingSessionInput): "after_run" | "immediate" {
  return input.delivery === "immediate" ? "immediate" : "after_run";
}

/** Inputs still owned by the queue: waiting, being delivered, or retained for the user. */
export function visiblePendingInputs(inputs: PendingSessionInput[] | undefined): PendingSessionInput[] {
  return (inputs ?? []).filter((input) =>
    input.status === "queued" || input.status === "delivering" || input.status === "retained");
}

export function mergePendingInputList(
  list: PendingSessionInput[],
  input: PendingSessionInput,
): PendingSessionInput[] {
  const index = list.findIndex((item) =>
    item.id === input.id
    || (
      item.runId === input.runId
      && item.mergeGroupId === input.mergeGroupId
      && item.status !== "delivering"
      && item.status !== "accepted"
      && item.status !== "restored"
    ));
  if (index < 0) return [...list, input];
  const existing = list[index]!;
  if (existing.id === input.id) {
    // Command replies and stream events interleave; the queue revision orders
    // them, so a returned steering claim can become editable again.
    const incoming = input.revision ?? 0;
    const current = existing.revision ?? 0;
    if (incoming < current) return list;
    // Without a newer revision, a delayed enqueue acknowledgement must not
    // make sent steering withdrawable.
    if (incoming === current && existing.status === "delivering" && input.status === "queued") {
      return list;
    }
  }
  const next = [...list];
  next.splice(index, 1, input);
  return next;
}

/**
 * True when no run will deliver the input on its own: it was retained after a
 * failure or ambiguous hand-off, or it waits for insertion into a run that is
 * no longer active.
 */
export function isUndeliveredPendingInput(
  input: PendingSessionInput,
  activeRunId: string | null,
): boolean {
  if (input.status === "retained") return true;
  return input.status === "queued"
    && pendingInputDelivery(input) === "immediate"
    && input.runId !== activeRunId;
}

/** Prefer the active run's inputs; otherwise surface leftovers needing a decision. */
export function queuedFollowUpInputs(
  inputs: PendingSessionInput[] | undefined,
  activeRunId: string | null,
): PendingSessionInput[] {
  const visible = visiblePendingInputs(inputs);
  if (!activeRunId) return visible;
  const current = visible.filter((input) => input.runId === activeRunId);
  return current.length > 0 ? current : visible;
}

export interface QueuedFollowUpSummary {
  inputs: PendingSessionInput[];
  displayText: string;
  images: NonNullable<PendingSessionInput["images"]>;
  canInsert: boolean;
  canEdit: boolean;
  isInserting: boolean;
  isUndelivered: boolean;
}

export function summarizeQueuedFollowUp(
  inputs: PendingSessionInput[],
  activeRunId: string | null,
): QueuedFollowUpSummary | null {
  if (inputs.length === 0) return null;
  const isUndelivered = inputs.every((input) => isUndeliveredPendingInput(input, activeRunId));
  return {
    inputs,
    displayText: inputs
      .map((input) => input.displayText || input.text)
      .filter((text) => text.trim().length > 0)
      .join("\n"),
    images: inputs.flatMap((input) => input.images ?? []),
    canInsert: inputs.some((input) =>
      input.status === "queued"
      && input.runId === activeRunId
      && pendingInputDelivery(input) !== "immediate"),
    canEdit: inputs.every((input) => input.status === "queued" || input.status === "retained"),
    isInserting: !isUndelivered && inputs.every((input) => pendingInputDelivery(input) === "immediate"),
    isUndelivered,
  };
}
