use super::*;
use serde_json::json;

#[test]
fn retry_recognizes_completed_native_answer_but_not_old_or_unfinished_output() {
    let mut native = tests::snapshot();
    native.pending_tool_ids.clear();
    native.messages[1].stop_reason = Some("end_turn".into());
    let receipt = json!({"claude_checkpoint":{"uuid":native.messages[0].uuids[0]}});
    assert!(completed_native_message(&native, Some(&receipt), &[]).is_some());
    assert!(completed_native_message(&native, None, &[]).is_none());
    assert!(completed_native_message(
        &native,
        Some(&json!({"claude_checkpoint":{"uuid":uuid::Uuid::new_v4()}})),
        &[]
    )
    .is_none());
    assert!(
        completed_native_message(&native, Some(&receipt), &["unsent steering".into()]).is_none()
    );
    native.incomplete_tail = true;
    assert!(completed_native_message(&native, Some(&receipt), &[]).is_none());
    native.incomplete_tail = false;
    native.pending_tool_ids.push("unknown write".into());
    assert!(completed_native_message(&native, Some(&receipt), &[]).is_none());
    native.pending_tool_ids.clear();
    native.messages[1].stop_reason = None;
    assert!(completed_native_message(&native, Some(&receipt), &[]).is_none());
    native.messages[1].stop_reason = Some("max_tokens".into());
    assert!(completed_native_message(&native, Some(&receipt), &[]).is_none());
}

#[test]
fn retry_completion_can_use_input_token_when_native_ack_was_lost() {
    let mut native = tests::snapshot();
    native.pending_tool_ids.clear();
    native.messages[1].stop_reason = Some("end_turn".into());
    let token = uuid::Uuid::new_v4().to_string();
    native.messages[0].content = json!(pending_input_text(&token, "write once"));
    let receipt = json!({"claude_pending_input":{"version":1,"token":token}});
    assert!(completed_native_message(&native, Some(&receipt), &[]).is_some());
}

#[test]
fn retry_rebuild_keeps_every_proven_unsent_input_and_does_not_change_identity() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let id = store
        .create_session("automatic recovery", None, None, "chat", None)
        .unwrap();
    let remote: ssh::SshConfig =
        serde_json::from_value(json!({"target":"unused.invalid"})).unwrap();
    let native = tests::snapshot();
    store
        .bind_claude_native_session(&id, &remote, &native.native_id)
        .unwrap();
    store
        .reconcile_claude_native(&id, &remote, &native, &[], None)
        .unwrap();
    let first = store
        .add_message(&id, MessageRole::User, "queued instruction one")
        .unwrap();
    let second = store
        .add_message(&id, MessageRole::User, "queued instruction two")
        .unwrap();
    for message in [&first, &second] {
        store
            .set_message_response_request_metadata(
                &id,
                message,
                &json!({
                    "backend":"claude_code_cli", "claude_connection":remote,
                    "claude_pending_input":{"version":1,"token":uuid::Uuid::new_v4()},
                }),
            )
            .unwrap();
    }
    let before = store.get_messages(&id).unwrap();
    store
        .rebuild_claude_native_for_retry(&id, &remote, &native, &before, Some(&first))
        .unwrap();
    let after = store.get_messages(&id).unwrap();
    assert_eq!(after[after.len() - 2].id, first);
    assert_eq!(after.last().unwrap().id, second);
    let metadata = store.get_response_request_metadata(&id).unwrap();
    assert_eq!(
        store
            .claude_recovery_pending_messages(&id, &after, &metadata, Some(&first))
            .unwrap(),
        [first, second]
    );
    let mut wrong = native.clone();
    wrong.native_id = uuid::Uuid::new_v4().to_string();
    assert!(store
        .rebuild_claude_native_for_retry(&id, &remote, &wrong, &after, None)
        .is_err());
    assert_eq!(
        native_session_id(&store, &id).unwrap().as_deref(),
        Some(native.native_id.as_str())
    );
}

#[tokio::test]
async fn transport_retry_reuses_completed_tool_receipt_instead_of_reexecuting_effect() {
    use super::super::{native_journal::Journal, turn_retry};
    use std::sync::atomic::{AtomicU32, Ordering};
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::new(root.path().into());
    let executions = AtomicU32::new(0);
    let synchronizations = AtomicU32::new(0);
    let (_send, receive) = tokio::sync::watch::channel(false);
    let call = json!({"callId":"write-once","tool":"Write","input":{"file_path":"asset","content":"updated"}});
    let result = turn_retry::run(
        |attempt| {
            if journal.begin(&call).unwrap().is_none() {
                executions.fetch_add(1, Ordering::SeqCst);
                std::fs::write(root.path().join("effect"), "updated once").unwrap();
                journal
                    .complete(
                        "write-once",
                        &json!({"callId":"write-once","result":{"type":"create"}}),
                    )
                    .unwrap();
            }
            async move {
                if attempt == 1 {
                    Err(turn_retry::disconnected("lost tool result ACK"))
                } else {
                    Ok(())
                }
            }
        },
        |_, _| {
            synchronizations.fetch_add(1, Ordering::SeqCst);
            let mut native = tests::snapshot();
            native.pending_tool_ids = vec!["write-once".into()];
            let notice = native.resume_context(&journal).unwrap().unwrap();
            assert!(notice.contains("completed") && notice.contains("Do not retry"));
            async { Ok(()) }
        },
        receive,
        &[Duration::ZERO],
    )
    .await
    .unwrap();
    assert_eq!(result, Some(()));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(synchronizations.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("effect")).unwrap(),
        "updated once"
    );
}
