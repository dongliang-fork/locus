use super::*;
use serde_json::json;

#[tokio::test]
async fn codex_oauth_native_web_is_available_in_every_loading_mode_without_local_fetch() {
    let root = tempfile::tempdir().unwrap();
    let mut instance = super::super::tests::native_plan_test_instance(&root);
    instance.backend = LlmBackend::OpenAiCodex {
        auth: Arc::new(tokio::sync::Mutex::new(
            crate::auth::codex::CodexAuthState::for_test("test-token", "test-account"),
        )),
        transport: crate::commands::CodexTransportMode::Http,
        base_url: None,
    };
    instance.effective_model = "openai/gpt-6-astra".into();
    let active = HashSet::from(["web_fetch".to_string()]);
    for mode in [
        crate::config::DynamicToolLoadingMode::Native,
        crate::config::DynamicToolLoadingMode::Direct,
        crate::config::DynamicToolLoadingMode::MetaTool,
    ] {
        let renderer = instance.refresh_lazy_tool_renderer(mode, true);
        let tools = instance
            .prepare_request_tools(renderer, mode, &active)
            .await;
        assert_eq!(
            tools
                .api_tools
                .iter()
                .filter(|tool| tool["type"] == "namespace" && tool["name"] == "web")
                .count(),
            1
        );
        assert!(!tools
            .api_tools
            .iter()
            .any(|tool| tool["function"]["name"] == "web_fetch"));
        assert!(
            !instance
                .is_allowed_tool_for_active_skills("web_fetch", &active)
                .await
        );
        assert!(!instance
            .lazy_tool_manifest_prompt()
            .await
            .unwrap_or_default()
            .contains("`web_fetch`"));
        let load = instance
            .execute_tool_load_with_mode_and_skills(&json!({"tools":["web_fetch"]}), mode, &active)
            .await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&load.output).unwrap()["tools"][0]["status"],
            "not_allowed"
        );
    }
    assert!(AgentInstance::is_readonly_tool_call(
        "web.run",
        &json!({"open":[{"ref_id":"https://example.com/"}]})
    ));
    assert!(plan_policy::allows_observation("web.run", &json!({})));
    instance.backend = LlmBackend::OpenRouter {
        api_key: String::new(),
        base_url: None,
    };
    let tools = instance
        .prepare_request_tools(
            LazyToolRenderer::ToolLoadFallback,
            crate::config::DynamicToolLoadingMode::MetaTool,
            &HashSet::new(),
        )
        .await;
    assert!(!tools.api_tools.iter().any(|tool| tool["name"] == "web"));
    assert!(
        instance
            .is_allowed_tool_for_active_skills("web_fetch", &active)
            .await
    );
    let store = SessionStore::new(root.path()).unwrap();
    assert_eq!(
        instance.execute_codex_web(&store, &json!({})).await.output,
        "web.run is available only with Codex OAuth."
    );
}

#[tokio::test]
async fn codex_oauth_web_round_trip_persists_resumes_and_exports_native_results() {
    tokio::time::timeout(Duration::from_secs(20), round_trip())
        .await
        .expect("native web round trip timed out");
}

async fn round_trip() {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;
    use hyper_util::rt::TokioIo;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/codex", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let server = tokio::spawn(async move {
        for index in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            let requests = observed.clone();
            let service = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| {
                    let requests = requests.clone();
                    async move {
                        let path = request.uri().path().to_string();
                        assert_eq!(
                            request.headers()["authorization"],
                            "Bearer synthetic-web-token"
                        );
                        assert_eq!(request.headers()["chatgpt-account-id"], "synthetic-account");
                        let compressed = request
                            .headers()
                            .get("content-encoding")
                            .is_some_and(|value| value == "zstd");
                        let bytes = request.into_body().collect().await.unwrap().to_bytes();
                        let bytes = if compressed {
                            zstd::stream::decode_all(bytes.as_ref()).unwrap()
                        } else {
                            bytes.to_vec()
                        };
                        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        requests.lock().unwrap().push((path, body));
                        let (content_type, body) = if index == 1 {
                            ("application/json",json!({"output":"Native page content [Example](https://example.com/) turn0view0"}).to_string())
                        } else {
                            let output = if index == 0 {
                                vec![
                                    json!({"type":"function_call","id":"fc_native_web","call_id":"call_native_web",
                            "namespace":"web","name":"run","arguments":r#"{"open":[{"ref_id":"https://example.com/"}]}"#}),
                                ]
                            } else {
                                vec![
                                    json!({"type":"message","id":"msg_final","role":"assistant","content":[{"type":"output_text","text":"The native page was read."}]}),
                                ]
                            };
                            let id = format!("resp_{index}");
                            (
                                "text/event-stream",
                                format!(
                                    "data: {}\n\ndata: {}\n\n",
                                    json!({"type":"response.created","response":{"id":id}}),
                                    json!({"type":"response.completed","response":{"id":id,"status":"completed","output":output,
                                "usage":{"input_tokens":20,"output_tokens":10}}})
                                ),
                            )
                        };
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .header("content-type", content_type)
                                .header("connection", "close")
                                .body(Full::new(Bytes::from(body)))
                                .unwrap(),
                        )
                    }
                },
            );
            hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .await
                .unwrap();
        }
    });
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let session = store
        .create_session("native web", None, None, "chat", None)
        .unwrap();
    store
        .add_message(&session, MessageRole::User, "Read https://example.com/")
        .unwrap();
    let mut instance = super::super::tests::native_plan_test_instance(&root);
    instance.session_id = session.clone();
    instance.effective_model = "openai/gpt-6-astra".into();
    instance.backend = LlmBackend::OpenAiCodex {
        auth: Arc::new(tokio::sync::Mutex::new(
            crate::auth::codex::CodexAuthState::for_test(
                "synthetic-web-token",
                "synthetic-account",
            ),
        )),
        transport: crate::commands::CodexTransportMode::Http,
        base_url: Some(base_url),
    };
    let renderer =
        instance.refresh_lazy_tool_renderer(crate::config::DynamicToolLoadingMode::Native, true);
    let tools = instance
        .prepare_request_tools(
            renderer,
            crate::config::DynamicToolLoadingMode::Native,
            &HashSet::new(),
        )
        .await;
    let first = instance
        .call_llm(
            &store,
            None,
            None,
            None,
            LlmRequestOptions::default(),
            &["Read the requested page"],
            &store.get_messages_for_prompt(&session).unwrap(),
            &tools.api_tools,
            tools.tool_search_description.as_deref(),
            |_| {},
            |_| {},
            |_, _| {},
        )
        .await
        .unwrap();
    assert_eq!(first.tool_calls.len(), 1);
    let call = &first.tool_calls[0];
    assert_eq!(call.name, "web.run");
    let assistant = store
        .add_message(&session, MessageRole::Assistant, &first.text)
        .unwrap();
    store
        .update_message_tool_calls(&assistant, &first.tool_calls)
        .unwrap();
    let result = instance
        .execute_codex_web(&store, &serde_json::from_str(&call.arguments).unwrap())
        .await;
    assert_eq!(result.outcome, ToolRunOutcome::Done);
    store
        .add_tool_result(&session, &call.id, &result.output)
        .unwrap();
    drop(store);
    let store = SessionStore::new(root.path()).unwrap();
    let second = instance
        .call_llm(
            &store,
            None,
            None,
            None,
            LlmRequestOptions::default(),
            &["Read the requested page"],
            &store.get_messages_for_prompt(&session).unwrap(),
            &tools.api_tools,
            tools.tool_search_description.as_deref(),
            |_| {},
            |_| {},
            |_, _| {},
        )
        .await
        .unwrap();
    assert_eq!(second.text, "The native page was read.");
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "/codex/responses",
            "/codex/alpha/search",
            "/codex/responses"
        ]
    );
    let specs = &requests[0].1["input"][0]["tools"];
    assert!(specs
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["type"] == "namespace" && tool["name"] == "web"));
    assert!(!specs
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["type"] == "web_search"));
    assert_eq!(requests[1].1["id"], session);
    let input = requests[2].1["input"].as_array().unwrap();
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    assert_eq!(call["namespace"], "web");
    assert_eq!(call["name"], "run");
    assert!(input
        .iter()
        .any(|item| item["type"] == "function_call_output"
            && item["output"]
                .as_str()
                .is_some_and(|text| text.contains("Native page content"))));
    let export = root.path().join("context.yaml");
    crate::session::context_export::export_session_context_yaml(
        &store, &session, "", None, None, &export,
    )
    .unwrap();
    let exported = std::fs::read_to_string(export).unwrap();
    assert!(exported.contains("web.run"));
    assert!(exported.contains("Native page content"));
}
