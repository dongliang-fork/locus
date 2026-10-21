// Focused release harness: compiles the production grep engine, both adapters,
// and their tests without linking the desktop app. No copied search algorithm.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import os from "node:os";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = new Set(process.argv.slice(2));
for (const arg of args) {
  if (!["--tests-only", "--real"].includes(arg)) throw new Error(`Unknown argument: ${arg}`);
}
if (args.has("--real") && !process.env.LOCUS_GREP_BENCH_ROOT) throw new Error("--real requires LOCUS_GREP_BENCH_ROOT");
const dir = path.join(os.tmpdir(), `locus-grep-harness-${createHash("sha256").update(root).digest("hex").slice(0, 12)}`);
mkdirSync(dir, { recursive: true });
const appLock = readFileSync(path.join(root, "src-tauri/Cargo.lock"), "utf8");
const version = name => {
  const match = appLock.match(new RegExp(`\\[\\[package\\]\\]\\s+name = "${name}"\\s+version = "([^"]+)"`));
  if (!match) throw new Error(`Missing locked dependency: ${name}`);
  return `=${match[1]}`;
};
writeFileSync(path.join(dir, "Cargo.lock"), appLock);
const source = relative => JSON.stringify(path.join(root, relative).replaceAll("\\", "/"));
const pathModule = readFileSync(path.join(root, "src-tauri/src/tool/path.rs"), "utf8");
const end = pathModule.indexOf("\nimpl super::ToolExecutionContext");
if (end < 0) throw new Error("Production path resolver layout changed; update harness boundary");
writeFileSync(path.join(dir, "lib.rs"), `#![allow(dead_code, unused_imports)]
#[global_allocator] static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
// Only application shell types are stubbed; search/path code below is production.
mod prompt {
    pub mod tools { pub const GREP: &str = ""; }
    pub struct Prompt { pub description: String, pub parameters: serde_json::Value }
    pub fn parse_tool_prompt(_: &str) -> Prompt { Prompt { description: String::new(), parameters: serde_json::json!({}) } }
}
mod tool {
    #[derive(Clone, Default)] pub struct ToolExecutionContext { pub working_dir: Option<String> }
    impl ToolExecutionContext { pub fn working_root(&self) -> Option<&std::path::Path> { self.working_dir.as_deref().map(std::path::Path::new) } }
    pub struct ToolResult { pub output: String, pub is_error: bool }
    pub type ToolExecuteFn = std::sync::Arc<dyn Fn(serde_json::Value, ToolExecutionContext) -> std::pin::Pin<Box<dyn std::future::Future<Output=ToolResult> + Send>> + Send + Sync>;
    pub struct ToolDef { pub name: String, pub description: String, pub parameters: serde_json::Value, pub mutates_workspace: bool, pub execute: ToolExecuteFn }
    pub mod path { ${pathModule.slice(0, end)} }
    pub mod builtins {
        use super::{ToolDef, ToolResult, ToolExecutionContext, ToolExecuteFn};
        fn make_exec<F>(f: F) -> ToolExecuteFn where F: Fn(serde_json::Value,ToolExecutionContext) -> std::pin::Pin<Box<dyn std::future::Future<Output=ToolResult>+Send>> + Send + Sync + 'static { std::sync::Arc::new(f) }
        #[path = ${source("src-tauri/src/tool/builtins/search_engine.rs")}] pub(crate) mod search_engine;
        #[path = ${source("src-tauri/src/tool/builtins/search_core.rs")}] pub(crate) mod search_core;
        #[path = ${source("src-tauri/src/tool/builtins/search.rs")}] mod search;
    }
}
mod llm { pub mod claude_code_cli {
    #[path = ${source("src-tauri/src/llm/claude_code_cli/native_grep.rs")}] pub(crate) mod native_grep;
} }
`);
writeFileSync(path.join(dir, "Cargo.toml"), `[package]
name = "locus-grep-harness"
version = "0.1.0"
edition = "2021"
[lib]
path = "lib.rs"
[dependencies]
${["ignore", "regex", "grep-regex", "grep-searcher", "dunce", "tempfile", "serde_json", "mimalloc"].map(name => `${name} = "${version(name)}"`).join("\n")}
tokio = { version = "${version("tokio")}", features = ["rt-multi-thread", "macros"] }
[profile.release]
lto = "off"
codegen-units = 256
`);
console.log(`Grep harness and logs: ${dir}`);
const records = [];
async function cargo(label, extra) {
  let output = "";
  const child = spawn("cargo", ["test", "--offline", "--release", "--manifest-path", path.join(dir, "Cargo.toml"),
    "--target-dir", path.join(root, "src-tauri/target"), ...extra], { cwd: root, windowsHide: true });
  for (const stream of [child.stdout, child.stderr]) stream.on("data", data => { output += data; process.stdout.write(data); });
  const code = await new Promise((resolve, reject) => { child.on("error", reject); child.on("close", resolve); });
  writeFileSync(path.join(dir, `${label}.log`), output);
  for (const line of output.split(/\r?\n/)) {
    const marker = line.indexOf("GREP_BENCH {");
    if (marker >= 0) records.push(JSON.parse(line.slice(marker + "GREP_BENCH ".length)));
  }
  if (code !== 0) throw new Error(`${label} failed (${code}); see ${dir}`);
}
await cargo("tests", ["--lib", "--", "--test-threads=1"]);
if (!args.has("--tests-only")) {
  await cargo("synthetic", ["grep_benchmark_synthetic", "--", "--ignored", "--nocapture", "--test-threads=1"]);
  if (args.has("--real")) await cargo("external", ["grep_benchmark_external", "--", "--ignored", "--nocapture", "--test-threads=1"]);
  writeFileSync(path.join(dir, "benchmark.json"), JSON.stringify({ capturedAt: new Date().toISOString(), allocator: "mimalloc", records }, null, 2));
}
