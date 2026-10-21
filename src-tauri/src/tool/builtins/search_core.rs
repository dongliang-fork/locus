use super::search_engine::{self, MatchMode, SearchOptions};
use std::collections::BinaryHeap;
use std::path::Path;

pub(super) use super::search_engine::should_skip_generated_root_entry;

pub(crate) struct GrepConfig<'a> {
    pattern: &'a str,
    search_path: &'a str,
    include: Option<&'a str>,
    working_dir: Option<&'a str>,
    max_line_length: usize,
    limit: usize,
    scan_budget: usize,
    threads: usize,
}

impl<'a> GrepConfig<'a> {
    #[cfg(test)]
    pub(crate) fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    pub(crate) fn production(
        pattern: &'a str,
        search_path: &'a str,
        include: Option<&'a str>,
        working_dir: Option<&'a str>,
    ) -> Self {
        let limit = 100usize;
        Self {
            pattern,
            search_path,
            include,
            working_dir,
            max_line_length: 500,
            limit,
            scan_budget: limit.saturating_mul(10),
            threads: available_parallelism(),
        }
    }
}

struct GrepMatch {
    rel_path: String,
    line_num: u64,
    line_text: String,
}

impl PartialEq for GrepMatch {
    fn eq(&self, other: &Self) -> bool {
        self.line_num == other.line_num && self.rel_path == other.rel_path
    }
}

impl Eq for GrepMatch {}

impl Ord for GrepMatch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rel_path
            .cmp(&other.rel_path)
            .then(self.line_num.cmp(&other.line_num))
    }
}

impl PartialOrd for GrepMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn run_grep(config: GrepConfig<'_>) -> Result<String, String> {
    let search_root_path = crate::tool::path::resolve_workspace_path(
        config.working_dir.map(Path::new),
        config.search_path,
    )?;
    let result = search_engine::search(&SearchOptions {
        root: &search_root_path,
        pattern: config.pattern,
        glob: config.include,
        file_type: None,
        case_insensitive: false,
        dot_matches_new_line: false,
        mode: MatchMode::Lines {
            per_file_limit: config.limit,
            max_line_length: config.max_line_length,
            scan_budget: config.scan_budget,
        },
        threads: config.threads,
    })?;
    let base_path = config
        .working_dir
        .and_then(|dir| dunce::canonicalize(Path::new(dir)).ok())
        .unwrap_or_else(|| {
            dunce::canonicalize(&search_root_path).unwrap_or_else(|_| search_root_path.clone())
        });
    let can_strip_lexically = dunce::canonicalize(&search_root_path)
        .ok()
        .is_some_and(|canonical| canonical == search_root_path);
    let mut heap = BinaryHeap::<GrepMatch>::new();
    let mut total_seen = 0;
    let mut heap_overflowed = false;
    for file in result.files {
        let path = &file.path;
        let relative_path = can_strip_lexically
            .then(|| path.strip_prefix(&base_path).ok().map(Path::to_path_buf))
            .flatten()
            .or_else(|| {
                dunce::canonicalize(path).ok().and_then(|absolute| {
                    absolute
                        .strip_prefix(&base_path)
                        .ok()
                        .map(Path::to_path_buf)
                })
            })
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| path.display().to_string().replace('\\', "/"));
        total_seen += file.lines.len();
        for line in file.lines {
            let item = GrepMatch {
                rel_path: relative_path.clone(),
                line_num: line.number as u64,
                line_text: line.text,
            };
            if heap.len() < config.limit {
                heap.push(item);
            } else {
                heap_overflowed = true;
                if heap.peek().is_some_and(|largest| item < *largest) {
                    heap.pop();
                    heap.push(item);
                }
            }
        }
    }
    let final_matches = heap.into_sorted_vec();
    if final_matches.is_empty() {
        return Ok("No matches found".to_string());
    }

    let shown = final_matches.len();
    let early = result.early_stopped;
    let file_was_capped = result.file_capped;
    let more_exist = early || file_was_capped || heap_overflowed || total_seen > shown;
    let total_label = if file_was_capped || early {
        format!("{}+", total_seen)
    } else {
        total_seen.to_string()
    };

    let header = if early {
        format!(
            "Found {} matches; search STOPPED EARLY at the scan budget — showing {} of an incomplete subset",
            total_label, shown
        )
    } else if more_exist {
        format!(
            "Found {} matches (showing first {} by path)",
            total_label, shown
        )
    } else {
        format!("Found {} matches", total_seen)
    };
    let mut output = vec![header];
    let mut current_file = String::new();
    for matched in &final_matches {
        if current_file != matched.rel_path {
            current_file = matched.rel_path.clone();
            output.push(format!("\n{}:", matched.rel_path));
        }
        output.push(format!("  {}:{}", matched.line_num, matched.line_text));
    }

    if early {
        output.push(format!(
            "\n⚠ Incomplete & non-deterministic: the search stopped after {} matches without visiting every file, so these are NOT guaranteed to be the globally first matches by path and may differ between runs. Narrow `pattern`, `path`, or `include` (or search a more specific subdirectory) for complete, stable results.",
            config.scan_budget
        ));
    } else if more_exist {
        output.push(format!(
            "\n({} of {} shown — the first by path. Narrow `pattern`, `path`, or `include` to see the rest.)",
            shown, total_label
        ));
    }

    Ok(output.join("\n"))
}

fn available_parallelism() -> usize {
    search_engine::default_threads()
}
