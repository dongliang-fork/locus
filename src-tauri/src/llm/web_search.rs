//! Provider-executed (server-side) web search shared by the Anthropic Messages
//! and OpenAI Chat transports.
//!
//! Two request shapes exist: Anthropic's `web_search_20250305` tool and
//! OpenRouter's `openrouter:web_search` server tool. OpenRouter runs the whole
//! search loop inside the request and never sends a `web_search_tool_result`:
//! results arrive only as citations (Messages `citations_delta`, Chat
//! `delta.annotations`). Google-native grounding does not even emit a
//! `server_tool_use` block — only `usage.server_tool_use.web_search_requests`
//! shows that a search ran. The helpers here turn those signals into the same
//! `web_search` server-tool record the Anthropic path already persists, so the
//! UI shows the search and later rounds replay its sources and excerpts as text
//! (`[Web Search Result]`) instead of searching again.

use serde_json::{json, Value};

use crate::commands::CustomEndpointServerTools;
use crate::session::models::{Citation, CitationKind, ServerToolKind, ToolCallInfo};

pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";
pub const OPENROUTER_WEB_SEARCH_TYPE: &str = "openrouter:web_search";

/// `openrouter:web_search` engines Locus forwards; anything else (including
/// `auto`) omits the parameter so OpenRouter applies its own default.
const OPENROUTER_SEARCH_ENGINES: &[&str] =
    &["native", "exa", "parallel", "firecrawl", "perplexity"];

/// Excerpts are replayed into every later request, so keep each one short.
const MAX_EXCERPT_CHARS: usize = 600;
const MAX_HITS_PER_SEARCH: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSearchTool {
    /// Anthropic `web_search_20250305` (official API and compatible gateways).
    AnthropicNative,
    /// OpenRouter `openrouter:web_search`; `engine: None` keeps OpenRouter's
    /// `auto` (provider-native search where available, Exa otherwise).
    OpenRouter { engine: Option<String> },
}

impl WebSearchTool {
    /// Search tool for a custom Anthropic Messages endpoint.
    pub fn for_messages_endpoint(
        server_tools: &CustomEndpointServerTools,
        endpoint: &str,
    ) -> Option<Self> {
        if !server_tools.web_search {
            return None;
        }
        if is_openrouter_endpoint(endpoint) {
            Some(Self::OpenRouter {
                engine: normalize_openrouter_engine(server_tools.web_search_engine.as_deref()),
            })
        } else {
            Some(Self::AnthropicNative)
        }
    }

    /// OpenAI Chat has no standard server search tool; only OpenRouter's applies.
    pub fn for_chat_endpoint(
        server_tools: &CustomEndpointServerTools,
        endpoint: &str,
    ) -> Option<Self> {
        match Self::for_messages_endpoint(server_tools, endpoint)? {
            tool @ Self::OpenRouter { .. } => Some(tool),
            Self::AnthropicNative => None,
        }
    }

    pub fn tool_definition(&self) -> Value {
        match self {
            Self::AnthropicNative => json!({
                "type": "web_search_20250305",
                "name": WEB_SEARCH_TOOL_NAME,
                "max_uses": 5
            }),
            Self::OpenRouter { engine } => {
                let mut tool = json!({ "type": OPENROUTER_WEB_SEARCH_TYPE });
                if let Some(engine) = engine {
                    tool["parameters"] = json!({ "engine": engine });
                }
                tool
            }
        }
    }

    /// Only Anthropic's own tool type is documented to accept `cache_control`.
    pub fn accepts_cache_control(&self) -> bool {
        matches!(self, Self::AnthropicNative)
    }
}

pub fn is_openrouter_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

fn normalize_openrouter_engine(value: Option<&str>) -> Option<String> {
    let engine = value?.trim().to_ascii_lowercase();
    OPENROUTER_SEARCH_ENGINES
        .contains(&engine.as_str())
        .then_some(engine)
}

/// Names a provider uses for its search server tool.
pub fn is_web_search_tool_name(name: &str) -> bool {
    name == WEB_SEARCH_TOOL_NAME || name == OPENROUTER_WEB_SEARCH_TYPE
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub url: String,
    pub title: Option<String>,
    pub excerpt: Option<String>,
}

impl SearchHit {
    /// Accepts an Anthropic `web_search_result_location` citation and an
    /// OpenRouter Chat `url_citation` annotation (nested or flattened).
    pub fn from_citation(value: &Value) -> Option<Self> {
        let body = value.get("url_citation").unwrap_or(value);
        let url = string_field(body, &["url", "uri"])?;
        let excerpt = string_field(body, &["cited_text", "content"])
            .map(|text| compact_excerpt(&text))
            .filter(|text| !text.is_empty());
        Some(Self {
            url,
            title: string_field(body, &["title"]),
            excerpt,
        })
    }
}

/// Adds a hit, merging repeats of the same URL within one search.
pub fn push_hit(hits: &mut Vec<SearchHit>, hit: SearchHit) {
    if let Some(existing) = hits.iter_mut().find(|existing| existing.url == hit.url) {
        if existing.title.is_none() {
            existing.title = hit.title;
        }
        if existing.excerpt.is_none() {
            existing.excerpt = hit.excerpt;
        }
        return;
    }
    hits.push(hit);
}

/// Tool output for one search: optional query line, then numbered sources
/// with indented excerpts. Shown in the tool card and replayed to the model.
pub fn format_search_output(query: Option<&str>, hits: &[SearchHit]) -> String {
    let mut lines = Vec::new();
    if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
        lines.push(query.to_string());
    }
    if hits.is_empty() {
        lines.push("No result details were returned to the client.".to_string());
    }
    for (index, hit) in hits.iter().take(MAX_HITS_PER_SEARCH).enumerate() {
        match hit.title.as_deref() {
            Some(title) => lines.push(format!("{}. {} ({})", index + 1, title, hit.url)),
            None => lines.push(format!("{}. {}", index + 1, hit.url)),
        }
        if let Some(excerpt) = hit.excerpt.as_deref() {
            lines.push(format!("   {}", excerpt));
        }
    }
    if hits.len() > MAX_HITS_PER_SEARCH {
        lines.push(format!(
            "… {} more results",
            hits.len() - MAX_HITS_PER_SEARCH
        ));
    }
    lines.join("\n")
}

/// Record for searches the provider ran without exposing a tool call block.
pub fn provider_search_record(id: String, requests: u32, hits: &[SearchHit]) -> ToolCallInfo {
    let header = match requests {
        0 => "Provider-side web search".to_string(),
        1 => "Provider-side web search (1 request)".to_string(),
        n => format!("Provider-side web search ({} requests)", n),
    };
    let arguments = if requests > 0 {
        json!({ "requests": requests }).to_string()
    } else {
        "{}".to_string()
    };
    ToolCallInfo {
        id,
        name: WEB_SEARCH_TOOL_NAME.to_string(),
        arguments,
        order: None,
        server_tool: Some(ServerToolKind::WebSearch),
        server_tool_output: Some(format!("{}\n{}", header, format_search_output(None, hits))),
        server_tool_result: None,
        outcome: None,
        recorded_output: None,
        nested_tool_calls: None,
    }
}

/// Text replay of recorded searches for chat-shaped histories. Server tools
/// have no `tool` result message, so they must not be sent back as
/// `tool_calls`; their output rides along as assistant text instead. (The
/// Anthropic history builder replays result blocks natively, or as user-side
/// text: in the assistant's voice, models learn to write fake results.)
pub fn replay_search_results(tool_calls: &[ToolCallInfo]) -> Option<String> {
    let blocks: Vec<String> = tool_calls
        .iter()
        .filter(|tool_call| tool_call.server_tool == Some(ServerToolKind::WebSearch))
        .filter_map(|tool_call| tool_call.server_tool_output.as_deref())
        .map(str::trim)
        .filter(|output| !output.is_empty())
        .map(|output| format!("[Web Search Result]\n{}", output))
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// Inline source citation for a text span (UTF-16 offsets, as the renderer expects).
pub fn url_citation(hit: &SearchHit, start_index: u32, end_index: u32) -> Citation {
    Citation {
        id: String::new(),
        kind: CitationKind::Url,
        start_index: Some(start_index),
        end_index: Some(end_index),
        url: Some(hit.url.clone()),
        title: hit.title.clone(),
        file_id: None,
        filename: None,
        reference_ids: Vec::new(),
    }
}

/// Orders by position, drops repeats of the same URL at the same position and
/// assigns stable ids (same rules as the Responses citation collector).
pub fn finalize_citations(mut citations: Vec<Citation>) -> Vec<Citation> {
    citations.sort_by_key(|citation| {
        (
            citation.start_index.unwrap_or(u32::MAX),
            citation.end_index.unwrap_or(u32::MAX),
        )
    });
    let mut seen = std::collections::HashSet::new();
    citations.retain(|citation| seen.insert((citation.url.clone(), citation.end_index)));
    for (index, citation) in citations.iter_mut().enumerate() {
        citation.id = format!("citation-{}", index + 1);
    }
    citations
}

pub fn utf16_len(value: &str) -> u32 {
    u32::try_from(value.encode_utf16().count()).unwrap_or(u32::MAX)
}

fn compact_excerpt(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // Search engines join snippets with "..."; keep only snippets that carry
    // text (Exa also emits punctuation-only fragments such as ". ... )").
    let compact = compact
        .split("...")
        .map(str::trim)
        .filter(|snippet| snippet.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join(" ... ");
    if compact.chars().count() <= MAX_EXCERPT_CHARS {
        return compact;
    }
    let mut truncated: String = compact.chars().take(MAX_EXCERPT_CHARS).collect();
    truncated.push('…');
    truncated
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn server_tools(web_search: bool, engine: Option<&str>) -> CustomEndpointServerTools {
        CustomEndpointServerTools {
            web_search,
            web_search_engine: engine.map(str::to_string),
        }
    }

    #[test]
    fn detects_openrouter_hosts_only() {
        assert!(is_openrouter_endpoint("https://openrouter.ai/api/v1"));
        assert!(is_openrouter_endpoint(" https://eu.openrouter.ai/api/v1 "));
        assert!(!is_openrouter_endpoint("https://api.anthropic.com"));
        assert!(!is_openrouter_endpoint(
            "https://openrouter.ai.example.com/v1"
        ));
        assert!(!is_openrouter_endpoint("not a url"));
    }

    #[test]
    fn picks_tool_shape_per_endpoint() {
        let enabled = server_tools(true, None);
        assert_eq!(
            WebSearchTool::for_messages_endpoint(&enabled, "https://api.deepseek.com/anthropic/v1"),
            Some(WebSearchTool::AnthropicNative)
        );
        assert_eq!(
            WebSearchTool::for_messages_endpoint(&enabled, "https://openrouter.ai/api/v1"),
            Some(WebSearchTool::OpenRouter { engine: None })
        );
        assert_eq!(
            WebSearchTool::for_chat_endpoint(&enabled, "https://api.deepseek.com/v1"),
            None
        );
        assert_eq!(
            WebSearchTool::for_messages_endpoint(
                &server_tools(false, None),
                "https://openrouter.ai/api/v1"
            ),
            None
        );
    }

    #[test]
    fn openrouter_tool_forwards_known_engines_only() {
        let exa = WebSearchTool::for_chat_endpoint(
            &server_tools(true, Some("Exa")),
            "https://openrouter.ai/api/v1",
        )
        .unwrap();
        assert_eq!(
            exa.tool_definition(),
            json!({ "type": "openrouter:web_search", "parameters": { "engine": "exa" } })
        );
        let auto = WebSearchTool::for_chat_endpoint(
            &server_tools(true, Some("auto")),
            "https://openrouter.ai/api/v1",
        )
        .unwrap();
        assert_eq!(
            auto.tool_definition(),
            json!({ "type": "openrouter:web_search" })
        );
        assert!(!auto.accepts_cache_control());
        assert!(WebSearchTool::AnthropicNative.accepts_cache_control());
    }

    #[test]
    fn parses_messages_and_chat_citations() {
        let messages = SearchHit::from_citation(&json!({
            "type": "web_search_result_location",
            "url": "https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/",
            "title": "Announcing Rust 1.99.1",
            "cited_text": "Announcing Rust 1.99.1\n\n...\n\n...\n\nOct. 15, 2026 · The Rust Release Team",
            "encrypted_index": ""
        }))
        .unwrap();
        assert_eq!(messages.title.as_deref(), Some("Announcing Rust 1.99.1"));
        assert_eq!(
            messages.excerpt.as_deref(),
            Some("Announcing Rust 1.99.1 ... Oct. 15, 2026 · The Rust Release Team")
        );

        let chat = SearchHit::from_citation(&json!({
            "type": "url_citation",
            "url_citation": { "url": "https://example.com", "title": "Example", "start_index": 0, "end_index": 0 }
        }))
        .unwrap();
        assert_eq!(chat.url, "https://example.com");
        assert_eq!(chat.excerpt, None);
        assert!(
            SearchHit::from_citation(&json!({ "type": "char_location", "cited_text": "x" }))
                .is_none()
        );
    }

    #[test]
    fn excerpts_drop_punctuation_only_snippets() {
        let hit = SearchHit::from_citation(&json!({
            "url": "https://a.test",
            "cited_text": "... # Version 1.99.1 (2026-10-15)\n\n...\n\n. ... . ... ) ... )\n\n... # Version 1.98.1 ..."
        }))
        .unwrap();
        assert_eq!(
            hit.excerpt.as_deref(),
            Some("# Version 1.99.1 (2026-10-15) ... # Version 1.98.1")
        );
    }

    #[test]
    fn excerpts_are_truncated() {
        let hit = SearchHit::from_citation(
            &json!({ "url": "https://a.test", "cited_text": "word ".repeat(400) }),
        )
        .unwrap();
        let excerpt = hit.excerpt.unwrap();
        assert_eq!(excerpt.chars().count(), MAX_EXCERPT_CHARS + 1);
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn formats_search_output_and_provider_record() {
        let mut hits = Vec::new();
        push_hit(
            &mut hits,
            SearchHit {
                url: "https://a.test".into(),
                title: None,
                excerpt: None,
            },
        );
        push_hit(
            &mut hits,
            SearchHit {
                url: "https://a.test".into(),
                title: Some("A".into()),
                excerpt: Some("fact".into()),
            },
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            format_search_output(Some("rust"), &hits),
            "rust\n1. A (https://a.test)\n   fact"
        );

        let record = provider_search_record("msg_1_web_search".into(), 2, &[]);
        assert!(record.is_server_tool());
        assert_eq!(record.name, "web_search");
        assert_eq!(record.arguments, r#"{"requests":2}"#);
        assert_eq!(
            record.server_tool_output.as_deref(),
            Some("Provider-side web search (2 requests)\nNo result details were returned to the client.")
        );
    }

    #[test]
    fn finalize_citations_dedupes_and_numbers() {
        let hit = SearchHit {
            url: "https://a.test".into(),
            title: None,
            excerpt: None,
        };
        let other = SearchHit {
            url: "https://b.test".into(),
            title: None,
            excerpt: None,
        };
        let citations = finalize_citations(vec![
            url_citation(&other, 10, 20),
            url_citation(&hit, 0, 5),
            url_citation(&hit, 0, 5),
        ]);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].url.as_deref(), Some("https://a.test"));
        assert_eq!(citations[0].id, "citation-1");
        assert_eq!(citations[1].id, "citation-2");
    }
}
