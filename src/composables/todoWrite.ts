import type { TodoItem } from "../types";

const TODO_STATUSES = new Set<TodoItem["status"]>([
  "pending",
  "in_progress",
  "completed",
  "cancelled",
]);

const TODO_PRIORITIES = new Set<TodoItem["priority"]>([
  "high",
  "medium",
  "low",
]);

function normalizeTodoItems(value: unknown): TodoItem[] | null {
  if (!Array.isArray(value)) return null;

  const items: TodoItem[] = [];
  for (const valueItem of value) {
    if (!valueItem || typeof valueItem !== "object" || Array.isArray(valueItem)) continue;
    const item = valueItem as Record<string, unknown>;
    const content = typeof item.content === "string" ? item.content : "";
    if (!content) continue;

    const status = TODO_STATUSES.has(item.status as TodoItem["status"])
      ? item.status as TodoItem["status"]
      : "pending";
    const priority = TODO_PRIORITIES.has(item.priority as TodoItem["priority"])
      ? item.priority as TodoItem["priority"]
      : "medium";

    items.push({ content, status, priority });
  }

  return items;
}

export function parseTodoWriteArguments(argumentsText: string): TodoItem[] | null {
  try {
    const args = JSON.parse(argumentsText) as Record<string, unknown> | null;
    return args && typeof args === "object" && !Array.isArray(args)
      ? normalizeTodoItems(args.todos)
      : null;
  } catch {
    return null;
  }
}

export function parseClaudeTodoOutput(outputText: string): TodoItem[] | null {
  try {
    const snapshot = JSON.parse(outputText)?.locusTodoSnapshot;
    return snapshot?.version === 1 ? normalizeTodoItems(snapshot.items) : null;
  } catch { return null; }
}

/** Only acknowledged native snapshots become lists; never hide native errors. */
export function todoToolItems(name: string, argumentsText: string, outputText: string, status: string): TodoItem[] | null {
  if (status === "error") return null;
  if (["TodoWrite", "TaskCreate", "TaskUpdate", "TaskList"].includes(name)) {
    return parseClaudeTodoOutput(outputText);
  }
  if (name !== "todowrite") return null;
  return parseTodoWriteArguments(argumentsText) ?? parseLegacyTodoWriteOutput(outputText) ?? [];
}

/** Keep native operation messages/warnings separate from its todo snapshot. */
export function todoToolOutput(name: string, outputText: string): string {
  if (!["TodoWrite", "TaskCreate", "TaskUpdate", "TaskList"].includes(name)) return "";
  try {
    const value = JSON.parse(outputText);
    if (value?.locusTodoSnapshot?.version !== 1) return "";
    if (typeof value.locusOutput === "string") return value.locusOutput;
    // Older transcripts kept the native result beside the full task snapshot.
    if (value.result !== undefined) return typeof value.result === "string" ? value.result : JSON.stringify(value.result, null, 2);
  } catch { /* Incomplete streaming output is not an acknowledged snapshot. */ }
  return "";
}

/** Compatibility for tool results created before todowrite stopped echoing its input. */
export function parseLegacyTodoWriteOutput(outputText: string): TodoItem[] | null {
  const jsonStart = outputText.indexOf("[");
  if (jsonStart < 0) return null;

  try {
    return normalizeTodoItems(JSON.parse(outputText.slice(jsonStart)));
  } catch {
    return null;
  }
}
