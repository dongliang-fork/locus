//! HTML to markdown or plain text for `web_fetch`.
//!
//! Every DOM walk here uses an explicit stack. The recursive html2md walker
//! this replaces overflowed the 2 MiB tokio worker stack at roughly 5000
//! nesting levels, which aborts the whole process. The renderer also drops
//! page noise (scripts, navigation, hidden nodes, inline `data:` images and
//! form controls), prefers `<main>`/`<article>` when they hold most of the
//! text, and resolves links and images against the page URL.

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Style {
    Markdown,
    Text,
}

#[derive(Debug, Default)]
pub(super) struct RenderedPage {
    pub title: Option<String>,
    /// Empty when only page metadata was requested.
    pub content: String,
    /// `<meta http-equiv="refresh">` delay in seconds and target.
    pub refresh: Option<(u64, Url)>,
    /// Little visible text plus script/noscript markers: the content is
    /// probably rendered by JavaScript, which web_fetch does not run.
    pub needs_javascript: bool,
    /// Set when the content was narrowed to this element.
    pub content_root: Option<&'static str>,
    /// Non-whitespace characters of rendered (or renderable) text.
    pub visible_chars: usize,
    /// The markup was too deeply nested to parse; the content is the
    /// page's plain text from a linear tag strip.
    pub simplified: bool,
}

/// Open block containers beyond this depth mean pathological markup:
/// html5ever's scope check for each block start tag walks every open
/// element, so parsing such a page is quadratic (minutes of CPU for a few
/// MiB of nested `<div>`s).
const MAX_CONTAINER_DEPTH: usize = 2_000;

/// Renders `html` in `style`, or only collects metadata when `style` is
/// `None` (raw HTML output).
pub(super) fn render(html: &str, page_url: &Url, style: Option<Style>) -> RenderedPage {
    if container_depth(html) > MAX_CONTAINER_DEPTH {
        let content = style.map_or(String::new(), |_| plain_text_fallback(html));
        let visible_chars = content.chars().filter(|c| !c.is_whitespace()).count();
        return RenderedPage {
            content,
            visible_chars,
            simplified: true,
            ..RenderedPage::default()
        };
    }
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let document = dom.document.clone();
    let base = find_first(&document, |node| {
        element_name(node) == Some("base") && attr(node, "href").is_some()
    })
    .and_then(|node| page_url.join(attr(&node, "href")?.trim()).ok())
    .unwrap_or_else(|| page_url.clone());

    let title = find_first(&document, |node| element_name(node) == Some("title"))
        .map(|node| collapse_whitespace(&text_content(&node)))
        .filter(|title| !title.is_empty());
    let refresh = meta_refresh(&document, &base);
    let has_scripts = find_first(&document, |node| {
        matches!(element_name(node), Some("script" | "noscript"))
    })
    .is_some();
    let body = find_first(&document, |node| element_name(node) == Some("body"))
        .unwrap_or_else(|| document.clone());

    let mut page = RenderedPage {
        title,
        refresh,
        ..RenderedPage::default()
    };
    let visible_chars = match style {
        Some(style) => {
            let (root, root_name) = match content_root(&body) {
                Some((root, name)) => (root, Some(name)),
                None => (body.clone(), None),
            };
            let mut writer = Writer::new(style, &base, root_name.is_some());
            writer.walk(&root);
            page.content = writer.finish();
            page.content_root = root_name;
            page.content.chars().filter(|c| !c.is_whitespace()).count()
        }
        None => visible_text_len(&body),
    };
    let page_text = if style.is_some() {
        page.content.to_lowercase()
    } else {
        String::new()
    };
    page.needs_javascript = (visible_chars < 200 && has_scripts)
        || (visible_chars < 2000 && mentions_javascript_requirement(&page_text));
    page.visible_chars = visible_chars;
    page
}

fn mentions_javascript_requirement(text: &str) -> bool {
    [
        "enable javascript",
        "javascript is required",
        "javascript is disabled",
        "requires javascript",
        "turn on javascript",
        "启用 javascript",
        "启用javascript",
        "开启 javascript",
        "开启javascript",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

// ─── Pathological markup ────────────────────────────────────────────────────

/// Start tags whose insertion runs html5ever's "p in button scope" check.
fn is_scope_checked_container(name: &[u8]) -> bool {
    const NAMES: &[&[u8]] = &[
        b"address",
        b"article",
        b"aside",
        b"blockquote",
        b"center",
        b"details",
        b"dialog",
        b"dir",
        b"div",
        b"dl",
        b"fieldset",
        b"figcaption",
        b"figure",
        b"footer",
        b"form",
        b"header",
        b"hgroup",
        b"main",
        b"menu",
        b"nav",
        b"ol",
        b"section",
        b"summary",
        b"ul",
        b"table",
    ];
    NAMES
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

fn is_raw_text_element(name: &[u8]) -> bool {
    [
        b"script".as_slice(),
        b"style",
        b"textarea",
        b"title",
        b"xmp",
        b"noscript",
    ]
    .iter()
    .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Byte offset of the first case-insensitive `needle` at or after `from`.
fn find_ascii_ci(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&index| haystack[index..index + needle.len()].eq_ignore_ascii_case(needle))
}

/// Linear estimate of the deepest nesting of scope-checked containers.
fn container_depth(html: &str) -> usize {
    let bytes = html.as_bytes();
    let (mut index, mut depth, mut deepest) = (0usize, 0usize, 0usize);
    while let Some(offset) = bytes[index..].iter().position(|&b| b == b'<') {
        index += offset + 1;
        if bytes[index..].starts_with(b"!--") {
            match find_ascii_ci(bytes, b"-->", index) {
                Some(end) => index = end + 3,
                None => break,
            }
            continue;
        }
        let closing = bytes.get(index) == Some(&b'/');
        let name_start = index + usize::from(closing);
        let name_end = bytes[name_start..]
            .iter()
            .position(|b| !b.is_ascii_alphanumeric())
            .map_or(bytes.len(), |len| name_start + len);
        let name = &bytes[name_start..name_end];
        index = name_end;
        if name.is_empty() {
            continue;
        }
        if !closing && is_raw_text_element(name) {
            let mut close = b"</".to_vec();
            close.extend_from_slice(name);
            match find_ascii_ci(bytes, &close, index) {
                Some(end) => index = end,
                None => break,
            }
            continue;
        }
        if is_scope_checked_container(name) {
            if closing {
                depth = depth.saturating_sub(1);
            } else {
                depth += 1;
                deepest = deepest.max(depth);
            }
        }
    }
    deepest
}

fn decode_entity(entity: &str) -> Option<char> {
    if let Some(number) = entity.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '©',
        "reg" => '®',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "middot" => '·',
        "laquo" => '«',
        "raquo" => '»',
        _ => return None,
    })
}

fn push_decoded_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..]
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| Some((decode_entity(&rest[1..1 + end])?, end + 2)));
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

/// Linear tag strip for pages too deeply nested to parse: skips raw-text
/// elements and comments, breaks lines at block tags, decodes entities.
fn plain_text_fallback(html: &str) -> String {
    const LINE_TAGS: &[&[u8]] = &[
        b"p",
        b"div",
        b"br",
        b"li",
        b"tr",
        b"h1",
        b"h2",
        b"h3",
        b"h4",
        b"h5",
        b"h6",
        b"section",
        b"article",
        b"table",
        b"ul",
        b"ol",
        b"blockquote",
        b"pre",
        b"hr",
        b"dd",
        b"dt",
    ];
    let bytes = html.as_bytes();
    let mut raw = String::with_capacity(html.len() / 2);
    let mut index = 0;
    while let Some(offset) = bytes[index..].iter().position(|&b| b == b'<') {
        push_decoded_text(&mut raw, &html[index..index + offset]);
        index += offset + 1;
        if bytes[index..].starts_with(b"!--") {
            match find_ascii_ci(bytes, b"-->", index) {
                Some(end) => index = end + 3,
                None => return finish_plain_text(raw),
            }
            continue;
        }
        let closing = bytes.get(index) == Some(&b'/');
        let name_start = index + usize::from(closing);
        let name_end = bytes[name_start..]
            .iter()
            .position(|b| !b.is_ascii_alphanumeric())
            .map_or(bytes.len(), |len| name_start + len);
        let name = bytes[name_start..name_end].to_vec();
        match bytes[name_end..].iter().position(|&b| b == b'>') {
            Some(end) => index = name_end + end + 1,
            None => return finish_plain_text(raw),
        }
        if !closing && (is_raw_text_element(&name) || name.eq_ignore_ascii_case(b"svg")) {
            let mut close = b"</".to_vec();
            close.extend_from_slice(&name);
            match find_ascii_ci(bytes, &close, index) {
                Some(end) => {
                    index = bytes[end..]
                        .iter()
                        .position(|&b| b == b'>')
                        .map_or(bytes.len(), |gt| end + gt + 1);
                }
                None => return finish_plain_text(raw),
            }
            continue;
        }
        if LINE_TAGS.iter().any(|tag| tag.eq_ignore_ascii_case(&name)) {
            raw.push('\n');
        }
    }
    push_decoded_text(&mut raw, &html[index..]);
    finish_plain_text(raw)
}

fn finish_plain_text(raw: String) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in raw.lines() {
        let line = collapse_whitespace(line);
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        out.push_str(&line);
        blank = false;
    }
    out
}

// ─── DOM helpers ────────────────────────────────────────────────────────────

fn element_name(node: &Handle) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(&name.local),
        _ => None,
    }
}

fn attr(node: &Handle, name: &str) -> Option<String> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|attribute| &*attribute.name.local == name)
            .map(|attribute| attribute.value.to_string()),
        _ => None,
    }
}

fn has_attr(node: &Handle, name: &str) -> bool {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .any(|attribute| &*attribute.name.local == name),
        _ => false,
    }
}

/// First descendant of `root`, in document order, that `matches`.
fn find_first(root: &Handle, matches: impl Fn(&Handle) -> bool) -> Option<Handle> {
    let mut stack: Vec<Handle> = root.children.borrow().iter().rev().cloned().collect();
    while let Some(node) = stack.pop() {
        if matches(&node) {
            return Some(node);
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
    None
}

fn text_content(root: &Handle) -> String {
    let mut text = String::new();
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        if let NodeData::Text { contents } = &node.data {
            text.push_str(&contents.borrow());
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
    text
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn meta_refresh(document: &Handle, base: &Url) -> Option<(u64, Url)> {
    let meta = find_first(document, |node| {
        element_name(node) == Some("meta")
            && attr(node, "http-equiv")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("refresh"))
    })?;
    let content = attr(&meta, "content")?;
    let (delay, rest) = content
        .split_once([';', ','])
        .unwrap_or((content.as_str(), ""));
    let delay = delay
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|d| d.is_finite() && *d >= 0.0)? as u64;
    let rest = rest.trim();
    let target = match (rest.get(..3), rest.get(3..)) {
        (Some(key), Some(value)) if key.eq_ignore_ascii_case("url") => {
            let value = value.trim_start();
            value.strip_prefix('=').unwrap_or(value).trim()
        }
        _ => rest,
    };
    let target = target.trim_matches(|c| c == '"' || c == '\'').trim();
    if target.is_empty() {
        return None;
    }
    Some((delay, base.join(target).ok()?))
}

// ─── Noise and content selection ────────────────────────────────────────────

/// Elements whose subtree never carries readable page content.
fn is_noise_element(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "noscript"
            | "template"
            | "head"
            | "title"
            | "meta"
            | "link"
            | "base"
            | "svg"
            | "math"
            | "canvas"
            | "iframe"
            | "frame"
            | "frameset"
            | "object"
            | "embed"
            | "applet"
            | "video"
            | "audio"
            | "source"
            | "track"
            | "map"
            | "area"
            | "input"
            | "button"
            | "select"
            | "option"
            | "optgroup"
            | "textarea"
            | "datalist"
            | "output"
            | "progress"
            | "meter"
            | "nav"
    )
}

fn is_hidden(node: &Handle) -> bool {
    if has_attr(node, "hidden") {
        return true;
    }
    if attr(node, "aria-hidden").is_some_and(|value| value.trim().eq_ignore_ascii_case("true")) {
        return true;
    }
    attr(node, "style").is_some_and(|style| {
        let style: String = style
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        style.contains("display:none") || style.contains("visibility:hidden")
    })
}

fn has_chrome_role(node: &Handle) -> bool {
    attr(node, "role").is_some_and(|role| {
        role.split_whitespace().any(|role| {
            matches!(
                role.to_ascii_lowercase().as_str(),
                "navigation"
                    | "banner"
                    | "contentinfo"
                    | "complementary"
                    | "search"
                    | "menu"
                    | "menubar"
                    | "toolbar"
            )
        })
    })
}

fn is_content_element(node: &Handle, name: &str) -> bool {
    name == "main"
        || name == "article"
        || attr(node, "role").is_some_and(|role| role.trim().eq_ignore_ascii_case("main"))
}

/// Site chrome that only counts as noise outside `<main>`/`<article>`
/// (an article's own `<header>` holds its title and byline).
fn is_outer_chrome(node: &Handle, name: &str) -> bool {
    if matches!(name, "header" | "footer" | "aside") {
        return true;
    }
    // Common widget containers named by id/class, matched on the leading
    // word so "sidebar-left" matches but "content-with-sidebar" does not.
    ["id", "class"].iter().any(|key| {
        attr(node, key).is_some_and(|value| {
            value.split_whitespace().any(|token| {
                let lead = token
                    .split(['-', '_'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                matches!(
                    lead.as_str(),
                    "sidebar" | "breadcrumb" | "breadcrumbs" | "cookie" | "cookies" | "consent"
                )
            })
        })
    })
}

/// Whether to drop `node` entirely; `in_content` is true inside
/// `<main>`/`<article>`.
fn skip_element(node: &Handle, name: &str, in_content: bool) -> bool {
    if is_noise_element(name) {
        return true;
    }
    if name == "dialog" && !has_attr(node, "open") {
        return true;
    }
    if is_hidden(node) || has_chrome_role(node) {
        return true;
    }
    !in_content && is_outer_chrome(node, name)
}

/// Visible text characters (whitespace excluded) the renderer would emit.
fn visible_text_len(root: &Handle) -> usize {
    let start_in_content = element_name(root).is_some_and(|name| is_content_element(root, name));
    let mut total = 0;
    let mut stack: Vec<(Handle, bool)> = root
        .children
        .borrow()
        .iter()
        .map(|child| (child.clone(), start_in_content))
        .collect();
    while let Some((node, in_content)) = stack.pop() {
        match &node.data {
            NodeData::Text { contents } => {
                total += contents
                    .borrow()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .count();
            }
            NodeData::Element { .. } => {
                let name = element_name(&node).unwrap_or("");
                if skip_element(&node, name, in_content) {
                    continue;
                }
                let in_content = in_content || is_content_element(&node, name);
                stack.extend(
                    node.children
                        .borrow()
                        .iter()
                        .map(|child| (child.clone(), in_content)),
                );
            }
            _ => {}
        }
    }
    total
}

/// `<main>` (or `role="main"`), else the dominant top-level `<article>`,
/// when it carries at least a quarter of the page's visible text.
fn content_root(body: &Handle) -> Option<(Handle, &'static str)> {
    let mut main: Option<Handle> = None;
    let mut articles: Vec<Handle> = Vec::new();
    let mut stack: Vec<Handle> = body.children.borrow().iter().rev().cloned().collect();
    while let Some(node) = stack.pop() {
        let Some(name) = element_name(&node) else {
            continue;
        };
        if is_noise_element(name) || is_hidden(&node) {
            continue;
        }
        if name == "main"
            || attr(&node, "role").is_some_and(|role| role.trim().eq_ignore_ascii_case("main"))
        {
            main = Some(node);
            break;
        }
        if name == "article" {
            articles.push(node);
            continue;
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }

    let (candidate, label) = match main {
        Some(main) => (main, "main"),
        None => {
            let lengths: Vec<usize> = articles.iter().map(visible_text_len).collect();
            let total: usize = lengths.iter().sum();
            let (index, largest) = lengths
                .iter()
                .copied()
                .enumerate()
                .max_by_key(|(_, len)| *len)?;
            if largest * 10 < total * 6 {
                return None;
            }
            (articles.swap_remove(index), "article")
        }
    };
    let candidate_len = visible_text_len(&candidate);
    let page_len = visible_text_len(body);
    (candidate_len >= 200 && candidate_len * 4 >= page_len).then_some((candidate, label))
}

// ─── Writer ─────────────────────────────────────────────────────────────────

enum Capture {
    Link {
        href: Option<String>,
        title: Option<String>,
    },
    Wrap(&'static str),
    Code,
    Heading(usize),
    Pre {
        language: Option<String>,
    },
    Cell {
        colspan: usize,
    },
    Caption,
}

struct Frame {
    capture: Capture,
    text: String,
}

enum Prefix {
    Quote,
    Indent(usize),
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    caption: Option<String>,
}

enum Exit {
    Nothing,
    Block(u8),
    Capture,
    List,
    ListItem,
    Quote,
    Table,
    Row,
    Content,
}

enum Step {
    Enter(Handle),
    Exit(Exit),
}

/// Cap on synthetic cells from `colspan`, so a hostile value cannot blow up
/// the output.
const MAX_COLSPAN: usize = 32;

struct Writer<'a> {
    style: Style,
    base: &'a Url,
    out: String,
    frames: Vec<Frame>,
    pending_space: bool,
    pending_breaks: u8,
    prefixes: Vec<Prefix>,
    lists: Vec<(bool, u64)>,
    tables: Vec<Table>,
    pre_depth: usize,
    content_depth: usize,
}

impl<'a> Writer<'a> {
    fn new(style: Style, base: &'a Url, root_is_content: bool) -> Self {
        Self {
            style,
            base,
            out: String::new(),
            frames: Vec::new(),
            pending_space: false,
            pending_breaks: 0,
            prefixes: Vec::new(),
            lists: Vec::new(),
            tables: Vec::new(),
            pre_depth: 0,
            content_depth: usize::from(root_is_content),
        }
    }

    fn markdown(&self) -> bool {
        self.style == Style::Markdown
    }

    fn walk(&mut self, root: &Handle) {
        let mut stack: Vec<Step> = root
            .children
            .borrow()
            .iter()
            .rev()
            .map(|child| Step::Enter(child.clone()))
            .collect();
        while let Some(step) = stack.pop() {
            let node = match step {
                Step::Exit(exit) => {
                    self.exit(exit);
                    continue;
                }
                Step::Enter(node) => node,
            };
            match &node.data {
                NodeData::Text { contents } => self.text(&contents.borrow()),
                NodeData::Element { .. } => {
                    let name = element_name(&node).unwrap_or("").to_string();
                    if skip_element(&node, &name, self.content_depth > 0) {
                        continue;
                    }
                    if let Some(exit) = self.enter(&node, &name) {
                        stack.push(Step::Exit(exit));
                        stack.extend(
                            node.children
                                .borrow()
                                .iter()
                                .rev()
                                .map(|child| Step::Enter(child.clone())),
                        );
                    }
                }
                _ => {}
            }
        }
        while !self.frames.is_empty() {
            self.exit(Exit::Capture);
        }
    }

    fn finish(mut self) -> String {
        let mut out = std::mem::take(&mut self.out);
        let trimmed_len = out.trim_end().len();
        out.truncate(trimmed_len);
        out.trim_start_matches('\n').to_string()
    }

    // ── Output primitives ──

    fn buf(&mut self) -> &mut String {
        match self.frames.last_mut() {
            Some(frame) => &mut frame.text,
            None => &mut self.out,
        }
    }

    fn prefix_string(&self) -> String {
        self.prefixes
            .iter()
            .map(|prefix| match prefix {
                Prefix::Quote => "> ".to_string(),
                Prefix::Indent(width) => " ".repeat(*width),
            })
            .collect()
    }

    /// Writes pending separation before new content.
    fn flush(&mut self) {
        if self.pending_breaks > 0 {
            let breaks = self.pending_breaks;
            self.pending_breaks = 0;
            self.pending_space = false;
            let prefix = self.prefix_string();
            let quote_line = prefix.trim_end().to_string();
            let buf = self.buf();
            if buf.is_empty() {
                buf.push_str(&prefix);
                return;
            }
            let content_len = buf.trim_end_matches([' ', '\t']).len();
            buf.truncate(content_len);
            let existing = buf.len() - buf.trim_end_matches('\n').len();
            if existing == 0 {
                buf.push('\n');
            }
            if breaks >= 2 && existing < 2 {
                buf.push_str(&quote_line);
                buf.push('\n');
            }
            buf.push_str(&prefix);
        } else if self.pending_space {
            self.pending_space = false;
            let depth = self.frames.len();
            // A space at the very start of a capture belongs before it.
            let target = if depth > 0 && self.frames[depth - 1].text.is_empty() {
                match depth {
                    1 => &mut self.out,
                    _ => &mut self.frames[depth - 2].text,
                }
            } else {
                self.buf()
            };
            if !target.is_empty() && !target.ends_with([' ', '\n']) {
                target.push(' ');
            }
        }
    }

    fn block(&mut self, breaks: u8) {
        if self.pre_depth > 0 {
            let buf = self.buf();
            if !buf.is_empty() && !buf.ends_with('\n') {
                buf.push('\n');
            }
            return;
        }
        if !self.frames.is_empty() {
            self.pending_space = true;
            return;
        }
        self.pending_breaks = self.pending_breaks.max(breaks);
    }

    fn line_break(&mut self) {
        if self.pre_depth > 0 {
            self.buf().push('\n');
        } else if !self.frames.is_empty() {
            self.pending_space = true;
        } else if !self.out.is_empty() {
            self.pending_breaks = (self.pending_breaks + 1).min(2);
        }
    }

    fn text(&mut self, raw: &str) {
        if self.pre_depth > 0 {
            self.flush();
            let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
            self.buf().push_str(&normalized);
            return;
        }
        for (index, word) in raw.split(char::is_whitespace).enumerate() {
            if index > 0 {
                self.pending_space = true;
            }
            if word.is_empty() {
                continue;
            }
            self.flush();
            self.buf().push_str(word);
        }
    }

    fn inline(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.flush();
        self.buf().push_str(text);
    }

    /// Multi-line block content (headings, fences, tables).
    fn block_lines(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.frames.is_empty() {
            let flat = collapse_whitespace(text);
            self.inline(&flat);
            return;
        }
        self.block(2);
        self.flush();
        let prefix = self.prefix_string();
        let buf = self.buf();
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                buf.push('\n');
                buf.push_str(&prefix);
            }
            buf.push_str(line);
        }
        self.block(2);
    }

    fn open(&mut self, capture: Capture) {
        self.flush();
        self.frames.push(Frame {
            capture,
            text: String::new(),
        });
    }

    // ── Elements ──

    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        let lower = href.to_ascii_lowercase();
        if href.is_empty()
            || href.starts_with('#')
            || lower.starts_with("javascript:")
            || lower.starts_with("data:")
            || lower.starts_with("vbscript:")
        {
            return None;
        }
        let url = self.base.join(href).ok()?;
        matches!(url.scheme(), "http" | "https" | "mailto" | "ftp").then(|| url.to_string())
    }

    fn image(&mut self, node: &Handle) {
        if !self.markdown() {
            return;
        }
        let alt = collapse_whitespace(&attr(node, "alt").unwrap_or_default());
        if alt.is_empty() {
            return;
        }
        let source = ["src", "data-src", "data-original", "data-lazy-src"]
            .iter()
            .filter_map(|key| attr(node, key))
            .find(|value| {
                let value = value.trim();
                !value.is_empty() && !value.to_ascii_lowercase().starts_with("data:")
            })
            .or_else(|| {
                attr(node, "srcset").and_then(|set| {
                    set.split_whitespace()
                        .next()
                        .map(|first| first.trim_end_matches(',').to_string())
                })
            });
        if let Some(url) = source.and_then(|source| self.resolve(&source)) {
            let image = format!("![{}]({})", escape_label(&alt), escape_destination(&url));
            self.inline(&image);
        }
    }

    fn enter(&mut self, node: &Handle, name: &str) -> Option<Exit> {
        // Inside a code block only line structure matters; markup such as
        // highlighter spans and links stays plain text.
        if self.pre_depth > 0 {
            return match name {
                "br" => {
                    self.line_break();
                    None
                }
                "img" | "wbr" => None,
                "div" | "p" | "li" | "tr" => {
                    self.block(1);
                    Some(Exit::Block(1))
                }
                _ => Some(Exit::Nothing),
            };
        }
        let markdown = self.markdown();
        let in_data_table = self.tables.last().is_some();
        let exit = match name {
            "br" => {
                self.line_break();
                return None;
            }
            "hr" => {
                self.block(2);
                if markdown {
                    self.block_lines("---");
                }
                return None;
            }
            "img" => {
                self.image(node);
                return None;
            }
            "wbr" => return None,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(name.as_bytes()[1] - b'0');
                self.block(2);
                self.open(Capture::Heading(level));
                Exit::Capture
            }
            "pre" | "listing" | "xmp" | "plaintext" => {
                self.block(2);
                let language = code_language(node).or_else(|| {
                    node.children
                        .borrow()
                        .iter()
                        .find(|child| element_name(child) == Some("code"))
                        .and_then(code_language)
                });
                self.open(Capture::Pre { language });
                self.pre_depth += 1;
                Exit::Capture
            }
            "code" | "kbd" | "samp" | "tt" => {
                self.open(Capture::Code);
                Exit::Capture
            }
            "strong" | "b" if markdown => {
                self.open(Capture::Wrap("**"));
                Exit::Capture
            }
            "em" | "i" if markdown => {
                self.open(Capture::Wrap("*"));
                Exit::Capture
            }
            "del" | "s" | "strike" if markdown => {
                self.open(Capture::Wrap("~~"));
                Exit::Capture
            }
            "a" if markdown => {
                let href = attr(node, "href").and_then(|href| self.resolve(&href));
                let title = attr(node, "title")
                    .or_else(|| attr(node, "aria-label"))
                    .map(|title| collapse_whitespace(&title))
                    .filter(|title| !title.is_empty());
                self.open(Capture::Link { href, title });
                Exit::Capture
            }
            "ul" | "ol" | "menu" | "dir" => {
                self.block(if self.lists.is_empty() { 2 } else { 1 });
                let start = attr(node, "start")
                    .and_then(|start| start.trim().parse::<u64>().ok())
                    .unwrap_or(1);
                self.lists.push((name == "ol", start));
                Exit::List
            }
            "li" => {
                self.block(1);
                let marker = match self.lists.last() {
                    Some((true, number)) => format!("{}. ", number),
                    _ => "- ".to_string(),
                };
                self.inline(&marker);
                self.prefixes.push(Prefix::Indent(marker.chars().count()));
                Exit::ListItem
            }
            "blockquote" => {
                self.block(2);
                if markdown && self.frames.is_empty() {
                    self.prefixes.push(Prefix::Quote);
                    Exit::Quote
                } else {
                    Exit::Block(2)
                }
            }
            "table" => {
                let has_nested_table =
                    find_first(node, |child| element_name(child) == Some("table")).is_some();
                if has_nested_table || !self.frames.is_empty() || in_data_table {
                    self.block(1);
                    Exit::Block(1)
                } else {
                    self.block(2);
                    self.tables.push(Table::default());
                    Exit::Table
                }
            }
            "caption" if in_data_table => {
                self.open(Capture::Caption);
                Exit::Capture
            }
            "tr" if in_data_table => {
                if let Some(table) = self.tables.last_mut() {
                    table.row = Some(Vec::new());
                }
                Exit::Row
            }
            "td" | "th" if in_data_table => {
                let colspan = attr(node, "colspan")
                    .and_then(|span| span.trim().parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, MAX_COLSPAN);
                self.open(Capture::Cell { colspan });
                Exit::Capture
            }
            "tr" | "td" | "th" | "caption" | "thead" | "tbody" | "tfoot" => {
                self.block(1);
                Exit::Block(1)
            }
            "main" | "article" => {
                self.block(1);
                self.content_depth += 1;
                Exit::Content
            }
            _ if is_content_element(node, name) => {
                self.block(1);
                self.content_depth += 1;
                Exit::Content
            }
            "p" | "figure" | "details" | "dl" | "address" | "fieldset" => {
                self.block(2);
                Exit::Block(2)
            }
            "div" | "section" | "header" | "footer" | "aside" | "figcaption" | "summary" | "dt"
            | "dd" | "form" | "legend" | "center" | "hgroup" | "search" | "body" | "html"
            | "dialog" => {
                self.block(1);
                Exit::Block(1)
            }
            _ => Exit::Nothing,
        };
        Some(exit)
    }

    fn exit(&mut self, exit: Exit) {
        match exit {
            Exit::Nothing => {}
            Exit::Block(breaks) => self.block(breaks),
            Exit::Capture => self.close_capture(),
            Exit::List => {
                self.lists.pop();
                self.block(if self.lists.is_empty() { 2 } else { 1 });
            }
            Exit::ListItem => {
                self.prefixes.pop();
                if let Some((_, number)) = self.lists.last_mut() {
                    *number += 1;
                }
                self.block(1);
            }
            Exit::Quote => {
                self.prefixes.pop();
                self.block(2);
            }
            Exit::Row => {
                if let Some(table) = self.tables.last_mut() {
                    if let Some(row) = table.row.take() {
                        if row.iter().any(|cell| !cell.is_empty()) {
                            table.rows.push(row);
                        }
                    }
                }
            }
            Exit::Table => {
                if let Some(table) = self.tables.pop() {
                    let rendered = render_table(table, self.style);
                    self.block_lines(&rendered);
                }
            }
            Exit::Content => {
                self.content_depth = self.content_depth.saturating_sub(1);
                self.block(1);
            }
        }
    }

    fn close_capture(&mut self) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let markdown = self.markdown();
        match frame.capture {
            Capture::Pre { language } => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                let code = trim_blank_lines(&frame.text);
                if code.trim().is_empty() {
                    return;
                }
                if markdown {
                    let fence = "`".repeat(longest_run(code, '`').max(2) + 1);
                    let block = format!(
                        "{fence}{}\n{code}\n{fence}",
                        language.as_deref().unwrap_or("")
                    );
                    self.block_lines(&block);
                } else {
                    self.block_lines(code);
                }
            }
            Capture::Heading(level) => {
                let inner = collapse_whitespace(&frame.text);
                if inner.is_empty() {
                    return;
                }
                if markdown && self.frames.is_empty() {
                    self.block_lines(&format!("{} {}", "#".repeat(level), inner));
                } else {
                    self.block_lines(&inner);
                }
            }
            Capture::Code => {
                let inner = frame.text.trim();
                if inner.is_empty() {
                    return;
                }
                if markdown {
                    let fence = "`".repeat(longest_run(inner, '`') + 1);
                    let pad = if inner.starts_with('`') || inner.ends_with('`') {
                        " "
                    } else {
                        ""
                    };
                    self.inline(&format!("{fence}{pad}{inner}{pad}{fence}"));
                } else {
                    self.inline(inner);
                }
            }
            Capture::Wrap(marker) => {
                let inner = frame.text.trim();
                if !inner.is_empty() {
                    self.inline(&format!("{marker}{inner}{marker}"));
                }
            }
            Capture::Link { href, title } => {
                let inner = frame.text.trim();
                let label = if inner.is_empty() {
                    title.unwrap_or_default()
                } else {
                    inner.to_string()
                };
                match href {
                    _ if label.is_empty() => {}
                    Some(href) if href.trim_end_matches('/') == label.trim_end_matches('/') => {
                        self.inline(&href)
                    }
                    // The label is already markdown (it may hold an image),
                    // so its brackets are not escaped.
                    Some(href) => {
                        self.inline(&format!("[{}]({})", label, escape_destination(&href)))
                    }
                    None => self.inline(&label),
                }
            }
            Capture::Cell { colspan } => {
                let mut cell = collapse_whitespace(&frame.text);
                if markdown {
                    cell = cell.replace('|', "\\|");
                }
                if let Some(row) = self.tables.last_mut().and_then(|table| table.row.as_mut()) {
                    row.push(cell);
                    row.extend(std::iter::repeat_n(String::new(), colspan - 1));
                }
            }
            Capture::Caption => {
                let caption = collapse_whitespace(&frame.text);
                if let Some(table) = self.tables.last_mut() {
                    table.caption = (!caption.is_empty()).then_some(caption);
                }
            }
        }
    }
}

fn code_language(node: &Handle) -> Option<String> {
    attr(node, "class")?
        .split_whitespace()
        .find_map(|class| {
            class
                .strip_prefix("language-")
                .or_else(|| class.strip_prefix("lang-"))
        })
        .filter(|language| {
            !language.is_empty()
                && language
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '#' | '_'))
        })
        .map(str::to_string)
}

fn trim_blank_lines(text: &str) -> &str {
    let start = text
        .char_indices()
        .take_while(|(_, c)| c.is_whitespace())
        .filter(|(_, c)| *c == '\n')
        .last()
        .map_or(0, |(index, _)| index + 1);
    text[start..].trim_end()
}

fn longest_run(text: &str, target: char) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for c in text.chars() {
        if c == target {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn escape_label(label: &str) -> String {
    label.replace('[', "\\[").replace(']', "\\]")
}

fn escape_destination(url: &str) -> String {
    url.replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
}

fn render_table(table: Table, style: Style) -> String {
    let Table { rows, caption, .. } = table;
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    if let Some(caption) = caption {
        lines.push(caption);
    }
    if columns <= 1 || style == Style::Text {
        lines.extend(rows.iter().map(|row| {
            row.iter()
                .filter(|cell| !cell.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ")
        }));
        return lines.join("\n");
    }
    let format_row = |row: &[String]| {
        let cells: Vec<&str> = (0..columns)
            .map(|index| row.get(index).map_or("", String::as_str))
            .collect();
        format!("| {} |", cells.join(" | "))
    };
    let mut rows = rows.into_iter();
    if let Some(header) = rows.next() {
        lines.push(format_row(&header));
        lines.push(format!("|{}", " --- |".repeat(columns)));
    }
    lines.extend(rows.map(|row| format_row(&row)));
    lines.join("\n")
}
