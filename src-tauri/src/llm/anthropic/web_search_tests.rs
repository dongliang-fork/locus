//! Server-side web search over the Anthropic Messages stream, driven by
//! recorded OpenRouter responses (`google/gemini-3.8-flash`, trimmed) and
//! hand-written official-Anthropic streams.

use super::{parse_anthropic_sse, LlmResponse};
use crate::llm::web_search;
use crate::session::models::ServerToolKind;

async fn parse(sse: &str) -> LlmResponse {
    let response = reqwest::Response::from(
        http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(sse.to_string())
            .unwrap(),
    );
    parse_anthropic_sse(
        "test",
        "https://openrouter.ai/api/v1/messages",
        false,
        response,
        String::new(),
        None,
        |_| {},
        |_| {},
        |_, _| {},
    )
    .await
    .expect("fixture should parse")
}

fn sse(events: &[(&str, serde_json::Value)]) -> String {
    events
        .iter()
        .map(|(event, data)| format!("event: {}\ndata: {}\n\n", event, data))
        .collect()
}

#[tokio::test]
async fn openrouter_native_search_becomes_record_and_inline_citation() {
    let resp = parse(include_str!(
        "../fixtures/openrouter_messages_native_search.sse"
    ))
    .await;

    assert!(resp.text.contains("1.99.1"));
    assert_eq!(resp.finish_reason, "end_turn");
    // message_start reports 0 input tokens and `null` cache counts; the real
    // totals arrive in message_delta.
    assert_eq!(resp.input_tokens, 97);
    assert_eq!(resp.output_tokens, 302);
    assert!((resp.cost_usd - 0.02920525).abs() < 1e-9);

    assert_eq!(resp.tool_calls.len(), 1);
    let search = &resp.tool_calls[0];
    assert_eq!(search.id, "msg_kyW4455Kua5IzHphczhZ_web_search");
    assert_eq!(search.name, "web_search");
    assert_eq!(search.server_tool, Some(ServerToolKind::WebSearch));
    let output = search.server_tool_output.as_deref().unwrap();
    assert!(
        output.starts_with(
            "Provider-side web search (2 requests)\n1. rust-lang.org (https://vertexaisearch.cloud.google.com/"
        ),
        "{output}"
    );

    // The citation arrives after its text block closed; it still spans it.
    assert_eq!(resp.citations.len(), 1);
    let citation = &resp.citations[0];
    assert_eq!(citation.id, "citation-1");
    assert_eq!(citation.start_index, Some(0));
    assert_eq!(citation.end_index, Some(web_search::utf16_len(&resp.text)));
    assert_eq!(citation.title.as_deref(), Some("rust-lang.org"));
}

#[tokio::test]
async fn openrouter_native_search_in_tool_round_keeps_client_call() {
    let resp = parse(include_str!(
        "../fixtures/openrouter_messages_native_tool_round.sse"
    ))
    .await;

    assert_eq!(resp.finish_reason, "tool_calls");
    assert_eq!(resp.input_tokens, 102);
    assert_eq!(resp.tool_calls.len(), 2);
    let client = &resp.tool_calls[0];
    assert_eq!(client.name, "read_file");
    assert!(!client.is_server_tool());
    assert_eq!(client.arguments, r#"{"path":"Cargo.toml"}"#);

    let search = &resp.tool_calls[1];
    assert!(search.is_server_tool());
    assert_eq!(
        search.server_tool_output.as_deref(),
        Some(
            "Provider-side web search (2 requests)\nNo result details were returned to the client."
        )
    );
    assert!(resp.citations.is_empty());
}

#[tokio::test]
async fn openrouter_exa_server_tool_use_is_never_dispatched_locally() {
    let resp = parse(include_str!(
        "../fixtures/openrouter_messages_exa_search.sse"
    ))
    .await;

    // `server_tool_use` blocks without a `web_search_tool_result` used to be
    // treated as client calls, forcing a bogus local `openrouter:web_search`.
    assert_eq!(resp.finish_reason, "end_turn");
    assert!(resp.text.contains("Rust 1.99.1"));
    assert_eq!(resp.tool_calls.len(), 2);
    assert!(resp.tool_calls.iter().all(|tool_call| {
        tool_call.is_server_tool()
            && tool_call.name == "web_search"
            && tool_call.server_tool == Some(ServerToolKind::WebSearch)
    }));

    let first = resp.tool_calls[0].server_tool_output.as_deref().unwrap();
    assert!(
        first.starts_with("latest stable release Rust programming language"),
        "{first}"
    );
    assert!(first.contains(
        "\n1. Announcing Rust 1.99.1 | Rust Blog (https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/)\n   Announcing Rust 1.99.1 | Rust Blog ... ## Announcing Rust 1.99.1 ... Oct. 15, 2026"
    ), "{first}");
    let second = resp.tool_calls[1].server_tool_output.as_deref().unwrap();
    assert!(
        second.starts_with("site:github.com/rust-lang/rust/releases\n"),
        "{second}"
    );
    assert!(
        second.contains("(https://github.com/rust-lang/rust/releases)"),
        "{second}"
    );
    assert!(
        !second.contains("blog.rust-lang.org/2026/10/15"),
        "{second}"
    );

    // Results ride on an empty carrier text block: no inline citations.
    assert!(resp.citations.is_empty());
    // message_delta carries `cache_creation_input_tokens: null`.
    assert_eq!(resp.input_tokens, 21739);
    assert_eq!(resp.cache_read_tokens, 3888);
    assert_eq!(resp.output_tokens, 1634);
}

#[tokio::test]
async fn openrouter_exa_tool_round_attaches_results_to_the_search_call() {
    let resp = parse(include_str!(
        "../fixtures/openrouter_messages_exa_tool_round.sse"
    ))
    .await;

    assert_eq!(resp.finish_reason, "tool_calls");
    assert_eq!(resp.tool_calls.len(), 2);
    let search = &resp.tool_calls[0];
    assert!(search.is_server_tool());
    let output = search.server_tool_output.as_deref().unwrap();
    assert!(
        output
            .contains("\n1. The Rust Release Announcements (https://blog.rust-lang.org/releases/)"),
        "{output}"
    );
    assert!(
        output
            .contains("\n3. Announcing Rust 1.99.1 (https://blog.rust-lang.org/releases/latest/)"),
        "{output}"
    );
    let client = &resp.tool_calls[1];
    assert_eq!(client.name, "read_file");
    assert!(!client.is_server_tool());
}

#[tokio::test]
async fn anthropic_search_result_block_and_text_citations() {
    let url = "https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/";
    let stream = sse(&[
        (
            "message_start",
            serde_json::json!({"type":"message_start","message":{"id":"msg_a","usage":{"input_tokens":10,"output_tokens":1}}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust release\"}"}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":0}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":url,"title":"Announcing Rust 1.99.1","encrypted_content":"x"}]}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":1}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":url,"title":"Announcing Rust 1.99.1","cited_text":"Rust 1.99.1","encrypted_index":"y"}}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Rust 1.99.1 is out."}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":2}),
        ),
        (
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20,"server_tool_use":{"web_search_requests":1}}}),
        ),
        ("message_stop", serde_json::json!({"type":"message_stop"})),
    ]);
    let resp = parse(&stream).await;

    assert_eq!(resp.finish_reason, "end_turn");
    assert_eq!(resp.input_tokens, 10);
    assert_eq!(resp.cost_usd, 0.0);
    assert_eq!(
        resp.tool_calls.len(),
        1,
        "no synthesized record when the block was surfaced"
    );
    let search = &resp.tool_calls[0];
    assert_eq!(search.arguments, r#"{"query":"rust release"}"#);
    assert_eq!(
        search.server_tool_output.as_deref(),
        Some("rust release\n1. Announcing Rust 1.99.1 (https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/)")
    );
    assert_eq!(resp.citations.len(), 1);
    assert_eq!(resp.citations[0].start_index, Some(0));
    assert_eq!(resp.citations[0].end_index, Some(19));
    assert_eq!(resp.web_search_results.len(), 1);
}

#[tokio::test]
async fn anthropic_empty_search_result_is_still_a_server_tool() {
    let stream = sse(&[
        (
            "message_start",
            serde_json::json!({"type":"message_start","message":{"id":"msg_b","usage":{"input_tokens":5}}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_2","name":"web_search","input":{}}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"nothing\"}"}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":0}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_2","content":[]}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":1}),
        ),
        (
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        ),
        ("message_stop", serde_json::json!({"type":"message_stop"})),
    ]);
    let resp = parse(&stream).await;

    assert_eq!(resp.finish_reason, "end_turn");
    assert_eq!(resp.tool_calls.len(), 1);
    assert!(resp.tool_calls[0].is_server_tool());
    assert_eq!(
        resp.tool_calls[0].server_tool_output.as_deref(),
        Some("nothing\nNo results.")
    );
}

#[tokio::test]
async fn unknown_server_tools_are_recorded_not_dispatched() {
    let stream = sse(&[
        (
            "message_start",
            serde_json::json!({"type":"message_start","message":{"id":"msg_c","usage":{"input_tokens":5}}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"call_9","name":"openrouter:datetime","input":{}}}),
        ),
        (
            "content_block_stop",
            serde_json::json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        ),
        ("message_stop", serde_json::json!({"type":"message_stop"})),
    ]);
    let resp = parse(&stream).await;

    assert_eq!(resp.finish_reason, "end_turn");
    let tool = &resp.tool_calls[0];
    assert!(tool.is_server_tool());
    assert_eq!(tool.server_tool, None);
    assert_eq!(tool.arguments, "{}");
    assert_eq!(
        tool.server_tool_output.as_deref(),
        Some("Provider-side tool `openrouter:datetime` ran on the server.")
    );
}

#[tokio::test]
async fn search_result_blocks_are_kept_per_call_for_native_replay() {
    // DeepSeek shape: both searches start before either result arrives, and
    // each result names its call through `tool_use_id`.
    let result = |id: &str, url: &str| serde_json::json!({"type":"web_search_tool_result","tool_use_id":id,"content":[{"type":"web_search_result","title":"t","url":url,"encrypted_content":"enc","page_age":null}]});
    let stream = sse(&[
        (
            "message_start",
            serde_json::json!({"type":"message_start","message":{"id":"msg_d","usage":{"input_tokens":7}}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"call_00","name":"web_search","input":{},"caller":{"type":"direct"}}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"first\"}"}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"call_01","name":"web_search","input":{}}}),
        ),
        (
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"second\"}"}}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":2,"content_block":result("call_00","https://a.example/")}),
        ),
        (
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":3,"content_block":result("call_01","https://b.example/")}),
        ),
        (
            "message_delta",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        ),
        ("message_stop", serde_json::json!({"type":"message_stop"})),
    ]);
    let resp = parse(&stream).await;

    assert_eq!(resp.tool_calls.len(), 2);
    for (call, query, url) in [
        (&resp.tool_calls[0], "first", "https://a.example/"),
        (&resp.tool_calls[1], "second", "https://b.example/"),
    ] {
        assert_eq!(call.server_tool, Some(ServerToolKind::WebSearch));
        assert_eq!(call.arguments, format!(r#"{{"query":"{query}"}}"#));
        assert!(call
            .server_tool_output
            .as_deref()
            .is_some_and(|output| output.contains(url)));
        let block = call.server_tool_result.as_ref().expect("raw result kept");
        assert_eq!(block.origin, "openrouter.ai");
        assert_eq!(block.block_type, "web_search_tool_result");
        assert_eq!(block.content[0]["url"], serde_json::json!(url));
        assert_eq!(
            block.content[0]["encrypted_content"],
            serde_json::json!("enc")
        );
    }
}
