use super::*;

#[test]
fn native_grep_modes_are_deterministic_across_thread_counts_and_pages() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..32 {
        std::fs::write(
            root.path().join(format!("{index:02}.py")),
            "before\nNeedle needle\nafter\nneedle\n",
        )
        .unwrap();
    }
    let root = root.path().to_str().unwrap();
    for mode in ["content", "count", "files_with_matches"] {
        for offset in [0, 1, 3, 12, 127, 128, 1000] {
            let input = json!({"pattern":"needle", "-i":true,"output_mode":mode,"offset":offset,"head_limit":3,"-C":1});
            let serial = run_with_threads(root, &input, 1).unwrap();
            for _ in 0..3 {
                assert_eq!(run_with_threads(root, &input, 4).unwrap(), serial);
            }
            assert_eq!(serial["numMatches"], 96);
            assert_eq!(serial["totalFiles"], 32);
            assert_eq!(serial["scannedFiles"], 32);
            assert!(serial["numFiles"].as_u64().unwrap() <= 3);
        }
    }
}

#[test]
fn native_grep_paginates_context_across_files_without_duplicates() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.py"), "before\nneedle\nneedle\nafter\n").unwrap();
    std::fs::write(root.path().join("b.py"), "before\nneedle\nafter\n").unwrap();
    let result = run(
        root.path().to_str().unwrap(),
        &json!({
            "pattern":"needle","output_mode":"content","context":1,"offset":3,"head_limit":3
        }),
    )
    .unwrap();
    let lines: Vec<_> = result["content"].as_str().unwrap().lines().collect();
    assert_eq!(result["totalLines"], 7);
    assert_eq!(result["numLines"], 3);
    assert_eq!(result["numFiles"], 2);
    assert!(lines[0].ends_with("a.py-4-after"));
    assert!(lines[1].ends_with("b.py-1-before"));
    assert!(lines[2].ends_with("b.py:2:needle"));
}

#[test]
fn native_grep_keeps_types_globs_multiline_counts_and_lossy_text() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("sub")).unwrap();
    std::fs::write(
        root.path().join("sub/a.py"),
        "START\r\n中文 needle needle\r\nEND\r\n",
    )
    .unwrap();
    std::fs::write(root.path().join("b.txt"), b"needle\xff\n").unwrap();
    let path = root.path().to_str().unwrap();
    let result = run(path, &json!({"pattern":"start.*END","multiline":true,"-i":true,"type":"py","output_mode":"content","-n":false})).unwrap();
    assert_eq!(result["numMatches"], 1);
    assert_eq!(result["numLines"], 3);
    assert!(result["content"]
        .as_str()
        .unwrap()
        .contains("a.py:中文 needle needle"));
    assert_eq!(
        run(path, &json!({"pattern":"start.*END","-i":true})).unwrap()["numFiles"],
        0
    );
    let excluded = run(
        path,
        &json!({"pattern":"needle","glob":"!sub/**","output_mode":"content"}),
    )
    .unwrap();
    assert_eq!(excluded["scannedFiles"], 1);
    assert!(excluded["content"].as_str().unwrap().contains("needle�"));
    let counted = run(
        path,
        &json!({"pattern":"needle","glob":"*.{py,txt}","output_mode":"count","head_limit":0}),
    )
    .unwrap();
    assert_eq!(counted["numMatches"], 3);
    assert_eq!(counted["numFiles"], 2);
    assert_eq!(counted["appliedLimit"], 10000);
    let literal_newline = run(
        path,
        &json!({"pattern":"START\\r\\n中文","output_mode":"content"}),
    )
    .unwrap();
    assert_eq!(literal_newline["numMatches"], 1);
}

#[test]
fn native_grep_rejects_invalid_arguments_and_handles_no_matches() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_str().unwrap();
    for input in [
        json!({}),
        json!({"pattern":"["}),
        json!({"pattern":"a","output_mode":"unknown"}),
        json!({"pattern":"a","glob":"["}),
        json!({"pattern":"a","type":"unknown-type"}),
    ] {
        assert!(run(path, &input).is_err(), "{input}");
    }
    let result = run(path, &json!({"pattern":"needle","head_limit":1})).unwrap();
    assert_eq!(result["numFiles"], 0);
    assert_eq!(result["numMatches"], 0);
    assert_eq!(result["filenames"], json!([]));
}
