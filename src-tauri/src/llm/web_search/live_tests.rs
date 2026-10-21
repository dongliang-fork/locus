//! Opt-in checks against the real OpenRouter API through the production
//! transports. They use the OpenRouter custom provider already configured in
//! Locus (key read from the keychain, never printed or written) and spend a
//! few cents of search and inference quota each. Run with:
//! `cargo test --lib web_search::live_tests -- --ignored --nocapture`
//! (reports use `std::eprintln!`: the crate-level `eprintln!` routes to tracing,
//! which has no subscriber under tests).

use super::{is_openrouter_endpoint, WebSearchTool};
use crate::llm::{anthropic, chat_completions};
use crate::session::models::{ChatMessage, ToolCallInfo};

const SYSTEM: &str =
    "You are a concise coding agent. Use web search for current facts and tools for files.";
const SEARCH_THEN_READ: &str = "First search the web for the latest stable Rust release version and its release date. Then call read_file on Cargo.toml so we can compare it with the project's rust-version.";

fn model() -> String {
    std::env::var("LOCUS_OPENROUTER_WEB_TEST_MODEL")
        .unwrap_or_else(|_| "google/gemini-3.8-flash".into())
}

/// (endpoint, key) of the first configured OpenRouter custom provider. Reads
/// the providers file directly so no config migration or write can happen.
fn openrouter_credentials() -> (String, String) {
    let path = crate::commands::custom_providers_path().expect("custom providers path");
    let file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read custom providers"))
            .expect("parse custom providers");
    let provider = file["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|provider| {
            provider["endpoint"]
                .as_str()
                .is_some_and(is_openrouter_endpoint)
        })
        .expect("an OpenRouter custom provider must be configured in Locus");
    let id = provider["id"].as_str().expect("provider id");
    let key = crate::keychain::get_secret(&crate::keychain::endpoint_key_name(id))
        .expect("read keychain")
        .expect("OpenRouter provider key missing");
    (provider["endpoint"].as_str().unwrap().to_string(), key)
}

fn read_file_tool() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file from the local workspace.",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }
    })
}

fn message(role: &str, content: &str) -> ChatMessage {
    serde_json::from_value(serde_json::json!({
        "id": format!("{}_{}", role, uuid::Uuid::new_v4()),
        "role": role,
        "content": content,
        "createdAt": 0
    }))
    .unwrap()
}

fn search_calls(tool_calls: &[ToolCallInfo]) -> Vec<&ToolCallInfo> {
    tool_calls
        .iter()
        .filter(|tool_call| tool_call.is_server_tool())
        .collect()
}

fn report(
    label: &str,
    text: &str,
    tool_calls: &[ToolCallInfo],
    citations: usize,
    cost: f64,
    input_tokens: u32,
) {
    std::eprintln!("== {label}: cost=${cost:.5} input_tokens={input_tokens} citations={citations}");
    for tool_call in tool_calls {
        std::eprintln!(
            "-- tool {} server={} args={}\n{}",
            tool_call.name,
            tool_call.is_server_tool(),
            tool_call.arguments,
            tool_call
                .server_tool_output
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(700)
                .collect::<String>()
        );
    }
    std::eprintln!("-- text: {}", text.chars().take(600).collect::<String>());
}

async fn messages_round(
    endpoint: &str,
    key: &str,
    history: &[ChatMessage],
    search: &WebSearchTool,
) -> anthropic::LlmResponse {
    anthropic::stream_chat_native(
        key,
        &model(),
        SYSTEM,
        history,
        &[read_file_tool()],
        endpoint,
        None,
        false,
        Some(search),
        None,
        "OpenRouterLive",
        false,
        Some(4000),
        |_| {},
        |_| {},
        |_, _| {},
    )
    .await
    .expect("OpenRouter Messages request")
}

#[tokio::test]
#[ignore = "real OpenRouter request with the configured Locus key; opt in with --ignored"]
async fn messages_native_search_records_search_and_cost() {
    let (endpoint, key) = openrouter_credentials();
    let history = vec![message(
        "user",
        "Search the web: what is the latest stable Rust release and when was it released? Two sentences.",
    )];
    let resp = messages_round(
        &endpoint,
        &key,
        &history,
        &WebSearchTool::OpenRouter { engine: None },
    )
    .await;
    report(
        "messages/auto",
        &resp.text,
        &resp.tool_calls,
        resp.citations.len(),
        resp.cost_usd,
        resp.input_tokens,
    );

    assert_eq!(resp.finish_reason, "end_turn");
    assert!(
        !search_calls(&resp.tool_calls).is_empty(),
        "search must be recorded"
    );
    assert!(resp.tool_calls.iter().all(ToolCallInfo::is_server_tool));
    assert!(resp.cost_usd > 0.0);
    assert!(resp.input_tokens > 0);
}

#[tokio::test]
#[ignore = "real OpenRouter request with the configured Locus key; opt in with --ignored"]
async fn messages_exa_excerpts_carry_into_the_next_round() {
    let (endpoint, key) = openrouter_credentials();
    let search = WebSearchTool::OpenRouter {
        engine: Some("exa".into()),
    };
    let user = message("user", SEARCH_THEN_READ);
    let first = messages_round(&endpoint, &key, std::slice::from_ref(&user), &search).await;
    report(
        "messages/exa round 1",
        &first.text,
        &first.tool_calls,
        first.citations.len(),
        first.cost_usd,
        first.input_tokens,
    );

    assert_eq!(first.finish_reason, "tool_calls");
    let searches = search_calls(&first.tool_calls);
    assert!(!searches.is_empty(), "Exa search must be recorded");
    assert!(
        searches.iter().any(|call| call
            .server_tool_output
            .as_deref()
            .is_some_and(|out| out.contains("\n1. "))),
        "Exa results must reach the record"
    );
    let read = first
        .tool_calls
        .iter()
        .find(|call| !call.is_server_tool())
        .expect("model should call read_file after searching");

    let mut assistant = message("assistant", &first.text);
    assistant.tool_calls = Some(first.tool_calls.clone());
    let mut tool = message(
        "tool",
        "[package]\nname = \"demo\"\nedition = \"2024\"\nrust-version = \"1.85\"\n",
    );
    tool.tool_call_id = Some(read.id.clone());
    let second = messages_round(&endpoint, &key, &[user, assistant, tool], &search).await;
    report(
        "messages/exa round 2",
        &second.text,
        &second.tool_calls,
        second.citations.len(),
        second.cost_usd,
        second.input_tokens,
    );

    assert_eq!(second.finish_reason, "end_turn");
    assert!(second.text.contains("1.85"));
    std::eprintln!(
        "round 2 searched again: {}",
        !search_calls(&second.tool_calls).is_empty()
    );
}

#[tokio::test]
#[ignore = "real OpenRouter request with the configured Locus key; opt in with --ignored"]
async fn chat_native_search_records_search_and_citations() {
    let (endpoint, key) = openrouter_credentials();
    let search = WebSearchTool::OpenRouter { engine: None };
    let history = vec![message(
        "user",
        "Search the web: what is the latest stable Rust release and when was it released? Two sentences.",
    )];
    let resp = chat_completions::stream_chat(
        &key,
        &model(),
        SYSTEM,
        &history,
        &[read_file_tool()],
        &endpoint,
        chat_completions::CustomChatTuning {
            max_output_tokens: Some(4000),
            web_search: Some(&search),
            ..Default::default()
        },
        false,
        |_| {},
        |_| {},
        |_, _| {},
    )
    .await
    .expect("OpenRouter Chat request");
    report(
        "chat/auto",
        &resp.text,
        &resp.tool_calls,
        resp.citations.len(),
        resp.cost_usd,
        resp.input_tokens,
    );

    assert_eq!(search_calls(&resp.tool_calls).len(), 1);
    assert!(resp.tool_calls.iter().all(ToolCallInfo::is_server_tool));
    assert!(resp.cost_usd > 0.0);
}
