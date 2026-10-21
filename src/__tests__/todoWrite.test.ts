
import { describe, expect, it } from "vitest";
import { parseLegacyTodoWriteOutput, parseTodoWriteArguments, todoToolItems, todoToolOutput } from "../composables/todoWrite";

describe("todowrite presentation", () => {
  it.each(["TodoWrite", "TaskCreate", "TaskUpdate", "TaskList"])("shows %s snapshots without protocol/task details", (name) => {
    const items = [{ content: "done", status: "completed", priority: "medium" }];
    for (const snapshot of [
      { version: 1, items },
      { version: 1, kind: "tasks", items, tasks: [{ description: "full internal details" }] },
    ]) {
      expect(todoToolItems(name, "{}", JSON.stringify({ locusTodoSnapshot: snapshot }), "done")).toEqual(items);
    }
    expect(todoToolItems(name, "{}", "Permission denied", "error")).toBeNull();
    expect(todoToolItems(name, "{}", "", "running")).toBeNull();
  });

  it("preserves task operation warnings without echoing synchronization details", () => {
    const snapshot = { version: 1, items: [], tasks: [{ description: "SYNC_BODY" }] };
    expect(todoToolOutput("TaskUpdate", JSON.stringify({ locusTodoSnapshot: snapshot, locusOutput: "Task updated; warning: dependency is pending" }))).toBe("Task updated; warning: dependency is pending");
    const legacy = todoToolOutput("TaskUpdate", JSON.stringify({ locusTodoSnapshot: snapshot, result: { warning: "Pending dependency" } }));
    expect(legacy).toContain("Pending dependency");
    expect(legacy).not.toContain("SYNC_BODY");
    expect(todoToolOutput("python", JSON.stringify({ locusTodoSnapshot: snapshot, locusOutput: "irrelevant" }))).toBe("");
  });

  it("does not hide failed writes or unrelated JSON outputs", () => {
    expect(todoToolItems("todowrite", '{"todos":[]}', "failed", "error")).toBeNull();
    expect(todoToolItems("python", "{}", '{"locusTodoSnapshot":{"version":1,"items":[]}}', "done")).toBeNull();
  });
  it("normalizes tool arguments into displayable todos", () => {
    expect(parseTodoWriteArguments(JSON.stringify({
      todos: [
        { content: "done", status: "completed", priority: "high" },
        { content: "active", status: "in_progress" },
        { content: "invalid", status: "unknown", priority: "unknown" },
        { status: "pending" },
      ],
    }))).toEqual([
      { content: "done", status: "completed", priority: "high" },
      { content: "active", status: "in_progress", priority: "medium" },
      { content: "invalid", status: "pending", priority: "medium" },
    ]);
  });

  it("supports echoed results from older sessions", () => {
    expect(parseLegacyTodoWriteOutput([
      "1 todos (0 remaining)",
      '[{"content":"done","status":"completed","priority":"low"}]',
    ].join("\n"))).toEqual([
      { content: "done", status: "completed", priority: "low" },
    ]);
  });
});
