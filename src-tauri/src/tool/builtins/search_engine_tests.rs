use super::*;
use std::collections::{BTreeSet, HashSet};

fn options(root: &Path, mode: MatchMode, threads: usize) -> SearchOptions<'_> {
    SearchOptions {
        root,
        pattern: "needle",
        glob: None,
        file_type: None,
        case_insensitive: false,
        dot_matches_new_line: false,
        mode,
        threads,
    }
}

#[test]
fn parallel_workers_preserve_native_order_and_counts() {
    let root = tempfile::tempdir().unwrap();
    for directory in 0..8 {
        let dir = root.path().join(format!("dir-{directory}"));
        std::fs::create_dir(&dir).unwrap();
        for file in 0..32 {
            std::fs::write(dir.join(format!("{file:03}.py")), "needle needle\nend\n").unwrap();
        }
    }
    let mode = MatchMode::Occurrences {
        content: true,
        before: 0,
        after: 1,
        retain_rows: 4,
    };
    let serial = search(&options(root.path(), mode, 1)).unwrap();
    let parallel = search(&options(root.path(), mode, 4)).unwrap();
    assert_eq!(serial.scanned_files, 256);
    assert_eq!(parallel.scanned_files, serial.scanned_files);
    assert!(parallel.workers_used > 1);
    let projection = |result: SearchResult| {
        result
            .files
            .into_iter()
            .map(|f| {
                (
                    f.path,
                    f.matches,
                    f.total_lines,
                    f.lines
                        .into_iter()
                        .map(|l| (l.number, l.text, l.is_match))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(projection(serial), projection(parallel));
}

#[test]
fn adapters_keep_their_filtering_and_streaming_contracts() {
    let root = tempfile::tempdir().unwrap();
    for dir in ["Library", "BuildPlayer", "runtime", ".hidden", "source"] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
        std::fs::write(root.path().join(dir).join("a.py"), "needle\n").unwrap();
    }
    let lines = search(&options(
        root.path(),
        MatchMode::Lines {
            per_file_limit: 100,
            max_line_length: 500,
            scan_budget: 1000,
        },
        4,
    ))
    .unwrap();
    assert_eq!(lines.files.len(), 2); // runtime is not implicitly excluded.
    let native = search(&options(
        root.path(),
        MatchMode::Occurrences {
            content: false,
            before: 0,
            after: 0,
            retain_rows: 0,
        },
        4,
    ))
    .unwrap();
    assert_eq!(native.files.len(), 5);
    let explicit = search(&options(
        &root.path().join("Library"),
        MatchMode::Lines {
            per_file_limit: 100,
            max_line_length: 500,
            scan_budget: 1000,
        },
        4,
    ))
    .unwrap();
    assert_eq!(explicit.files.len(), 1);
}

#[test]
fn ordinary_scan_budget_and_utf8_line_limit_remain_bounded() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        std::fs::write(
            root.path().join(format!("{index}.txt")),
            "needle中文\n".repeat(150),
        )
        .unwrap();
    }
    let result = search(&options(
        root.path(),
        MatchMode::Lines {
            per_file_limit: 100,
            max_line_length: 8,
            scan_budget: 1000,
        },
        4,
    ))
    .unwrap();
    assert!(result.early_stopped && result.file_capped);
    assert!(result
        .files
        .iter()
        .all(|f| f.lines.len() == 100 && f.lines[0].text == "needle..."));
    assert!(
        result.files.len() <= 13,
        "budget plus at most one file per in-flight worker"
    );
}

#[test]
fn native_size_binary_and_output_guards_apply_in_the_parallel_engine() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("text.py"), "needle\n").unwrap();
    std::fs::write(root.path().join("binary.py"), b"needle\n\0").unwrap();
    let large = std::fs::File::create(root.path().join("large.py")).unwrap();
    large.set_len(NATIVE_MAX_BYTES + 1).unwrap();
    let mode = MatchMode::Occurrences {
        content: true,
        before: 0,
        after: 0,
        retain_rows: 2,
    };
    let result = search(&options(root.path(), mode, 4)).unwrap();
    assert_eq!(result.visited_files, 3);
    assert_eq!(result.scanned_files, 1);
    assert_eq!(result.files.len(), 1);
    std::fs::write(
        root.path().join("dense.py"),
        "needle\n".repeat(NATIVE_MAX_ROWS + 1),
    )
    .unwrap();
    assert!(search(&options(root.path(), mode, 4))
        .unwrap_err()
        .contains("100000 lines"));
}

#[test]
fn ordinary_brace_globs_and_explicit_file_worker_limit_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.py", "b.txt", "c.md"] {
        std::fs::write(root.path().join(name), "needle\n").unwrap();
    }
    let mut request = options(
        root.path(),
        MatchMode::Lines {
            per_file_limit: 100,
            max_line_length: 500,
            scan_budget: 1000,
        },
        4,
    );
    request.glob = Some("*.{py, txt}");
    assert_eq!(search(&request).unwrap().files.len(), 2);
    let single = root.path().join("a.py");
    request.root = &single;
    request.threads = 16;
    let result = search(&request).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.workers_used, 1);
}

#[test]
fn occurrence_line_mapping_matches_the_previous_whole_file_semantics() {
    // Small differential oracle for edge cases; the quadratic implementation is
    // intentionally confined to this test, never used by a benchmark/production.
    fn reference(
        text: &str,
        regex: &regex::Regex,
        before: usize,
        after: usize,
    ) -> (usize, Vec<(usize, String, bool)>) {
        let lines: Vec<_> = text.lines().collect();
        let mut selected = BTreeSet::new();
        let mut matching = HashSet::new();
        let mut count = 0;
        for m in regex.find_iter(text) {
            count += 1;
            let first = text[..m.start()].bytes().filter(|&b| b == b'\n').count();
            let last = text[..m.end()].bytes().filter(|&b| b == b'\n').count();
            matching.extend(first..=last);
            selected.extend(
                first.saturating_sub(before)..=(last + after).min(lines.len().saturating_sub(1)),
            );
        }
        (
            count,
            selected
                .into_iter()
                .filter_map(|line| {
                    lines
                        .get(line)
                        .map(|text| (line + 1, text.to_string(), matching.contains(&line)))
                })
                .collect(),
        )
    }
    for text in [
        "",
        "\n",
        "needle\n",
        "前\r\nneedle needle\r\n后",
        "start\nneedle\nend\n",
        "a\n\nneedle\n\nb",
    ] {
        for pattern in [
            "needle",
            "^",
            "$",
            "",
            r"\b",
            "needle\n",
            "start.*end",
            r"\A",
            r"\z",
        ] {
            let regex = regex::RegexBuilder::new(pattern)
                .multi_line(true)
                .dot_matches_new_line(true)
                .build()
                .unwrap();
            for (before, after) in [(0, 0), (0, 2), (2, 0), (2, 2)] {
                let (count, expected) = reference(text, &regex, before, after);
                for retain in [0, 1, 3, 200] {
                    let (actual_count, total, rows) =
                        occurrence_lines(text, &regex, before, after, retain).unwrap();
                    let actual: Vec<_> = rows
                        .into_iter()
                        .map(|l| (l.number, l.text, l.is_match))
                        .collect();
                    assert_eq!(actual_count, count, "{text:?} / {pattern:?}");
                    assert_eq!(total, expected.len(), "{text:?} / {pattern:?}");
                    assert_eq!(
                        actual,
                        expected.iter().take(retain).cloned().collect::<Vec<_>>(),
                        "{text:?} / {pattern:?}"
                    );
                }
            }
        }
    }
}
