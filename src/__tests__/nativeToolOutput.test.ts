import { describe, expect, it } from "vitest";
import { nativeToolOutputDisplay } from "../components/nativeToolOutput";
import { parseToolSearchOutput } from "../components/toolSearchOutput";

describe("legacy native tool output projection", () => {
  it("does not repeat old/new file bodies, including when notifications follow", () => {
    const raw = JSON.stringify({ type: "update", filePath: "plan.md", content: "new body".repeat(10000), originalFile: "old body" });
    const context = "\n\n<system-reminder>Task finished</system-reminder>";
    expect(nativeToolOutputDisplay("write", raw + context)).toBe("Edited plan.md" + context);
  });
  it("renders search text and limits instead of JSON-escaped lines", () => {
    expect(nativeToolOutputDisplay("grep", JSON.stringify({ mode: "content", filenames: ["a.txt"], totalFiles: 1, numMatches: 2, content: "a.txt:1:one\na.txt:2:two" }))).toBe("Found 2 matches in 1 files\na.txt:1:one\na.txt:2:two");
    expect(nativeToolOutputDisplay("list", JSON.stringify({ filenames: ["a", "b"], numFiles: 2, totalMatches: 3, truncated: true }))).toContain("truncated, narrow the pattern\na\nb");
  });
  it("shows task log/answers without protocol boilerplate", () => {
    expect(nativeToolOutputDisplay("python", JSON.stringify({ retrieval_status: "success", task: { task_type: "local_bash", task_id: "t1", status: "failed", output: "error\nnext line" } }))).toBe("Task t1: failed (retrieval: success)\nerror\nnext line");
    expect(nativeToolOutputDisplay("ask_user_question", JSON.stringify({ questions: [{ options: ["not repeated"] }], answers: { "Continue?": "Yes" } }))).toBe("Continue?\nUser answered: Yes");
  });
  it("does not reinterpret arbitrary tool output or incomplete streaming JSON", () => {
    for (const output of ['{"type":"update","filePath":"x","content":"important"}', '{"task":{"output":"important"}}', '{"content":']) {
      expect(nativeToolOutputDisplay("python", output)).toBeNull();
    }
    expect(nativeToolOutputDisplay("write", "Created a.md\nKnowledge index: updated")).toBeNull();
  });
  it("understands native ToolSearch wrappers without displaying schemas", () => {
    expect(parseToolSearchOutput(JSON.stringify({ result: { tools: [{ name: "my_tool", description: "Summary", input_schema: { secret: "large schema" } }] } }))).toEqual({ tools: [{ name: "my_tool", description: "Summary" }] });
  });
});
