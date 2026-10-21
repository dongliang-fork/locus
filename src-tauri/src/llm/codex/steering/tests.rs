use super::*;
use std::collections::VecDeque;

#[derive(Default)]
struct Source {
    queued: StdMutex<VecDeque<SteeringInput>>,
    claimed: StdMutex<Vec<String>>,
    // Claimed inputs without a receipt or successor yet.
    unresolved: StdMutex<Vec<String>>,
    committed: StdMutex<Vec<(String, bool)>>,
    rejected: StdMutex<Vec<String>>,
    released: StdMutex<Vec<String>>,
}

impl Source {
    fn queue(&self, id: &str, text: &str) {
        self.queued.lock().unwrap().push_back(SteeringInput {
            id: id.into(),
            input: user_input(text, None),
        });
    }

    fn resolve(&self, id: &str) {
        self.unresolved.lock().unwrap().retain(|item| item != id);
    }
}

impl SteeringSource for Source {
    fn claim(&self) -> Result<Option<SteeringInput>, String> {
        let input = self.queued.lock().unwrap().pop_front();
        if let Some(input) = &input {
            self.claimed.lock().unwrap().push(input.id.clone());
            self.unresolved.lock().unwrap().push(input.id.clone());
        }
        Ok(input)
    }
    fn committed(&self, id: &str, before: bool) -> Result<(), String> {
        self.resolve(id);
        self.committed.lock().unwrap().push((id.into(), before));
        Ok(())
    }
    fn rejected(&self, id: &str) {
        self.resolve(id);
        self.rejected.lock().unwrap().push(id.into());
    }
    fn has_unresolved_input(&self) -> bool {
        !self.unresolved.lock().unwrap().is_empty()
    }
    fn release_unresolved(&self) {
        // The production queue makes these claimable again; recording them is
        // enough here and keeps retries free of a second steering race.
        let released = std::mem::take(&mut *self.unresolved.lock().unwrap());
        self.released.lock().unwrap().extend(released);
    }
}

fn steering_refusal() -> Value {
    json!({"type":"error","error":{"message":"The experimental native turn lane cannot accept stateful WebSocket messages while a native turn is running. Start a new independent response.create turn instead."}})
}

fn msg(id: &str, role: &str, text: &str) -> ChatMessage {
    serde_json::from_value(json!({"id":id,"role":role,"content":text,"createdAt":0})).unwrap()
}

fn output(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
}

fn created(id: &str) -> Value {
    json!({"type":"response.created","response":{"id":id}})
}
fn accepted(parent: &str) -> Value {
    json!({"type":"response.steer.accepted","steer":{"id":format!("steer_{parent}"),"previous_response_id":parent}})
}
fn completed(id: &str, output: Value) -> Value {
    json!({"type":"response.completed","response":{"id":id,"output":[output],
        "usage":{"input_tokens":20,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}}})
}

type ServerSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
async fn recv(ws: &mut ServerSocket) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("client event timeout")
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
async fn send(ws: &mut ServerSocket, event: Value) {
    ws.send(Message::Text(event.to_string().into()))
        .await
        .unwrap();
}
async fn setup() -> (tokio::net::TcpListener, String, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let sid = uuid::Uuid::new_v4().to_string();
    let session = cached_websocket_session(&sid);
    let mut state = session.lock().await;
    state.prewarm_attempted = true;
    state.connection_key = Some(websocket_connection_key(Some(&base), None));
    (listener, base, sid)
}
async fn connect(listener: &tokio::net::TcpListener) -> ServerSocket {
    let (tcp, _) = listener.accept().await.unwrap();
    tokio_tungstenite::accept_async_with_config(tcp, Some(websocket_config()))
        .await
        .unwrap()
}
async fn run(
    base: &str,
    sid: &str,
    history: &[ChatMessage],
    metadata: &HashMap<String, Value>,
    turn: &mut TurnState,
    source: &Source,
) -> Result<LlmResponse, String> {
    tokio::time::timeout(
        Duration::from_secs(8),
        stream_chat_steerable(
            "token",
            None,
            CodexTransportMode::Websocket,
            Some(base),
            "gpt-6-astra",
            "Test",
            history,
            &[],
            None,
            Some("low"),
            false,
            Some(sid),
            Some(metadata),
            turn,
            CodexStreamOptions::default(),
            Some(source),
            &|_| {},
            &|_| {},
            &|_, _| {},
        ),
    )
    .await
    .expect("steering response timeout")
}
fn save(
    history: &mut Vec<ChatMessage>,
    metadata: &mut HashMap<String, Value>,
    id: &str,
    response: &LlmResponse,
) {
    let mut message = msg(id, "assistant", &response.text);
    message.response_id = response.response_id.clone();
    message.tool_calls = Some(response.tool_calls.clone());
    history.push(message);
    metadata.insert(id.into(), response.continuation_request.clone().unwrap());
}

#[tokio::test]
async fn automatic_successors_preserve_rounds_usage_and_target_latest_response() {
    for normal_completion in [false, true] {
        let (listener, base, sid) = setup().await;
        let server = tokio::spawn(async move {
            let mut ws = connect(&listener).await;
            assert_eq!(recv(&mut ws).await["type"], "response.create");
            send(&mut ws, created("r1")).await;
            let event = recv(&mut ws).await;
            assert_eq!(
                event,
                json!({"type":"response.steer","previous_response_id":"r1","input":user_input("first update", None)})
            );
            send(&mut ws, accepted("r1")).await;
            let mut terminal = completed("r1", output("m1", "First"));
            if !normal_completion {
                terminal["type"] = json!("response.incomplete");
                terminal["response"]["incomplete_details"] = json!({"reason":"steered"});
            }
            send(&mut ws, terminal).await;
            send(&mut ws, created("r2")).await;
            // The next client event must be steering, never another create.
            let event = recv(&mut ws).await;
            assert_eq!(event["type"], "response.steer");
            assert_eq!(event["previous_response_id"], "r2");
            send(&mut ws, accepted("r2")).await;
            send(&mut ws, completed("r2", output("m2", "Second"))).await;
            send(&mut ws, created("r3")).await;
            send(&mut ws, completed("r3", output("m3", "Done"))).await;
        });
        let source = Source::default();
        source.queue("u1", "first update");
        let mut turn = TurnState::default();
        let mut history = vec![msg("u0", "user", "Start")];
        let mut metadata = HashMap::new();
        let first = run(&base, &sid, &history, &metadata, &mut turn, &source)
            .await
            .unwrap();
        assert_eq!(first.text, "First");
        assert_eq!(first.end_turn, Some(false));
        assert_eq!(
            (
                first.input_tokens,
                first.cache_read_tokens,
                first.output_tokens
            ),
            (15, 5, 3)
        );
        assert_eq!(first.response_completed, normal_completion);
        assert!(turn.has_steering_continuation());
        save(&mut history, &mut metadata, "a1", &first);
        history.push(msg("u1", "user", "first update"));
        source.queue("u2", "second update");
        let second = run(&base, &sid, &history, &metadata, &mut turn, &source)
            .await
            .unwrap();
        assert_eq!(second.text, "Second");
        assert_eq!(second.response_id.as_deref(), Some("r2"));
        save(&mut history, &mut metadata, "a2", &second);
        history.push(msg("u2", "user", "second update"));
        let third = run(&base, &sid, &history, &metadata, &mut turn, &source)
            .await
            .unwrap();
        assert_eq!(third.text, "Done");
        assert!(!turn.has_steering_continuation());
        assert_eq!(
            *source.committed.lock().unwrap(),
            vec![("u1".into(), false), ("u2".into(), false)]
        );
        server.await.unwrap();
        invalidate_cached_session(&sid);
    }
}

#[tokio::test]
async fn tool_continuation_sends_results_once_without_replaying_accepted_input() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        assert_eq!(recv(&mut ws).await["type"], "response.steer");
        send(&mut ws, accepted("r1")).await;
        send(
            &mut ws,
            completed(
                "r1",
                json!({"type":"function_call","id":"fc_1","call_id":"call_1",
            "name":"probe","namespace":"functions","arguments":"{}","status":"completed"}),
            ),
        )
        .await;
        send(&mut ws, json!({"type":"response.steer.pending","steer":{"id":"steer_r1","previous_response_id":"r1"},
            "reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"call_1","name":"probe"}]})).await;
        let event = recv(&mut ws).await;
        assert_eq!(event["previous_response_id"], "r1");
        assert_eq!(
            event["input"],
            json!([{"type":"function_call_output","call_id":"call_1","output":"saved result"}])
        );
        send(&mut ws, created("r2")).await;
        send(&mut ws, completed("r2", output("m2", "Updated"))).await;
    });
    let source = Source::default();
    source.queue("u1", "update");
    let mut turn = TurnState::default();
    let mut history = vec![msg("u0", "user", "Start")];
    let mut metadata = HashMap::new();
    let first = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(first.tool_calls.len(), 1);
    assert!(source.committed.lock().unwrap().is_empty());
    save(&mut history, &mut metadata, "a1", &first);
    let mut result = msg("t1", "tool", "saved result");
    result.tool_call_id = Some("call_1".into());
    history.push(result);
    let second = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(second.text, "Updated");
    assert_eq!(*source.committed.lock().unwrap(), vec![("u1".into(), true)]);
    server.await.unwrap();
    invalidate_cached_session(&sid);
}

#[tokio::test]
async fn rejected_steering_returns_to_normal_follow_up_without_stopping_output() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        recv(&mut ws).await;
        send(&mut ws, json!({"type":"response.steer.failed","steer":{"previous_response_id":"r1","input":user_input("update",None)},
            "error":{"code":"steering_not_supported"}})).await;
        send(&mut ws, completed("r1", output("msg_m1", "Original"))).await;
        let follow_up = recv(&mut ws).await;
        assert_eq!(follow_up["previous_response_id"], "r1");
        send(&mut ws, created("r2")).await;
        send(&mut ws, completed("r2", output("msg_m2", "Continued"))).await;
    });
    let source = Source::default();
    source.queue("u1", "update");
    let mut turn = TurnState::default();
    let mut history = vec![msg("u0", "user", "Start")];
    let mut metadata = HashMap::new();
    let first = run(
        &base,
        &sid,
        &history,
        &metadata,
        &mut turn,
        &source,
    )
    .await
    .unwrap();
    assert_eq!(first.text, "Original");
    assert_eq!(first.end_turn, Some(false));
    assert!(!turn.has_steering_continuation());
    assert_eq!(*source.rejected.lock().unwrap(), vec!["u1"]);
    save(&mut history, &mut metadata, "a1", &first);
    history.push(msg("u1", "user", "update"));
    let follow_up = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(follow_up.text, "Continued");
    server.await.unwrap();
    invalidate_cached_session(&sid);
}

#[tokio::test]
async fn disconnect_mid_response_releases_input_and_replays_local_history() {
    for acknowledge in [false, true] {
        let (listener, base, sid) = setup().await;
        let server = tokio::spawn(async move {
            let mut ws = connect(&listener).await;
            recv(&mut ws).await;
            send(&mut ws, created("r1")).await;
            recv(&mut ws).await;
            if acknowledge {
                send(&mut ws, accepted("r1")).await;
            }
            ws.close(None).await.unwrap();
            // The retry is a full replay on a new socket; the unsaved input is
            // not part of it, so the server cannot see the input twice.
            let mut ws = connect(&listener).await;
            let replay = recv(&mut ws).await;
            assert!(replay.get("previous_response_id").is_none());
            assert!(!replay["input"].to_string().contains("update"));
            send(&mut ws, created("r2")).await;
            send(&mut ws, completed("r2", output("m2", "Recovered"))).await;
        });
        let source = Source::default();
        source.queue("u1", "update");
        let response = run(
            &base,
            &sid,
            &[msg("u0", "user", "Start")],
            &HashMap::new(),
            &mut TurnState::default(),
            &source,
        )
        .await
        .unwrap();
        assert_eq!(response.text, "Recovered");
        assert_eq!(*source.released.lock().unwrap(), vec!["u1"]);
        assert!(source.committed.lock().unwrap().is_empty());
        assert!(!source.has_unresolved_input());
        server.await.unwrap();
        invalidate_cached_session(&sid);
    }
}

#[tokio::test]
async fn refused_steering_is_disabled_and_the_request_is_retried_without_it() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        assert_eq!(recv(&mut ws).await["type"], "response.steer");
        send(&mut ws, steering_refusal()).await;

        let mut ws = connect(&listener).await;
        let replay = recv(&mut ws).await;
        assert_eq!(replay["type"], "response.create");
        assert!(replay.get("previous_response_id").is_none());
        send(&mut ws, created("r2")).await;
        // A second queued input must not be steered once the lane refused it.
        assert!(tokio::time::timeout(Duration::from_millis(250), ws.next())
            .await
            .is_err());
        send(&mut ws, completed("r2", output("m2", "Done"))).await;
    });
    let source = Source::default();
    source.queue("u1", "first");
    source.queue("u2", "second");
    let response = run(
        &base,
        &sid,
        &[msg("u0", "user", "Start")],
        &HashMap::new(),
        &mut TurnState::default(),
        &source,
    )
    .await
    .unwrap();
    assert_eq!(response.text, "Done");
    assert_eq!(*source.released.lock().unwrap(), vec!["u1"]);
    assert_eq!(source.queued.lock().unwrap().len(), 1);
    server.await.unwrap();
    invalidate_cached_session(&sid);
}

#[tokio::test]
async fn refusal_after_visible_output_is_not_retried_by_the_transport() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        send(&mut ws, json!({"type":"response.output_text.delta","item_id":"m1","output_index":0,
            "content_index":0,"delta":"partial"})).await;
        assert_eq!(recv(&mut ws).await["type"], "response.steer");
        send(&mut ws, steering_refusal()).await;
        assert!(tokio::time::timeout(Duration::from_millis(1500), listener.accept())
            .await
            .is_err());
    });
    let source = Source::default();
    source.queue("u1", "update");
    let error = run(
        &base,
        &sid,
        &[msg("u0", "user", "Start")],
        &HashMap::new(),
        &mut TurnState::default(),
        &source,
    )
    .await
    .unwrap_err();
    assert!(error.contains("steering was refused"), "{error}");
    assert!(!error.contains("text_len=0"), "{error}");
    assert_eq!(*source.released.lock().unwrap(), vec!["u1"]);
    server.await.unwrap();
    invalidate_cached_session(&sid);
}

#[tokio::test]
async fn connection_lost_while_awaiting_successor_keeps_the_terminal_response() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        recv(&mut ws).await;
        send(&mut ws, accepted("r1")).await;
        send(
            &mut ws,
            json!({"type":"response.incomplete","response":{"id":"r1",
            "incomplete_details":{"reason":"steered"},"output":[output("msg_m1","Preserved")]}}),
        )
        .await;
        ws.close(None).await.unwrap();

        let mut ws = connect(&listener).await;
        let replay = recv(&mut ws).await;
        assert!(replay.get("previous_response_id").is_none());
        assert!(replay["input"].to_string().contains("update"));
        send(&mut ws, created("r2")).await;
        send(&mut ws, completed("r2", output("msg_m2", "Continued"))).await;
    });
    let source = Source::default();
    source.queue("u1", "update");
    let mut turn = TurnState::default();
    let mut history = vec![msg("u0", "user", "Start")];
    let mut metadata = HashMap::new();
    let response = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(response.text, "Preserved");
    assert_eq!(response.end_turn, Some(false));
    assert!(!turn.has_steering_continuation());
    assert_eq!(*source.rejected.lock().unwrap(), vec!["u1"]);
    assert!(source.released.lock().unwrap().is_empty());
    save(&mut history, &mut metadata, "a1", &response);
    history.push(msg("u1", "user", "update"));
    let follow_up = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(follow_up.text, "Continued");
    server.await.unwrap();
    invalidate_cached_session(&sid);
}

#[test]
fn receipts_are_correlated_and_user_messages_have_only_allowed_fields() {
    assert!(supports_steering("openai/gpt-6-astra"));
    assert!(!supports_steering("gpt-5.6"));
    let mut state = Submission::default();
    let wire = state.send(
        SteeringInput {
            id: "u".into(),
            input: user_input("hello", None),
        },
        "r",
    );
    assert_eq!(wire.as_object().unwrap().len(), 3);
    assert_eq!(wire["input"][0].as_object().unwrap().len(), 2);
    assert!(state.event(&accepted("other"), None).is_err());
    state.event(&accepted("r"), None).unwrap();
    assert!(state.event(&json!({"type":"response.steer.failed","steer":{"id":"wrong","previous_response_id":"r"}}), None).is_err());
}

#[tokio::test]
async fn failure_after_acceptance_returns_uncommitted_input_after_terminal_response() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        recv(&mut ws).await;
        send(&mut ws, accepted("r1")).await;
        send(
            &mut ws,
            json!({"type":"response.incomplete","response":{"id":"r1",
            "incomplete_details":{"reason":"steered"},"output":[output("msg_m1","Preserved")]}}),
        )
        .await;
        send(
            &mut ws,
            json!({"type":"response.steer.failed","steer":{"id":"steer_r1",
            "previous_response_id":"r1","input":user_input("update",None)},
            "error":{"code":"successor_creation_failed"}}),
        )
        .await;
        drop(ws);

        let mut ws = connect(&listener).await;
        let replay = recv(&mut ws).await;
        assert!(replay.get("previous_response_id").is_none());
        assert!(replay["input"]
            .as_array()
            .unwrap()
            .contains(&output("msg_m1", "Preserved")));
        assert!(replay["input"].to_string().contains("update"));
        send(&mut ws, created("r2")).await;
        send(&mut ws, completed("r2", output("msg_m2", "Continued"))).await;
    });
    let source = Source::default();
    source.queue("u1", "update");
    let mut turn = TurnState::default();
    let mut history = vec![msg("u0", "user", "Start")];
    let mut metadata = HashMap::new();
    let response = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(response.text, "Preserved");
    assert_eq!(response.finish_reason, "steered");
    assert_eq!(response.end_turn, Some(false));
    assert!(!turn.has_steering_continuation());
    assert!(source.committed.lock().unwrap().is_empty());
    assert_eq!(*source.rejected.lock().unwrap(), vec!["u1"]);
    save(&mut history, &mut metadata, "a1", &response);
    history.push(msg("u1", "user", "update"));
    let follow_up = run(&base, &sid, &history, &metadata, &mut turn, &source)
        .await
        .unwrap();
    assert_eq!(follow_up.text, "Continued");
    server.await.unwrap();
    invalidate_cached_session(&sid);
}

#[tokio::test]
async fn older_models_leave_pending_input_for_the_normal_queue() {
    let (listener, base, sid) = setup().await;
    let server = tokio::spawn(async move {
        let mut ws = connect(&listener).await;
        recv(&mut ws).await;
        send(&mut ws, created("r1")).await;
        assert!(tokio::time::timeout(Duration::from_millis(150), ws.next())
            .await
            .is_err());
        send(&mut ws, completed("r1", output("m1", "Done"))).await;
    });
    let source = Source::default();
    source.queue("u1", "update");
    let response = stream_chat_steerable(
        "token",
        None,
        CodexTransportMode::Websocket,
        Some(&base),
        "gpt-5.6",
        "Test",
        &[msg("u0", "user", "Start")],
        &[],
        None,
        None,
        false,
        Some(&sid),
        None,
        &mut TurnState::default(),
        CodexStreamOptions::default(),
        Some(&source),
        &|_| {},
        &|_| {},
        &|_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(response.text, "Done");
    assert!(source.claimed.lock().unwrap().is_empty());
    assert_eq!(source.queued.lock().unwrap().len(), 1);
    server.await.unwrap();
    invalidate_cached_session(&sid);
}
