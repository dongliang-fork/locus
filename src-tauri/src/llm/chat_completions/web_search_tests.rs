//! OpenRouter `openrouter:web_search` over Chat Completions, driven by
//! recorded `google/gemini-3.8-flash` streams (trimmed).

use super::{
    build_api_messages, build_request_body, drain_sse_buffer, finalize_stream_response,
    ChatCompletionsFlavor, ChatStreamState, CustomChatTuning,
};
use crate::llm::openrouter::LlmResponse;
use crate::llm::web_search::{self, WebSearchTool};
use crate::session::models::{ChatMessage, MessageRole, ServerToolKind, ToolCallInfo};

fn parse(raw: &str) -> LlmResponse {
    let mut state = ChatStreamState::new();
    let mut buffer = raw.to_string();
    let got_done = drain_sse_buffer(
        &mut buffer,
        true,
        false,
        &mut state,
        &|_| {},
        &|_| {},
        &|_, _| {},
    )
    .expect("fixture should parse");
    assert!(got_done, "fixture ends with [DONE]");
    finalize_stream_response(
        "Custom Chat",
        "google/gemini-3.8-flash",
        "https://openrouter.ai/api/v1/chat/completions",
        ChatCompletionsFlavor::Generic,
        false,
        got_done,
        state,
        String::new(),
        raw.to_string(),
    )
    .expect("fixture should finalize")
}

fn message(role: MessageRole, content: &str, tool_calls: Option<Vec<ToolCallInfo>>) -> ChatMessage {
    ChatMessage {
        id: format!("{:?}", role),
        role,
        content: content.to_string(),
        created_at: 0,
        prompt_prefix: None,
        prompt_suffix: None,
        response_id: None,
        content_order: None,
        thinking_order: None,
        tool_calls,
        tool_call_id: None,
        images: None,
        asset_refs: None,
        thinking_content: None,
        thinking_duration: None,
        thinking_signature: None,
        knowledge_proposal: None,
        render_parts: None,
    }
}

fn client_call(id: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: id.to_string(),
        name: "list".to_string(),
        arguments: "{}".to_string(),
        order: None,
        server_tool: None,
        server_tool_output: None,
        server_tool_result: None,
        outcome: None,
        recorded_output: None,
        nested_tool_calls: None,
    }
}

#[test]
fn native_search_becomes_record_and_inline_citations() {
    let resp = parse(include_str!(
        "../fixtures/openrouter_chat_native_search.sse"
    ));

    assert!(resp.text.contains("1.99.1"));
    assert_eq!(resp.finish_reason, "stop");
    assert!((resp.cost_usd - 0.0283675).abs() < 1e-9);
    assert_eq!(resp.tool_calls.len(), 1);
    let search = &resp.tool_calls[0];
    assert!(search.id.ends_with("_web_search"), "{}", search.id);
    assert_eq!(search.server_tool, Some(ServerToolKind::WebSearch));
    let output = search.server_tool_output.as_deref().unwrap();
    assert!(
        output.starts_with("Provider-side web search (2 requests)\n1. rust-lang.org ("),
        "{output}"
    );
    assert!(output.contains("\n2. rust-lang.org ("), "{output}");

    let spans: Vec<_> = resp
        .citations
        .iter()
        .map(|citation| (citation.start_index, citation.end_index))
        .collect();
    assert_eq!(spans, vec![(Some(0), Some(79)), (Some(81), Some(246))]);
    assert!(web_search::utf16_len(&resp.text) >= 246);
}

#[test]
fn exa_search_keeps_excerpts_in_the_record_only() {
    let resp = parse(include_str!("../fixtures/openrouter_chat_exa_search.sse"));

    assert_eq!(resp.finish_reason, "stop");
    assert_eq!(resp.tool_calls.len(), 1);
    let output = resp.tool_calls[0].server_tool_output.as_deref().unwrap();
    assert!(
        output.starts_with("Provider-side web search (3 requests)\n"),
        "{output}"
    );
    assert!(output.contains(
        "1. Rust Release Notes (https://doc.rust-lang.org/stable/releases.html)\n   Rust Release Notes ... # § Version 1.99.1 (2026-10-15)"
    ), "{output}");
    // The same page returned by two searches is listed once.
    assert_eq!(
        output
            .matches("(https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/)")
            .count(),
        1
    );
    // Zero-width annotations carry results, not text references.
    assert!(resp.citations.is_empty());
}

#[test]
fn request_body_appends_openrouter_search_tool() {
    let search = WebSearchTool::OpenRouter {
        engine: Some("exa".to_string()),
    };
    let function = serde_json::json!({"type":"function","function":{"name":"list","parameters":{"type":"object"}}});
    let body = build_request_body(
        "google/gemini-3.8-flash",
        Vec::new(),
        &[function],
        ChatCompletionsFlavor::Generic,
        CustomChatTuning {
            web_search: Some(&search),
            ..Default::default()
        },
    );
    assert_eq!(body["tools"][0]["function"]["name"], "list");
    assert_eq!(
        body["tools"][1],
        serde_json::json!({"type":"openrouter:web_search","parameters":{"engine":"exa"}})
    );

    let search_only = build_request_body(
        "google/gemini-3.8-flash",
        Vec::new(),
        &[],
        ChatCompletionsFlavor::Generic,
        CustomChatTuning {
            web_search: Some(&search),
            ..Default::default()
        },
    );
    assert_eq!(search_only["tools"].as_array().map(Vec::len), Some(1));

    let without = build_request_body(
        "google/gemini-3.8-flash",
        Vec::new(),
        &[],
        ChatCompletionsFlavor::Generic,
        CustomChatTuning::default(),
    );
    assert!(without.get("tools").is_none());
}

#[test]
fn history_replays_server_search_as_text_not_tool_calls() {
    let record = web_search::provider_search_record(
        "gen-1_web_search".to_string(),
        1,
        &[web_search::SearchHit {
            url: "https://a.test".to_string(),
            title: Some("A".to_string()),
            excerpt: Some("fact".to_string()),
        }],
    );
    let mut tool_result = message(MessageRole::Tool, "files", None);
    tool_result.tool_call_id = Some("call_1".to_string());
    let history = vec![
        message(MessageRole::User, "search", None),
        message(
            MessageRole::Assistant,
            "answer",
            Some(vec![record.clone(), client_call("call_1")]),
        ),
        tool_result,
        message(MessageRole::Assistant, "", Some(vec![record])),
    ];

    let messages = build_api_messages(
        "",
        &history,
        ChatCompletionsFlavor::Generic,
        false,
        false,
        None,
    );

    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[1]["content"],
        "answer\n\n[Web Search Result]\nProvider-side web search (1 request)\n1. A (https://a.test)\n   fact"
    );
    let calls = messages[1]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "call_1");
    assert_eq!(messages[2]["role"], "tool");
    assert!(messages[3]["content"]
        .as_str()
        .unwrap()
        .starts_with("[Web Search Result]\n"));
    assert!(messages[3].get("tool_calls").is_none());
}
