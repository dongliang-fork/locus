//! Opt-in upstream contract checks. Read only the existing Locus OAuth token;
//! never refresh/write credentials or send user conversations to the service.
use super::*;
use crate::auth::codex::CodexTokenData;
use crate::commands::CodexTransportMode;
use crate::llm::codex::{self, CodexStreamOptions, CodexTransportAttempt, TurnState};

fn existing_auth() -> CodexTokenData {
    let secret = crate::keychain::get_secret(crate::keychain::KEY_CODEX_TOKENS)
        .expect("read Locus OAuth credential")
        .expect("Locus OAuth login required");
    let token: CodexTokenData = serde_json::from_str(&secret).expect("parse OAuth credential");
    assert!(
        !token.validation_failed,
        "OAuth credential requires validation"
    );
    assert!(
        token.expires_at > chrono::Utc::now().timestamp_millis() + 60_000,
        "OAuth token expired; this check does not refresh credentials"
    );
    token
}

#[tokio::test]
#[ignore = "uses existing OAuth and real inference quota; opt in with --ignored --exact"]
async fn native_web_live_oauth_websocket_accepts_reserved_schema() {
    let auth = existing_auth();
    let model =
        std::env::var("LOCUS_CODEX_WEB_TEST_MODEL").unwrap_or_else(|_| "gpt-6-astra".into());
    let history = vec![
        serde_json::from_value(json!({"id":"user_probe", "role":"user",
        "content":"Reply exactly OK. Do not call tools.", "createdAt":0}))
        .unwrap(),
    ];
    let tools = vec![tool_spec()];
    let session = uuid::Uuid::new_v4().to_string();
    let options = CodexStreamOptions::default();
    let body = codex::build_request_body_with_tool_search(
        &model,
        "You are a concise assistant.",
        &history,
        &tools,
        None,
        Some("low"),
        Some(&session),
        None,
        options.clone(),
    );
    let mut trace = codex::diagnostics::Attempt::new(
        Some(&session),
        &options,
        1,
        CodexTransportMode::Websocket,
    );
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        codex::stream_chat_websocket_once(
            &auth.access_token,
            auth.account_id.as_deref(),
            None,
            &model,
            &history,
            &tools,
            Some(&session),
            None,
            false,
            body,
            &mut TurnState::default(),
            None,
            &|_| {},
            &|_| {},
            &|_, _| {},
            Duration::from_secs(30),
            Duration::from_secs(45),
            &mut trace,
        ),
    )
    .await
    .expect("OAuth schema check timed out")
    .unwrap_or_else(|error| panic!("{}", error.replace(&auth.access_token, "[redacted]")));
    match result {
        CodexTransportAttempt::Response(response) => {
            assert!(response.response_completed);
            assert!(response.tool_calls.is_empty());
            println!(
                "Native web.run schema accepted over WebSocket: model={model}, output_tokens={}",
                response.output_tokens
            );
        }
        CodexTransportAttempt::FallbackToHttp => {
            panic!("upstream WebSocket unavailable; schema acceptance unverified")
        }
    }
}

#[tokio::test]
#[ignore = "uses existing OAuth and real web service; opt in with --ignored --exact"]
async fn native_web_live_oauth_reads_public_page() {
    let auth = existing_auth();
    let model =
        std::env::var("LOCUS_CODEX_WEB_TEST_MODEL").unwrap_or_else(|_| "gpt-6-astra".into());
    let body = request_body(
        &uuid::Uuid::new_v4().to_string(),
        &model,
        &[],
        &json!({"open":[{"ref_id":"https://example.com/"}],"response_length":"short"}),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        execute(None, &body, |refresh| {
            let token = auth.access_token.clone();
            let account = auth.account_id.clone();
            async move {
                if refresh {
                    return Err(
                        "Upstream rejected existing OAuth token; live check does not refresh it"
                            .into(),
                    );
                }
                Ok((token, account))
            }
        }),
    )
    .await
    .expect("OAuth native page check timed out")
    .unwrap_or_else(|error| panic!("{}", error.replace(&auth.access_token, "[redacted]")));
    assert!(
        result.contains("Example Domain"),
        "upstream did not return the example page"
    );
    println!(
        "Native alpha/search read example.com successfully; output_bytes={}",
        result.len()
    );
}
