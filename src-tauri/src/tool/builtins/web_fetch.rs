//! `web_fetch`: fetch a public http(s) URL and return agent-readable text.
//!
//! `http` performs the request (redirects, destination checks, size limit,
//! diagnosable errors), `decode` classifies the content and picks the
//! charset, and `html` renders markdown or plain text. This module parses the
//! arguments, follows `<meta refresh>` redirect shells, pages long content
//! by character `offset`, and keeps recent documents so continuation reads
//! return the same text without refetching.

mod decode;
mod html;
mod http;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use url::Url;

use super::{make_exec, ToolDef, ToolResult};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MAX_CHARS: usize = 50_000;
const MAX_MAX_CHARS: usize = 200_000;
const MAX_REFRESH_HOPS: usize = 3;
const MAX_REFRESH_DELAY_SECS: u64 = 10;
/// A page with more text than this is content, not a redirect shell.
const REFRESH_SHELL_CHARS: usize = 2000;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_MAX_ENTRIES: usize = 16;
const CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Markdown,
    Html,
    Text,
}

impl Format {
    fn parse(value: Option<&serde_json::Value>) -> Result<Self, String> {
        match value.and_then(|value| value.as_str()).unwrap_or("markdown") {
            "markdown" => Ok(Self::Markdown),
            "html" => Ok(Self::Html),
            "text" => Ok(Self::Text),
            other => Err(format!(
                "Unsupported format {other:?}; use \"markdown\", \"text\" or \"html\"."
            )),
        }
    }

    fn accept(self) -> &'static str {
        match self {
            Self::Markdown => {
                "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
            }
            Self::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            Self::Html => {
                "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, */*;q=0.1"
            }
        }
    }
}

struct Request {
    url: Url,
    format: Format,
    timeout: Duration,
    max_chars: usize,
    offset: usize,
}

fn parse_request(args: &serde_json::Value) -> Result<Request, String> {
    let raw_url = args
        .get("url")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("");
    if raw_url.is_empty() {
        return Err("Missing required parameter: url.".to_string());
    }
    let url = Url::parse(raw_url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .ok_or_else(|| format!("URL must start with http:// or https:// (got {raw_url:?})."))?;
    let format = Format::parse(args.get("format"))?;
    let timeout_secs = args
        .get("timeout")
        .and_then(|value| value.as_f64())
        .filter(|secs| secs.is_finite())
        .map_or(DEFAULT_TIMEOUT_SECS, |secs| {
            (secs.ceil().max(1.0) as u64).min(MAX_TIMEOUT_SECS)
        });
    let max_chars = args
        .get("maxChars")
        .or_else(|| args.get("max_chars"))
        .and_then(|value| value.as_u64())
        .map_or(DEFAULT_MAX_CHARS, |chars| {
            (chars as usize).clamp(1, MAX_MAX_CHARS)
        });
    let offset = args
        .get("offset")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as usize;
    Ok(Request {
        url,
        format,
        timeout: Duration::from_secs(timeout_secs),
        max_chars,
        offset,
    })
}

// ─── Documents ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Document {
    requested_url: Url,
    final_url: Url,
    title: Option<String>,
    content_type: String,
    encoding_note: Option<String>,
    notes: Vec<String>,
    content: String,
    char_count: usize,
}

struct Processed {
    content: String,
    title: Option<String>,
    encoding_note: Option<String>,
    refresh: Option<(u64, Url)>,
    needs_javascript: bool,
    content_root: Option<&'static str>,
    visible_chars: usize,
    simplified: bool,
}

/// Decodes and renders one response body (CPU-bound; runs off the async
/// workers).
fn process_body(
    body: &[u8],
    content_type: &str,
    url: &Url,
    format: Format,
) -> Result<Processed, String> {
    let decoded = decode::decode(body, content_type, url.host_str()).map_err(|description| {
        http::unsupported_message(
            url,
            description,
            &decode::mime_type(content_type),
            Some(body.len() as u64),
        )
    })?;
    let encoding_note = decoded.encoding_note();
    if decoded.kind != decode::BodyKind::Html {
        let content = decoded.text.trim_end().to_string();
        let visible_chars = content.chars().filter(|c| !c.is_whitespace()).count();
        return Ok(Processed {
            content,
            title: None,
            encoding_note,
            refresh: None,
            needs_javascript: false,
            content_root: None,
            visible_chars,
            simplified: false,
        });
    }
    let style = match format {
        Format::Html => None,
        Format::Markdown => Some(html::Style::Markdown),
        Format::Text => Some(html::Style::Text),
    };
    let page = html::render(&decoded.text, url, style);
    let content = match style {
        None => decoded.text,
        Some(_) => page.content,
    };
    Ok(Processed {
        content,
        title: page.title,
        encoding_note,
        refresh: page.refresh,
        needs_javascript: page.needs_javascript,
        content_root: page.content_root,
        visible_chars: page.visible_chars,
        simplified: page.simplified,
    })
}

async fn fetch_document(
    request: &Request,
    client: reqwest::Client,
    allow_private: bool,
) -> Result<Document, String> {
    let options = http::FetchOptions {
        client,
        accept: request.format.accept(),
        deadline: tokio::time::Instant::now() + request.timeout,
        timeout: request.timeout,
        allow_private,
    };
    let mut url = request.url.clone();
    let mut notes = Vec::new();
    let mut refresh_hops = 0;
    loop {
        let response = http::fetch(&url, &options).await?;
        notes.extend(response.notes);
        let final_url = response.final_url;
        let content_type = response.content_type;
        let body = response.body;
        let format = request.format;
        let (processed, final_url, content_type) = tokio::task::spawn_blocking(move || {
            let processed = process_body(&body, &content_type, &final_url, format);
            (processed, final_url, content_type)
        })
        .await
        .map_err(|error| format!("Processing the page failed: {error}"))?;
        let processed = processed?;

        if let Some((delay, target)) = &processed.refresh {
            let follow = *delay <= MAX_REFRESH_DELAY_SECS
                && refresh_hops < MAX_REFRESH_HOPS
                && processed.visible_chars < REFRESH_SHELL_CHARS
                && *target != final_url
                && matches!(target.scheme(), "http" | "https");
            if follow {
                notes.push(format!(
                    "Followed a <meta refresh> redirect from {final_url}."
                ));
                url = target.clone();
                refresh_hops += 1;
                continue;
            }
        }

        if processed.simplified {
            notes.push(
                "The page's markup is nested too deeply to parse safely; showing its plain text."
                    .to_string(),
            );
        }
        if processed.needs_javascript {
            notes.push(
                "The page appears to build its content with JavaScript, which web_fetch does not run; the text may be incomplete. Look for another source if it is.".to_string(),
            );
        }
        if let Some(root) = processed.content_root {
            notes.push(format!(
                "Content taken from the page's <{root}> element; navigation, sidebars and footers are omitted (format \"html\" returns the full page)."
            ));
        }
        let char_count = processed.content.chars().count();
        return Ok(Document {
            requested_url: request.url.clone(),
            final_url,
            title: processed.title,
            content_type,
            encoding_note: processed.encoding_note,
            notes,
            content: processed.content,
            char_count,
        });
    }
}

// ─── Continuation cache ─────────────────────────────────────────────────────

struct CacheEntry {
    format: Format,
    fetched_at: Instant,
    document: Arc<Document>,
}

static CACHE: Mutex<Vec<CacheEntry>> = Mutex::new(Vec::new());

fn cache_lookup(url: &Url, format: Format) -> Option<(Arc<Document>, Duration)> {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|entry| entry.fetched_at.elapsed() < CACHE_TTL);
    cache
        .iter()
        .rev()
        .find(|entry| {
            entry.format == format
                && (entry.document.requested_url == *url || entry.document.final_url == *url)
        })
        .map(|entry| (entry.document.clone(), entry.fetched_at.elapsed()))
}

fn cache_store(document: Arc<Document>, format: Format) {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|entry| {
        entry.fetched_at.elapsed() < CACHE_TTL
            && !(entry.format == format && entry.document.requested_url == document.requested_url)
    });
    cache.push(CacheEntry {
        format,
        fetched_at: Instant::now(),
        document,
    });
    let mut bytes: usize = cache.iter().map(|entry| entry.document.content.len()).sum();
    while cache.len() > CACHE_MAX_ENTRIES || (bytes > CACHE_MAX_BYTES && cache.len() > 1) {
        bytes -= cache.remove(0).document.content.len();
    }
}

// ─── Output ─────────────────────────────────────────────────────────────────

fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

fn render_output(
    document: &Document,
    offset: usize,
    max_chars: usize,
    cached_for: Option<Duration>,
) -> Result<String, String> {
    if offset > 0 && offset >= document.char_count {
        return Err(format!(
            "offset {offset} is past the end of the content ({} characters).",
            document.char_count
        ));
    }
    let content = &document.content;
    let start = byte_index(content, offset);
    let mut end = byte_index(content, offset.saturating_add(max_chars));
    if end < content.len() {
        // Prefer to stop at a line break in the last fifth of the window.
        let window = &content[start..end];
        if let Some(newline) = window.rfind('\n') {
            if newline >= window.len() * 4 / 5 {
                end = start + newline + 1;
            }
        }
    }
    let slice = content[start..end].trim_end();
    let end_chars = offset + content[start..end].chars().count();

    let mut output = format!("URL: {}\n", document.final_url);
    if document.final_url != document.requested_url {
        output.push_str(&format!("Redirected from: {}\n", document.requested_url));
    }
    if let Some(title) = &document.title {
        output.push_str(&format!("Title: {title}\n"));
    }
    let content_type = if document.content_type.is_empty() {
        "unknown"
    } else {
        document.content_type.as_str()
    };
    match &document.encoding_note {
        Some(note) => output.push_str(&format!("Content-Type: {content_type}; {note}\n")),
        None => output.push_str(&format!("Content-Type: {content_type}\n")),
    }
    for note in &document.notes {
        output.push_str(&format!("Note: {note}\n"));
    }
    if let Some(age) = cached_for {
        output.push_str(&format!(
            "Cached: fetched {}s ago; continuation reads reuse it for 15 minutes.\n",
            age.as_secs()
        ));
    }
    output.push('\n');
    if document.char_count == 0 {
        output.push_str("(The page has no readable text content.)");
        return Ok(output);
    }
    output.push_str(slice);
    if end_chars < document.char_count {
        output.push_str(&format!(
            "\n\n[Showing characters {offset}-{end_chars} of {}. Call web_fetch with the same url and offset={end_chars} to read more.]",
            document.char_count
        ));
    } else if offset > 0 {
        output.push_str(&format!(
            "\n\n[Showing characters {offset}-{end_chars} of {}; end of content.]",
            document.char_count
        ));
    }
    Ok(output)
}

// ─── Tool ───────────────────────────────────────────────────────────────────

async fn execute(
    request: Request,
    client: Option<reqwest::Client>,
    allow_private: bool,
) -> Result<String, String> {
    if request.offset > 0 {
        if let Some((document, age)) = cache_lookup(&request.url, request.format) {
            return render_output(&document, request.offset, request.max_chars, Some(age));
        }
    }
    let client = match client {
        Some(client) => client,
        None => http::build_client()?,
    };
    let document = Arc::new(fetch_document(&request, client, allow_private).await?);
    cache_store(document.clone(), request.format);
    render_output(&document, request.offset, request.max_chars, None)
}

async fn cancelled(mut rx: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub(super) fn web_fetch() -> ToolDef {
    let prompt = crate::prompt::parse_tool_prompt(crate::prompt::tools::WEB_FETCH);
    ToolDef {
        name: "web_fetch".to_string(),
        description: prompt.description,
        parameters: prompt.parameters,
        mutates_workspace: false,
        execute: make_exec(|args, ctx| {
            Box::pin(async move {
                let request = match parse_request(&args) {
                    Ok(request) => request,
                    Err(message) => {
                        return ToolResult {
                            output: message,
                            is_error: true,
                        }
                    }
                };
                let work = execute(request, None, false);
                let result = match ctx.cancel_rx.clone() {
                    Some(rx) => tokio::select! {
                        result = work => result,
                        _ = cancelled(rx) => Err("web_fetch was cancelled.".to_string()),
                    },
                    None => work.await,
                };
                match result {
                    Ok(output) => ToolResult {
                        output,
                        is_error: false,
                    },
                    Err(output) => ToolResult {
                        output,
                        is_error: true,
                    },
                }
            })
        }),
    }
}
