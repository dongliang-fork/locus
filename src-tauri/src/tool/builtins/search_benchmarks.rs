//! Opt-in release benchmarks. Fixture creation is outside timed regions;
//! caches are deliberately not flushed and no wall-clock threshold gates CI.
use super::*;
use crate::llm::claude_code_cli::native_grep;
use crate::tool::builtins::search_core::{run_grep, GrepConfig};
use serde_json::{json, Value};
use std::time::Instant;

fn measure(name: &str, mut run: impl FnMut(usize) -> Value) {
    let parallel = default_threads();
    std::println!(
        "GREP_BENCH_BEGIN {}",
        json!({"case":name,"parallelThreads":parallel})
    );
    let started = Instant::now();
    let baseline = run(1); // Explicit warm-up and correctness baseline.
    std::println!(
        "GREP_BENCH_WARMUP {}",
        json!({"case":name,"threads":1,"ms":started.elapsed().as_secs_f64()*1000.0})
    );
    let started = Instant::now();
    assert_eq!(
        run(parallel),
        baseline,
        "{name}: serial/parallel result parity"
    );
    std::println!(
        "GREP_BENCH_WARMUP {}",
        json!({"case":name,"threads":parallel,"ms":started.elapsed().as_secs_f64()*1000.0})
    );
    let mut serial_ms = Vec::new();
    let mut parallel_ms = Vec::new();
    // Alternate ordering to reduce filesystem/Defender cache-order bias.
    for round in 0..3 {
        for threads in if round % 2 == 0 {
            [1, parallel]
        } else {
            [parallel, 1]
        } {
            let started = Instant::now();
            let result = std::hint::black_box(run(threads));
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(result, baseline);
            std::println!(
                "GREP_BENCH_SAMPLE {}",
                json!({"case":name,"round":round+1,"threads":threads,"ms":ms})
            );
            if threads == 1 {
                serial_ms.push(ms);
            } else {
                parallel_ms.push(ms);
            }
        }
    }
    // On a single-core machine both lanes intentionally run with one worker.
    if parallel == 1 {
        parallel_ms = serial_ms.split_off(3);
    }
    serial_ms.sort_by(f64::total_cmp);
    parallel_ms.sort_by(f64::total_cmp);
    std::println!(
        "GREP_BENCH {}",
        json!({"case":name,"cache":"warm/uncontrolled OS cache",
        "parallelThreads":parallel,"serialMs":serial_ms,"parallelMs":parallel_ms,
        "serialMedianMs":serial_ms[1],"parallelMedianMs":parallel_ms[1],
        "speedup":serial_ms[1]/parallel_ms[1],
        "scannedFiles":baseline["scannedFiles"],"numMatches":baseline["numMatches"],"numLines":baseline["numLines"]})
    );
}

#[test]
#[ignore = "manual grep benchmark: release, --ignored --nocapture --test-threads=1"]
fn grep_benchmark_synthetic() {
    let root = tempfile::tempdir().unwrap();
    let small = root.path().join("small");
    let dense = root.path().join("dense");
    std::fs::create_dir_all(&small).unwrap();
    std::fs::create_dir_all(&dense).unwrap();
    let files = std::env::var("LOCUS_GREP_BENCH_FILES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4096)
        .clamp(32, 20_000);
    let padding = "# ordinary source line with no match\n".repeat(128);
    for index in 0..files {
        let dir = small.join(format!("module-{:02}", index % 32));
        std::fs::create_dir_all(&dir).unwrap();
        let hit = if index % 17 == 0 {
            "needle needle\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join(format!("file-{index:06}.py")),
            format!("{padding}{hit}{padding}"),
        )
        .unwrap();
    }
    let dense_file = dense.join("dense.py");
    std::fs::write(&dense_file, "needle needle needle\n".repeat(20_000)).unwrap();
    std::println!(
        "GREP_BENCH_FIXTURE {}",
        json!({"files":files,"smallFileBytes":padding.len()*2,"denseLines":20000})
    );
    for (name, path, input) in [
        (
            "native-small-content",
            &small,
            json!({"pattern":"needle","glob":"*.py","output_mode":"content"}),
        ),
        (
            "native-small-miss",
            &small,
            json!({"pattern":"ABSENT_TOKEN","glob":"*.py","output_mode":"content"}),
        ),
        (
            "native-small-files",
            &small,
            json!({"pattern":"needle","glob":"*.py"}),
        ),
        (
            "native-small-count",
            &small,
            json!({"pattern":"needle","output_mode":"count"}),
        ),
        (
            "native-dense-content-page",
            &dense,
            json!({"pattern":"needle","output_mode":"content","head_limit":20,"offset":10000,"-C":2}),
        ),
        ("native-dense-files", &dense, json!({"pattern":"needle"})),
        (
            "native-explicit-file-page",
            &dense_file,
            json!({"pattern":"needle","output_mode":"content","head_limit":20,"offset":10000}),
        ),
    ] {
        measure(name, |threads| {
            native_grep::run_with_threads(path.to_str().unwrap(), &input, threads).unwrap()
        });
    }
    measure("ordinary-small-miss", |threads| {
        json!(run_grep(
            GrepConfig::production("ABSENT_TOKEN", small.to_str().unwrap(), Some("*.py"), None)
                .with_threads(threads)
        )
        .unwrap())
    });
    // The ordinary broad-match path intentionally has an incomplete,
    // non-deterministic subset; validate its budget rather than output equality.
    for threads in [1, default_threads()] {
        let started = Instant::now();
        let result = search(&SearchOptions {
            root: &small,
            pattern: "ordinary",
            glob: Some("*.py"),
            file_type: None,
            case_insensitive: false,
            dot_matches_new_line: false,
            mode: MatchMode::Lines {
                per_file_limit: 100,
                max_line_length: 500,
                scan_budget: 1000,
            },
            threads,
        })
        .unwrap();
        assert!(result.early_stopped);
        std::println!(
            "GREP_BENCH {}",
            json!({"case":"ordinary-broad-budget","threads":threads,
            "elapsedMs":started.elapsed().as_secs_f64()*1000.0,"scannedFiles":result.scanned_files,
            "earlyStopped":result.early_stopped,"workersUsed":result.workers_used})
        );
    }
}

#[test]
#[ignore = "optional read-only real-tree benchmark; set LOCUS_GREP_BENCH_ROOT explicitly"]
fn grep_benchmark_external() {
    let Ok(root) = std::env::var("LOCUS_GREP_BENCH_ROOT") else {
        std::println!("Set LOCUS_GREP_BENCH_ROOT to opt into a real-tree read-only benchmark.");
        return;
    };
    let pattern = std::env::var("LOCUS_GREP_BENCH_PATTERN")
        .unwrap_or_else(|_| "conditioning_cache|ConditioningLoader|conditioning_name".into());
    let glob = std::env::var("LOCUS_GREP_BENCH_GLOB").unwrap_or_else(|_| "*.py".into());
    measure("native-external", |threads| {
        native_grep::run_with_threads(
            &root,
            &json!({"pattern":pattern,"glob":glob,"output_mode":"content"}),
            threads,
        )
        .unwrap()
    });
}
