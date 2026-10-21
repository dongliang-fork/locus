//! Claude Grep's schema/pagination adapter. Traversal and file searching belong
//! to the same engine as the ordinary Locus grep tool.
use crate::tool::builtins::search_engine::{self, MatchMode, SearchOptions};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn run(root: &str, input: &Value) -> Result<Value, String> {
    run_with_threads(root, input, search_engine::default_threads())
}

pub(crate) fn run_with_threads(root: &str, input: &Value, threads: usize) -> Result<Value, String> {
    let pattern = input["pattern"].as_str().ok_or("Search requires pattern")?;
    let mode = input["output_mode"]
        .as_str()
        .unwrap_or("files_with_matches");
    if !["files_with_matches", "content", "count"].contains(&mode) {
        return Err("Invalid Grep output_mode".into());
    }
    let offset = input["offset"].as_u64().unwrap_or(0).min(1_000_000) as usize;
    let requested_limit = input["head_limit"].as_u64().unwrap_or(200);
    let limit = if requested_limit == 0 {
        10_000
    } else {
        requested_limit.min(10_000) as usize
    };
    let context = input["-C"].as_u64().or_else(|| input["context"].as_u64());
    let before = context
        .or_else(|| input["-B"].as_u64())
        .unwrap_or(0)
        .min(1000) as usize;
    let after = context
        .or_else(|| input["-A"].as_u64())
        .unwrap_or(0)
        .min(1000) as usize;
    let numbered = input["-n"].as_bool().unwrap_or(true);
    let result = search_engine::search(&SearchOptions {
        root: Path::new(root),
        pattern,
        glob: input["glob"].as_str(),
        file_type: input["type"].as_str(),
        case_insensitive: input["-i"].as_bool().unwrap_or(false),
        dot_matches_new_line: input["multiline"].as_bool().unwrap_or(false),
        mode: MatchMode::Occurrences {
            content: mode == "content",
            before,
            after,
            retain_rows: (offset + limit).min(100_000),
        },
        threads,
    })?;
    let total_files = result.files.len();
    let matches: usize = result.files.iter().map(|file| file.matches).sum();
    let total_lines = match mode {
        "content" => result.files.iter().map(|file| file.total_lines).sum(),
        "count" => total_files,
        _ => 0,
    };
    let mut filenames = Vec::new();
    let mut rows = Vec::new();
    let mut preceding_rows = 0;
    for (index, file) in result.files.into_iter().enumerate() {
        let path = file.path.to_string_lossy().into_owned();
        if mode == "files_with_matches" {
            if index >= offset && filenames.len() < limit {
                filenames.push(path);
            }
        } else if mode == "count" {
            if index >= offset && rows.len() < limit {
                rows.push(format!("{path}:{}", file.matches));
                filenames.push(path);
            }
        } else {
            let skip = offset.saturating_sub(preceding_rows);
            let previous_len = rows.len();
            for line in file.lines.into_iter().skip(skip).take(limit - rows.len()) {
                let sep = if line.is_match { ":" } else { "-" };
                rows.push(if numbered {
                    format!("{path}{sep}{}{sep}{}", line.number, line.text)
                } else {
                    format!("{path}{sep}{}", line.text)
                });
            }
            if rows.len() > previous_len {
                filenames.push(path);
            }
            preceding_rows += file.total_lines;
        }
    }
    // Counts remain complete even for a one-row page. Do not silently replace
    // them with the number of matches observed before an early stop.
    Ok(
        json!({"mode":mode,"numFiles":filenames.len(),"filenames":filenames,"content":rows.join("\n"),
        "numLines":rows.len(),"numMatches":matches,"totalFiles":total_files,"totalLines":total_lines,
        "appliedLimit":limit,"appliedOffset":offset,"scannedFiles":result.scanned_files}),
    )
}

#[cfg(test)]
#[path = "native_grep_tests.rs"]
mod tests;
