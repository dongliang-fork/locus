# Grep regression tests and benchmarks

Run from the repository root (Node.js and the existing Rust toolchain required):

```sh
# Compile production search code in release mode; run regressions only.
node scripts/grep-benchmark.mjs --tests-only

# Regressions + synthetic benchmark (default: 4,096 small files).
node scripts/grep-benchmark.mjs

# Optional larger fixture.
LOCUS_GREP_BENCH_FILES=12000 node scripts/grep-benchmark.mjs

# Optional real-tree, read-only benchmark. This never excludes runtime folders
# implicitly and can be expensive; choose the root/glob deliberately.
LOCUS_GREP_BENCH_ROOT='J:/text2anim' \
LOCUS_GREP_BENCH_PATTERN='conditioning_cache|ConditioningLoader|conditioning_name' \
LOCUS_GREP_BENCH_GLOB='*.py' \
node scripts/grep-benchmark.mjs --real
```

The focused harness loads the production engine, ordinary grep adapter and
Claude Grep adapter directly. Only desktop shell types are stubbed; the path
resolver is extracted from production source. The harness uses the application's
mimalloc allocator and seeds its lockfile from the app's Cargo.lock; direct
dependency versions are pinned as well. The full application integration tests
remain authoritative for
permission checks and NativeInvocation dispatch:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --lib tool::builtins::search
cargo test --manifest-path src-tauri/Cargo.toml --lib native_executor::tests::native_search
cargo test --manifest-path src-tauri/Cargo.toml --lib native_executor::tests::search_metadata
```

## What is measured

- Many small files: sparse content hits, misses, filename mode and count mode.
- Dense matches: complete occurrence counts with a small, deep content page.
  This also exercises the linear newline/context mapping (not the former
  repeated prefix scan).
- Ordinary grep's broad-match scan budget.
- Optional real-tree search using the same native adapter and parameters.

Synthetic fixture creation is not timed. Each deterministic case warms both
lanes, verifies exact result parity, then alternates one-worker and production
worker-count runs three times. Results include all samples, medians and speedup.
The production fan-out is `min(available_parallelism, 16)`; a single-core machine
compares two one-worker lanes. The ordinary broad-match case is reported
separately because its intentionally incomplete subset is non-deterministic.

**The serial baseline uses the same current engine with one worker**, not a copy
of the old implementation. It isolates parallel scheduling; it does not measure
all improvements against the previous whole-file/line-number implementation.

`GREP_BENCH` lines are machine-readable JSON; `GREP_BENCH_BEGIN`,
`GREP_BENCH_WARMUP`, and `GREP_BENCH_SAMPLE` expose progress for slow real trees.
The script prints the OS temporary
harness directory, which contains `tests.log`, `synthetic.log`, optional
`external.log`, and `benchmark.json`. Fixtures are temporary and removed by the
tests. Build output uses `src-tauri/target`.

## Interpreting results

- This is a warm/uncontrolled-OS-cache benchmark, not a guaranteed cold-cache
  measurement. It does not clear caches, change Defender, alter priorities, or
  stop inference processes. Report memory/I/O load when comparing runs.
- Parallelism can lose on a single file or very small/hot fixtures. No timing
  threshold or minimum speedup makes functional tests flaky. An explicitly
  supplied file path uses only one worker, even when the requested fan-out is 16.
- Native `head_limit` limits retained/returned rows, **not complete statistics**.
  Its filename/count/content metadata remains exact and path-sorted. Native
  whole-file regex semantics, hidden files, explicit generated paths, and
  binary/20 MiB guards are preserved.
- Ordinary grep retains its streaming line cap, generated-root filtering and
  early-stop warning. Both paths use the same parallel traversal and scheduler.
- `runtime/` is not unconditionally excluded: narrow the root or use an explicit
  glob if dependency/runtime contents are irrelevant to the query.
