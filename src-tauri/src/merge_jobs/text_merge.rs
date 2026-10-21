//! Three-way line merge for code and plain text, in the destination's terms.
//!
//! Source edits are applied onto the destination's own lines, so unchanged
//! regions keep their exact bytes and line endings. Line comparison ignores
//! CRLF/LF differences: a Windows working file that only differs from its Git
//! blob by line endings is unchanged. Like Git, edits that overlap or touch
//! (adjacent lines, or an insertion at the boundary of another edit) conflict.
use serde_json::{json, Value};

const PREVIEW_BYTES: usize = 2000;
const MAX_HUNKS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The source made no line change relative to its base.
    Unchanged,
    /// The destination already contains every source change.
    AlreadyApplied,
    /// The destination had no line change; the source applies directly.
    SourceOnly(Vec<u8>),
    /// Both sides changed different lines; the result combines them.
    Merged(Vec<u8>),
    /// Overlapping changes. `marked` holds diff3 conflict markers.
    Conflict { hunks: Vec<Value>, marked: Vec<u8> },
}

impl Outcome {
    pub(super) fn bytes(&self, target: &[u8]) -> Option<Vec<u8>> {
        match self {
            Outcome::Unchanged | Outcome::AlreadyApplied => Some(target.to_vec()),
            Outcome::SourceOnly(bytes) | Outcome::Merged(bytes) => Some(bytes.clone()),
            Outcome::Conflict { .. } => None,
        }
    }
}

struct Lines<'a> {
    raw: Vec<&'a str>,
    normalized: String,
}

fn lines(text: &str) -> Lines<'_> {
    let raw: Vec<&str> = text.split_inclusive('\n').collect();
    let normalized = raw
        .iter()
        .map(|line| line.strip_suffix("\r\n").map(|l| format!("{l}\n")).unwrap_or_else(|| (*line).to_owned()))
        .collect();
    Lines { raw, normalized }
}

/// One side's change in base line coordinates: `[start, end)` replaced by
/// side lines `[side_start, side_end)`.
#[derive(Debug, Clone, Copy)]
struct Edit {
    start: usize,
    end: usize,
    side_start: usize,
    side_end: usize,
}

fn edits(base: &str, side: &str) -> Vec<Edit> {
    similar::TextDiff::from_lines(base, side)
        .ops()
        .iter()
        .filter(|op| op.tag() != similar::DiffTag::Equal)
        .map(|op| {
            let old = op.old_range();
            let new = op.new_range();
            Edit { start: old.start, end: old.end, side_start: new.start, side_end: new.end }
        })
        .collect()
}

/// The destination's dominant line ending, used for lines taken from Git blobs.
fn newline(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

fn with_newline(line: &str, newline: &str) -> String {
    match line.strip_suffix("\r\n").or_else(|| line.strip_suffix('\n')) {
        Some(body) => format!("{body}{newline}"),
        None => line.to_owned(),
    }
}

fn preview(lines: &[&str]) -> String {
    let text: String = lines.concat();
    if text.len() <= PREVIEW_BYTES {
        return text;
    }
    let mut end = PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn as_text<'a>(bytes: &'a [u8], name: &str) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8 text"))
}

pub(super) fn merge(base: &[u8], target: &[u8], source: &[u8]) -> Result<Outcome, String> {
    let (base, target, source) = (as_text(base, "base")?, as_text(target, "destination")?, as_text(source, "source")?);
    let (b, t, s) = (lines(base), lines(target), lines(source));
    if s.normalized == b.normalized {
        return Ok(Outcome::Unchanged);
    }
    if t.normalized == s.normalized {
        return Ok(Outcome::AlreadyApplied);
    }
    let target_edits = edits(&b.normalized, &t.normalized);
    let source_edits = edits(&b.normalized, &s.normalized);
    let eol = newline(target);
    let b_lines: Vec<&str> = b.normalized.split_inclusive('\n').collect();
    let t_norm: Vec<&str> = t.normalized.split_inclusive('\n').collect();
    let s_norm: Vec<&str> = s.normalized.split_inclusive('\n').collect();

    // Cluster edits from both sides whose base ranges overlap or touch.
    #[derive(Default)]
    struct Cluster {
        start: usize,
        end: usize,
        target: Vec<Edit>,
        source: Vec<Edit>,
    }
    let mut all: Vec<(bool, Edit)> = target_edits
        .iter()
        .map(|e| (true, *e))
        .chain(source_edits.iter().map(|e| (false, *e)))
        .collect();
    all.sort_by_key(|(is_target, e)| (e.start, e.end, !*is_target));
    let mut clusters: Vec<Cluster> = Vec::new();
    for (is_target, edit) in all {
        match clusters.last_mut() {
            Some(cluster) if edit.start <= cluster.end => {
                cluster.end = cluster.end.max(edit.end);
                if is_target { cluster.target.push(edit) } else { cluster.source.push(edit) }
            }
            _ => {
                let mut cluster = Cluster { start: edit.start, end: edit.end, ..Default::default() };
                if is_target { cluster.target.push(edit) } else { cluster.source.push(edit) }
                clusters.push(cluster);
            }
        }
    }
    // Side line range covering a cluster: the side's lines between the mapped
    // cluster boundaries, given that side's edits inside the cluster.
    fn side_range(cluster_start: usize, cluster_end: usize, offset: isize, edits: &[Edit]) -> (usize, usize) {
        let start = (cluster_start as isize + offset) as usize;
        let delta: isize = edits
            .iter()
            .map(|e| (e.side_end - e.side_start) as isize - (e.end - e.start) as isize)
            .sum();
        let end = (cluster_end as isize + offset + delta) as usize;
        (start, end)
    }

    let mut out = String::with_capacity(target.len() + source.len() / 4);
    let mut marked = String::new();
    let mut hunks = Vec::new();
    let (mut base_pos, mut t_offset, mut s_offset) = (0usize, 0isize, 0isize);
    let mut t_pos = 0usize;
    let mut source_applied = false;
    for cluster in &clusters {
        // Unchanged region: destination lines keep their exact bytes.
        let t_start = (cluster.start as isize + t_offset) as usize;
        for line in &t.raw[t_pos..t_start] {
            out.push_str(line);
            marked.push_str(line);
        }
        let (ts, te) = side_range(cluster.start, cluster.end, t_offset, &cluster.target);
        let (ss, se) = side_range(cluster.start, cluster.end, s_offset, &cluster.source);
        let both = !cluster.target.is_empty() && !cluster.source.is_empty();
        if both && t_norm[ts..te] != s_norm[ss..se] {
            if hunks.len() < MAX_HUNKS {
                hunks.push(json!({
                    "base_lines": [cluster.start + 1, cluster.end + 1],
                    "target_lines": [ts + 1, te + 1],
                    "source_lines": [ss + 1, se + 1],
                    "base": preview(&b_lines[cluster.start..cluster.end]),
                    "target": preview(&t.raw[ts..te]),
                    "source": preview(&s_norm[ss..se]),
                }));
            }
            let close = |text: &mut String| {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push_str(eol);
                }
            };
            marked.push_str(&format!("<<<<<<< destination{eol}"));
            for line in &t.raw[ts..te] {
                marked.push_str(line);
            }
            close(&mut marked);
            marked.push_str(&format!("||||||| base{eol}"));
            for line in &b_lines[cluster.start..cluster.end] {
                marked.push_str(&with_newline(line, eol));
            }
            close(&mut marked);
            marked.push_str(&format!("======={eol}"));
            for line in &s_norm[ss..se] {
                marked.push_str(&with_newline(line, eol));
            }
            close(&mut marked);
            marked.push_str(&format!(">>>>>>> source{eol}"));
            for line in &t.raw[ts..te] {
                out.push_str(line);
            }
        } else if cluster.source.is_empty() || both {
            for line in &t.raw[ts..te] {
                out.push_str(line);
                marked.push_str(line);
            }
        } else {
            source_applied = true;
            for line in &s_norm[ss..se] {
                let line = with_newline(line, eol);
                out.push_str(&line);
                marked.push_str(&line);
            }
        }
        base_pos = cluster.end;
        t_offset = te as isize - base_pos as isize;
        s_offset = se as isize - base_pos as isize;
        t_pos = te;
    }
    let _ = base_pos;
    for line in &t.raw[t_pos..] {
        out.push_str(line);
        marked.push_str(line);
    }
    if !hunks.is_empty() {
        return Ok(Outcome::Conflict { hunks, marked: marked.into_bytes() });
    }
    if !source_applied {
        return Ok(Outcome::AlreadyApplied);
    }
    Ok(if target_edits.is_empty() {
        Outcome::SourceOnly(out.into_bytes())
    } else {
        Outcome::Merged(out.into_bytes())
    })
}

/// Catalog classification of a both-present text change.
pub(super) fn classify(base: &[u8], target: &[u8], source: &[u8]) -> (&'static str, Value) {
    match merge(base, target, source) {
        Ok(Outcome::Unchanged | Outcome::AlreadyApplied) => (
            "already_applied",
            json!({"reason":"the destination already contains these source lines (line endings ignored)"}),
        ),
        Ok(Outcome::SourceOnly(_)) => ("clean", json!({"merge":"source_lines"})),
        Ok(Outcome::Merged(_)) => (
            "clean",
            json!({"merge":"line_merge","reason":"the destination and the source changed different lines; include applies a three-way line merge"}),
        ),
        Ok(Outcome::Conflict { hunks, .. }) => (
            "conflict",
            json!({"reason":format!("{} overlapping line region(s) changed on both sides; inspect_asset(path, version='result') shows conflict markers, then use files.set_text with the resolved content or resolve side='source'/'target'", hunks.len()),"hunks":hunks}),
        ),
        Err(error) => ("conflict", json!({"reason":error})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(base: &str, target: &str, source: &str) -> Outcome {
        merge(base.as_bytes(), target.as_bytes(), source.as_bytes()).unwrap()
    }

    #[test]
    fn independent_lines_merge_and_keep_destination_line_endings() {
        let base = "a\nb\nc\nd\ne\n";
        let target = "A\r\nb\r\nc\r\nd\r\ne\r\n";
        let source = "a\nb\nc\nd\nE\n";
        assert_eq!(
            merged(base, target, source),
            Outcome::Merged("A\r\nb\r\nc\r\nd\r\nE\r\n".as_bytes().to_vec())
        );
    }

    #[test]
    fn line_ending_only_destination_is_unchanged_and_takes_source_lines() {
        let base = "one\ntwo\n";
        let target = "one\r\ntwo\r\n";
        let source = "one\nTWO\n";
        assert_eq!(merged(base, target, source), Outcome::SourceOnly(b"one\r\nTWO\r\n".to_vec()));
        assert_eq!(merged(base, target, base), Outcome::Unchanged);
        assert_eq!(merged(base, "one\r\nTWO\r\n", source), Outcome::AlreadyApplied);
        assert_eq!(classify(base.as_bytes(), target.as_bytes(), source.as_bytes()).0, "clean");
    }

    #[test]
    fn overlapping_and_adjacent_edits_conflict_with_markers() {
        let base = "a\nb\nc\n";
        let Outcome::Conflict { hunks, marked } = merged(base, "a\nB\nc\n", "a\nX\nc\n") else {
            panic!("expected conflict");
        };
        assert_eq!(hunks[0]["base_lines"], json!([2, 3]));
        assert_eq!(
            String::from_utf8(marked).unwrap(),
            "a\n<<<<<<< destination\nB\n||||||| base\nb\n=======\nX\n>>>>>>> source\nc\n"
        );
        // Adjacent changes conflict, as in Git.
        assert!(matches!(merged(base, "A\nb\nc\n", "a\nB\nc\n"), Outcome::Conflict { .. }));
        // Insertions at the same boundary conflict.
        assert!(matches!(merged(base, "a\nT\nb\nc\n", "a\nS\nb\nc\n"), Outcome::Conflict { .. }));
        // Identical edits on both sides are not conflicts.
        assert_eq!(merged(base, "a\nB\nc\n", "a\nB\nc\n"), Outcome::AlreadyApplied);
    }

    #[test]
    fn separated_edits_insertions_and_missing_final_newline_merge() {
        let base = "1\n2\n3\n4\n5\n6";
        let target = "1\n2\n2.5\n3\n4\n5\n6";
        let source = "1\n2\n3\n4\n5\n6\n7\n";
        assert_eq!(
            merged(base, target, source),
            Outcome::Merged(b"1\n2\n2.5\n3\n4\n5\n6\n7\n".to_vec())
        );
    }
}
