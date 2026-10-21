//! Shared file-search scheduler. Tool adapters choose their existing matching
//! and truncation semantics; neither adapter owns a second directory/I/O loop.
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const NATIVE_MAX_FILES: usize = 100_000;
const NATIVE_MAX_BYTES: u64 = 20 * 1024 * 1024;
const NATIVE_MAX_ROWS: usize = 100_000;

pub(crate) fn default_threads() -> usize {
    // Native multiline matching can retain a 20 MiB buffer per worker. Bound
    // fan-out on high-core-count machines, especially while inference is active.
    std::thread::available_parallelism()
        .map_or(4, usize::from)
        .min(16)
}

#[derive(Clone, Copy)]
pub(crate) enum MatchMode {
    /// Locus's line-oriented, bounded streaming search.
    Lines {
        per_file_limit: usize,
        max_line_length: usize,
        scan_budget: usize,
    },
    /// Claude's occurrence counts and whole-file regex semantics (including
    /// explicit newlines). Only content mode needs retained context rows.
    Occurrences {
        content: bool,
        before: usize,
        after: usize,
        retain_rows: usize,
    },
}

pub(crate) struct SearchOptions<'a> {
    pub root: &'a Path,
    pub pattern: &'a str,
    pub glob: Option<&'a str>,
    pub file_type: Option<&'a str>,
    pub case_insensitive: bool,
    pub dot_matches_new_line: bool,
    pub mode: MatchMode,
    pub threads: usize,
}

#[derive(Debug)]
pub(crate) struct MatchLine {
    pub number: usize,
    pub text: String,
    pub is_match: bool,
}

#[derive(Debug)]
pub(crate) struct FileMatches {
    pub path: PathBuf,
    pub lines: Vec<MatchLine>,
    pub matches: usize,
    pub total_lines: usize,
}

#[derive(Debug, Default)]
pub(crate) struct SearchResult {
    pub files: Vec<FileMatches>,
    pub scanned_files: usize,
    pub visited_files: usize,
    pub workers_used: usize,
    pub early_stopped: bool,
    pub file_capped: bool,
}

struct LineSink {
    lines: Vec<MatchLine>,
    limit: usize,
    max_line_length: usize,
    capped: bool,
}
impl Sink for LineSink {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> std::io::Result<bool> {
        let start = mat.line_number().unwrap_or(0) as usize;
        for (offset, raw) in mat.lines().enumerate() {
            let text = String::from_utf8_lossy(raw);
            let trimmed = text.trim();
            let text = if trimmed.len() > self.max_line_length {
                format!(
                    "{}...",
                    &trimmed[..trimmed.floor_char_boundary(self.max_line_length)]
                )
            } else {
                trimmed.to_string()
            };
            self.lines.push(MatchLine {
                number: start + offset,
                text,
                is_match: true,
            });
            if self.lines.len() >= self.limit {
                self.capped = true;
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Native matches are ordered, so both newline counting and context-range
/// merging are linear. Do not rescan text[..match.start()] for every match.
fn occurrence_lines(
    text: &str,
    regex: &regex::Regex,
    before: usize,
    after: usize,
    retain: usize,
) -> Result<(usize, usize, Vec<MatchLine>), String> {
    fn extend(ranges: &mut Vec<(usize, usize)>, first: usize, last: usize) -> usize {
        if let Some((_, end)) = ranges.last_mut() {
            if first <= end.saturating_add(1) {
                let added = last.saturating_sub(*end);
                *end = (*end).max(last);
                return added;
            }
        }
        ranges.push((first, last));
        last - first + 1
    }
    let line_count = text.lines().count();
    let mut matching = Vec::new();
    let mut selected = Vec::new();
    let (mut cursor, mut line, mut count, mut total_rows) = (0, 0, 0, 0);
    for m in regex.find_iter(text) {
        count += 1;
        line += text[cursor..m.start()]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        let first = line;
        line += text[m.start()..m.end()]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        cursor = m.end();
        extend(&mut matching, first, line);
        let from = first.saturating_sub(before);
        let to = (line + after).min(line_count.saturating_sub(1));
        if from <= to && line_count > 0 {
            total_rows += extend(&mut selected, from, to);
            if total_rows > NATIVE_MAX_ROWS {
                return Err("Search output exceeded 100000 lines; narrow the query".into());
            }
        }
    }
    let mut rows = Vec::new();
    let (mut selected_index, mut match_index) = (0, 0);
    for (number, text) in text.lines().enumerate() {
        if rows.len() >= retain {
            break;
        }
        while selected_index < selected.len() && selected[selected_index].1 < number {
            selected_index += 1;
        }
        if selected_index == selected.len() {
            break;
        }
        if number < selected[selected_index].0 {
            continue;
        }
        while match_index < matching.len() && matching[match_index].1 < number {
            match_index += 1;
        }
        let is_match = matching
            .get(match_index)
            .is_some_and(|&(first, last)| first <= number && number <= last);
        rows.push(MatchLine {
            number: number + 1,
            text: text.into(),
            is_match,
        });
    }
    Ok((count, total_rows, rows))
}

/// Shared parallel traversal, filtering, worker lifecycle and aggregation.
/// Native occurrence matching deliberately retains its bounded whole-file
/// representation: changing it to line regexes would break newline patterns,
/// occurrence counts and context pagination. Locus keeps its streaming sink.
pub(crate) fn search(options: &SearchOptions<'_>) -> Result<SearchResult, String> {
    let native = matches!(options.mode, MatchMode::Occurrences { .. });
    let single_file = options.root.is_file();
    let mut builder = ignore::WalkBuilder::new(options.root);
    builder
        .hidden(!native)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .threads(if single_file {
            1
        } else {
            options.threads.max(1)
        });
    let base = if single_file {
        options.root.parent().unwrap_or(options.root)
    } else {
        options.root
    };
    if let Some(glob) = options.glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(base);
        let patterns = if native {
            vec![glob.to_string()]
        } else {
            ordinary_glob_patterns(glob)
        };
        for pattern in patterns {
            if let Err(e) = overrides.add(&pattern) {
                if native {
                    return Err(e.to_string());
                }
            }
        }
        builder.overrides(overrides.build().map_err(|e| e.to_string())?);
    }
    if let Some(kind) = options.file_type {
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults().select(kind);
        builder.types(types.build().map_err(|e| e.to_string())?);
    }
    let line_regex = if native {
        None
    } else {
        Some(Arc::new(RegexMatcher::new(options.pattern).map_err(
            |e| format!("Invalid regex pattern '{}': {e}", options.pattern),
        )?))
    };
    let occurrence_regex = if native {
        Some(
            regex::RegexBuilder::new(options.pattern)
                .case_insensitive(options.case_insensitive)
                .multi_line(true)
                .dot_matches_new_line(options.dot_matches_new_line)
                .build()
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let state = Mutex::new((SearchResult::default(), None::<String>));
    let stopped = AtomicBool::new(false);
    let visited = AtomicUsize::new(0);
    let scanned = AtomicUsize::new(0);
    let workers = AtomicUsize::new(0);
    let total_rows = AtomicUsize::new(0);
    let fail = |error: String| {
        let mut state = state.lock().unwrap();
        if state.1.is_none() {
            state.1 = Some(error);
        }
        stopped.store(true, Ordering::Relaxed);
    };
    builder.build_parallel().run(|| {
        let mut counted_worker = false;
        let workers = &workers;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        let line_regex = line_regex.clone();
        let occurrence_regex = occurrence_regex.clone();
        let mut buffer = Vec::new();
        let fail = &fail;
        let stopped = &stopped;
        let visited = &visited;
        let scanned = &scanned;
        let total_rows = &total_rows;
        let state = &state;
        Box::new(move |entry| {
            if stopped.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    if native {
                        fail(e.to_string());
                        return ignore::WalkState::Quit;
                    }
                    return ignore::WalkState::Continue;
                }
            };
            let path = entry.path();
            if !native && should_skip_generated_root_entry(options.root, path) {
                return if entry.file_type().is_some_and(|t| t.is_dir()) {
                    ignore::WalkState::Skip
                } else {
                    ignore::WalkState::Continue
                };
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            if !counted_worker {
                workers.fetch_add(1, Ordering::Relaxed);
                counted_worker = true;
            }
            if visited.fetch_add(1, Ordering::Relaxed) >= NATIVE_MAX_FILES && native {
                fail("Search exceeded 100000 files; narrow the path or glob".into());
                return ignore::WalkState::Quit;
            }
            let (file, capped) = match options.mode {
                MatchMode::Lines {
                    per_file_limit,
                    max_line_length,
                    ..
                } => {
                    if is_binary_path(path) {
                        return ignore::WalkState::Continue;
                    }
                    let mut sink = LineSink {
                        lines: Vec::new(),
                        limit: per_file_limit,
                        max_line_length,
                        capped: false,
                    };
                    scanned.fetch_add(1, Ordering::Relaxed);
                    if searcher
                        .search_path(&**line_regex.as_ref().unwrap(), path, &mut sink)
                        .is_err()
                        || sink.lines.is_empty()
                    {
                        return ignore::WalkState::Continue;
                    }
                    (
                        FileMatches {
                            path: path.into(),
                            matches: sink.lines.len(),
                            total_lines: sink.lines.len(),
                            lines: sink.lines,
                        },
                        sink.capped,
                    )
                }
                MatchMode::Occurrences {
                    content,
                    before,
                    after,
                    retain_rows,
                } => {
                    // Check size before allocating/reading. take() also bounds
                    // files that grow between metadata and the actual read.
                    let read = || -> std::io::Result<Option<std::fs::File>> {
                        let file = std::fs::File::open(path)?;
                        Ok((file.metadata()?.len() <= NATIVE_MAX_BYTES).then_some(file))
                    };
                    let file = match read() {
                        Ok(Some(file)) => file,
                        Ok(None) => return ignore::WalkState::Continue,
                        Err(e) => {
                            fail(format!("{}: {e}", path.display()));
                            return ignore::WalkState::Quit;
                        }
                    };
                    buffer.clear();
                    if let Err(e) = file.take(NATIVE_MAX_BYTES + 1).read_to_end(&mut buffer) {
                        fail(format!("{}: {e}", path.display()));
                        return ignore::WalkState::Quit;
                    }
                    if buffer.len() as u64 > NATIVE_MAX_BYTES || buffer.contains(&0) {
                        return ignore::WalkState::Continue;
                    }
                    scanned.fetch_add(1, Ordering::Relaxed);
                    let text = String::from_utf8_lossy(&buffer);
                    let regex = occurrence_regex.as_ref().unwrap();
                    let (matches, rows, lines) = if content {
                        match occurrence_lines(&text, regex, before, after, retain_rows) {
                            Ok(result) => result,
                            Err(e) => {
                                fail(e);
                                return ignore::WalkState::Quit;
                            }
                        }
                    } else {
                        (regex.find_iter(&text).count(), 0, Vec::new())
                    };
                    if matches == 0 {
                        return ignore::WalkState::Continue;
                    }
                    (
                        FileMatches {
                            path: path.into(),
                            matches,
                            total_lines: rows,
                            lines,
                        },
                        false,
                    )
                }
            };
            let rows = total_rows.fetch_add(file.total_lines, Ordering::Relaxed) + file.total_lines;
            if native && rows > NATIVE_MAX_ROWS {
                fail("Search output exceeded 100000 lines; narrow the query".into());
                return ignore::WalkState::Quit;
            }
            let budget_reached =
                matches!(options.mode, MatchMode::Lines { scan_budget, .. } if rows >= scan_budget);
            if budget_reached {
                stopped.store(true, Ordering::Relaxed);
            }
            let mut state = state.lock().unwrap();
            state.0.file_capped |= capped;
            state.0.files.push(file);
            state.0.early_stopped |= budget_reached;
            if budget_reached {
                ignore::WalkState::Quit
            } else {
                ignore::WalkState::Continue
            }
        })
    });
    let (mut result, error) = state.into_inner().unwrap();
    if let Some(error) = error {
        return Err(error);
    }
    result.files.sort_by(|a, b| a.path.cmp(&b.path));
    result.scanned_files = scanned.load(Ordering::Relaxed);
    result.visited_files = visited.load(Ordering::Relaxed);
    result.workers_used = workers.load(Ordering::Relaxed);
    Ok(result)
}

fn ordinary_glob_patterns(pattern: &str) -> Vec<String> {
    // The ordinary adapter historically trims each brace-list alternative.
    if let Some(start) = pattern.find('{') {
        if let Some(end) = pattern[start + 1..].find('}').map(|end| end + start + 1) {
            return pattern[start + 1..end]
                .split(',')
                .map(|part| {
                    format!(
                        "{}{}{}",
                        &pattern[..start],
                        part.trim(),
                        &pattern[end + 1..]
                    )
                })
                .collect();
        }
    }
    vec![pattern.to_string()]
}

pub(super) fn should_skip_generated_root_entry(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let Some(first) = relative.components().next() else {
        return false;
    };
    let name = first.as_os_str().to_string_lossy();
    let name = name.trim();
    [
        "library",
        "temp",
        "obj",
        "logs",
        "usersettings",
        "memorycaptures",
        "recordings",
    ]
    .iter()
    .any(|generated| name.eq_ignore_ascii_case(generated))
        || name
            .get(.."build".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("build"))
}

fn is_binary_path(path: &Path) -> bool {
    let extensions = [
        "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "7z", "bin", "wasm", "pyc", "pdf",
        "png", "jpg", "jpeg", "gif", "webp", "mp4", "mp3", "mov",
    ];
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        extensions
            .iter()
            .any(|binary| e.eq_ignore_ascii_case(binary))
    })
}

#[cfg(test)]
#[path = "search_benchmarks.rs"]
mod benchmarks;
#[cfg(test)]
#[path = "search_engine_tests.rs"]
mod tests;
