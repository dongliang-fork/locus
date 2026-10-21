// web.run is a reserved tool: preserve the reference schema and descriptions
// exactly, including Codex's JsonSchema serialization (not just schemars output).
// Usage: bun scripts/sync-codex-web-tool.mjs [--check] [--offline] [--ref <codex checkout>]
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const refIndex = args.indexOf('--ref');
const reference = resolve(refIndex < 0 ? join(root, 'ref/codex') : args[refIndex + 1]);
const output = join(root, 'tools/codex_web.json');
const rustPath = (path) => JSON.stringify(path.replaceAll('\\', '/'));
const read = (path) => readFileSync(join(reference, path), 'utf8');

// Extract only the command DTOs, leaving provider/auth/response dependencies out
// of this small generator. The actual schema generator and normalizer are loaded
// unchanged from the reference checkout below.
const search = read('codex-rs/codex-api/src/search.rs');
const start = search.indexOf('#[derive', search.indexOf('pub enum SearchInput'));
const end = search.indexOf('\n#[derive', search.indexOf('pub enum SearchResponseLength'));
if (start < 0 || end < 0) throw new Error('Codex SearchCommands source layout changed');
const commands = search.slice(start, end);
const version = read('codex-rs/Cargo.toml').match(/^schemars = "([^"]+)"/m)?.[1];
if (!version) throw new Error('Missing reference schemars version');

const temp = mkdtempSync(join(tmpdir(), 'locus-codex-web-schema-'));
try {
  writeFileSync(join(temp, 'Cargo.toml'), `[package]
name = "locus-codex-web-schema"
version = "0.0.0"
edition = "2024"
[dependencies]
schemars = "=${version}"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
jsonptr = "=0.7.1"
urlencoding = "2"
[[bin]]
name = "schema"
path = "main.rs"
`);
  cpSync(join(reference, 'codex-rs/tools/src/json_schema.rs'), join(temp, 'json_schema.rs'));
  cpSync(join(reference, 'codex-rs/tools/src/json_schema'), join(temp, 'json_schema'), { recursive: true });
  writeFileSync(join(temp, 'web_run_description.md'), read('codex-rs/ext/web-search/web_run_description.md').replaceAll('\r\n', '\n'));
  writeFileSync(join(temp, 'main.rs'), `#![allow(dead_code, unused_imports)]
extern crate self as codex_api;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
${commands}
#[path = ${rustPath(join(reference, 'codex-rs/ext/web-search/src/schema.rs'))}]
mod schema;
mod json_schema;
fn main() {
    let parameters = json_schema::parse_tool_input_schema_without_compaction(&schema::commands_schema()).unwrap();
    let tool = serde_json::json!({
        "type": "namespace", "name": "web", "description": "Tools in the web namespace.",
        "tools": [{ "type": "function", "name": "run", "strict": false,
            "description": include_str!("web_run_description.md"),
            "parameters": parameters }]
    });
    println!("{}", serde_json::to_string_pretty(&tool).unwrap());
}
`);
  const generated = execFileSync('cargo', ['run', '--quiet', ...(args.includes('--offline') ? ['--offline'] : []), '--manifest-path', join(temp, 'Cargo.toml')], {
    cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
  }).replaceAll('\r\n', '\n');
  const tool = JSON.parse(generated);
  if (args.includes('--check')) {
    // Ignore formatting of the file, but preserve every value and description.
    if (JSON.stringify(JSON.parse(readFileSync(output, 'utf8'))) !== JSON.stringify(tool)) {
      throw new Error('tools/codex_web.json differs from the reference web.run definition');
    }
  } else {
    writeFileSync(output, generated);
  }
  const revision = execFileSync('git', ['-C', reference, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const hash = createHash('sha256').update(JSON.stringify(tool)).digest('hex');
  console.log(`Codex reference: ${revision}\nweb.run contract SHA-256: ${hash}`);
} finally {
  if (!resolve(temp).startsWith(resolve(tmpdir()) + sep)) throw new Error('Unexpected generator temporary directory');
  rmSync(temp, { recursive: true, force: true });
}
