type RecordValue = Record<string, unknown>;
const record = (value: unknown): value is RecordValue => !!value && typeof value === "object" && !Array.isArray(value);
const text = (value: unknown): string => typeof value === "string" ? value : "";
const paths = (value: unknown): string => Array.isArray(value) ? value.filter((item): item is string => typeof item === "string").join("\n") : "";

/** Compatibility projection for transcripts saved before native result/display
 * separation. Never rewrite arbitrary JSON produced by Python or other tools. */
export function nativeToolOutputDisplay(name: string, output: string): string | null {
  let value: unknown;
  let trailing = "";
  try { value = JSON.parse(output); }
  catch {
    const split = output.indexOf("\n\n");
    if (split < 0) return null;
    try { value = JSON.parse(output.slice(0, split)); trailing = output.slice(split); }
    catch { return null; }
  }
  if (!record(value)) return null;
  let result: string | null = null;
  if (["write", "Write"].includes(name) && ["create", "update"].includes(text(value.type)) && typeof value.filePath === "string") {
    result = `${value.type === "update" ? "Edited" : "Created"} ${value.filePath}`;
  } else if (["list", "Glob"].includes(name) && Array.isArray(value.filenames) && typeof value.numFiles === "number") {
    result = `Found ${value.totalMatches ?? value.numFiles} files (showing ${value.numFiles})${value.truncated ? "; truncated, narrow the pattern" : ""}\n${paths(value.filenames)}`;
  } else if (["grep", "Grep"].includes(name) && typeof value.mode === "string" && Array.isArray(value.filenames)) {
    result = `Found ${value.numMatches ?? "?"} matches in ${value.totalFiles ?? value.numFiles} files\n${value.mode === "files_with_matches" ? paths(value.filenames) : text(value.content)}`;
  } else if (["ask_user_question", "AskUserQuestion"].includes(name) && record(value.answers) && Array.isArray(value.questions)) {
    result = Object.entries(value.answers).map(([question, answer]) => `${question}\nUser answered: ${text(answer)}`).join("\n\n");
  } else if (["python", "TaskOutput"].includes(name) && typeof value.retrieval_status === "string" && record(value.task) && value.task.task_type === "local_bash") {
    result = `Task ${text(value.task.task_id)}: ${text(value.task.status)} (retrieval: ${value.retrieval_status})\n${text(value.task.output)}`;
  } else if (["python", "TaskStop"].includes(name) && value.task_type === "local_bash" && typeof value.task_id === "string" && typeof value.message === "string") {
    result = `${value.message}: ${value.task_id}`;
  }
  return result === null ? null : result + trailing;
}
