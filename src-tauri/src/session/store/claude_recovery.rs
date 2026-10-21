use super::*;
use crate::llm::claude_code_cli::{recovery::{self, NativeMessage, NativeSnapshot}, ssh::SshConfig};
use serde_json::{json, Value};

#[cfg(test)]
#[path = "claude_recovery_parts_tests.rs"]
mod parts_tests;

fn pending_input_matches(metadata: Option<&Value>, native: &NativeMessage) -> bool {
    if native.role != MessageRole::User { return false; }
    let Some(receipt) = metadata.and_then(|v|v.get("claude_pending_input")) else { return false; };
    receipt["version"] == 1 && receipt["token"].as_str().is_some_and(|token|
        recovery::pending_input_token(&recovery::text_content(&native.content)) == Some(token))
}

fn authoritative_user_text(native:&NativeMessage,existing:Option<&ChatMessage>)->String {
    let raw=recovery::text_content(&native.content);
    let text=if recovery::pending_input_token(&raw).is_some() {
        raw.split_once('\n').and_then(|(_,body)|body.rsplit_once("\n</locus-pending-user>").map(|(body,_)|body)).unwrap_or(&raw)
    } else {&raw};
    if let Some(message)=existing.filter(|message|crate::session::history::render_prompt_content(
        &message.content,message.prompt_prefix.as_deref(),message.prompt_suffix.as_deref())==text) {
        return message.content.clone();
    }
    text.to_string()
}

fn matches_native(message: &ChatMessage, metadata: Option<&Value>, native: &NativeMessage, exact: bool) -> bool {
    if is_context_handoff_message(message) && metadata.is_some_and(|v| v["claude_compaction"].is_object()) {
        // A live compact receipt points at the preceding native row, while
        // its local assistant handoff represents the following user summary.
        // Do not bind the handoff to that preceding assistant checkpoint.
        let summary = recovery::text_content(&native.content);
        return !exact && native.compact_summary && !summary.is_empty() && metadata.is_some_and(|v|
            v["claude_compaction"]["summary"].as_str().is_some_and(|text| text.contains(&summary)));
    }
    if message.role != native.role { return false; }
    if pending_input_matches(metadata,native) { return true; }
    if native.role == MessageRole::Tool && message.tool_call_id != native.tool_call_id { return false; }
    if let Some(value) = metadata {
        if value["claude_native"]["key"].as_str() == Some(native.key.as_str()) { return true; }
        if value["claude_checkpoint"]["uuid"].as_str().is_some_and(|id| native.uuids.iter().any(|n| n == id)) { return true; }
    }
    if exact { return false; }
    if native.role == MessageRole::Tool { return true; }
    if native.role == MessageRole::Assistant {
        let parsed = recovery::assistant_content(&native.content);
        if message.tool_calls.iter().flatten().any(|t| parsed.tool_calls.iter().any(|n| n.id == t.id)) { return true; }
        return !message.content.is_empty() && parsed.text.starts_with(&message.content);
    }
    let text = recovery::text_content(&native.content);
    !message.content.is_empty() && (text == message.content || text.contains(&message.content))
}

/// Migrate checkpoint-only tool rounds (and previously synchronized rounds)
/// without merging their local IDs: undo records, captures and context views
/// can still refer to each individual assistant message. Only distinct native
/// UUIDs with verified source boundaries establish a safe many-to-one match.
fn reconcile_assistant_parts<'a>(
    native: &NativeMessage,
    candidates: &[&'a ChatMessage],
    metadata: &HashMap<String, Value>,
    native_id: &str,
) -> Result<Vec<(NativeMessage, Option<&'a ChatMessage>)>, String> {
    let ambiguous = || format!("Multiple local messages match native Claude record {}; their source boundaries could not be verified. Synchronization stopped without changing history", native.key);
    if native.role != MessageRole::Assistant || native.parts.is_empty() {
        return Err(ambiguous());
    }
    let mut anchors = Vec::new();
    let mut seen = HashSet::new();
    for message in candidates {
        let checkpoint = &metadata[&message.id]["claude_checkpoint"];
        if checkpoint["sessionId"].as_str().is_some_and(|id| id != native_id) { return Err(ambiguous()); }
        let index = if let Some(uuid) = checkpoint["uuid"].as_str() {
            native.parts.iter().position(|part| part.uuid == uuid).ok_or_else(ambiguous)?
        } else {
            // Older CLI records may predate checkpoints. Distinct tool call
            // identities can prove the boundary; repeated text cannot.
            let calls = message.tool_calls.as_ref().filter(|calls| !calls.is_empty()).ok_or_else(ambiguous)?;
            let mut last = 0;
            for call in calls {
                let indices = native.parts.iter().enumerate().filter(|(_, part)| part.content.iter()
                    .any(|block| block["type"] == "tool_use" && block["id"] == call.id)).map(|(index, _)| index).collect::<Vec<_>>();
                if indices.len() != 1 { return Err(ambiguous()); }
                last = last.max(indices[0]);
            }
            last
        };
        if !seen.insert(index) { return Err(ambiguous()); }
        anchors.push((index, *message));
    }
    anchors.sort_by_key(|(index, _)| *index);
    let mut start = 0;
    let mut result = Vec::new();
    for (position, (anchor, message)) in anchors.iter().enumerate() {
        let last = position + 1 == anchors.len();
        let end = if last { native.parts.len() } else { anchor + 1 };
        let parts = &native.parts[start..end];
        let mut fragment = native.clone();
        fragment.key = format!("{}:part:{}", native.key, parts.last().unwrap().uuid);
        fragment.uuids = parts.iter().map(|part| part.uuid.clone()).collect();
        fragment.content = Value::Array(parts.iter().flat_map(|part| part.content.clone()).collect());
        fragment.timestamp = parts.first().and_then(|part| part.timestamp.clone());
        fragment.parts = parts.to_vec();
        // Usage describes the API response, not each persisted tool round.
        fragment.usage = if last { native.usage.clone() } else { None };
        let parsed = recovery::assistant_content(&fragment.content);
        if message.tool_calls.iter().flatten().any(|tool| !parsed.tool_calls.iter().any(|t| t.id == tool.id))
            || (!message.content.is_empty() && !parsed.text.starts_with(&message.content)) {
            return Err(ambiguous());
        }
        result.push((fragment, Some(*message)));
        start = end;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_fork_passive_reconciliation_rechecks_active_run_in_transaction() {
        for status in ["queued", "starting", "running", "waiting_input", "finishing", "cancelling"] {
            let root = tempfile::tempdir().unwrap();
            let store = SessionStore::new(root.path()).unwrap();
            let id = store.create_session("fork race", None, None, "chat", None).unwrap();
            let (remote, mut native) = fixture();
            store.reconcile_claude_native(&id, &remote, &native, &[], None).unwrap();
            let pending = store.add_message_with_images_asset_refs_signature_and_response(
                &id, MessageRole::User, "new fork request", None, None, None, None, None,
                Some(&json!({"backend":"claude_code_cli", "claude_pending_input":{"version":1,"token":Uuid::new_v4()}})),
            ).unwrap();
            let expected = store.get_messages(&id).unwrap();
            native.digest = "b".repeat(64);
            native.messages[1].content = json!([{"type":"text","text":"changed remote answer"}]);
            // Simulate a run starting after the caller's idle check / snapshot.
            store.try_start_run(&id, "active").unwrap();
            store.update_run_status("active", status, None).unwrap();
            assert!(!store.reconcile_claude_native_binding(
                &id, &remote, &native, &expected, None, false, false, false, true,
            ).unwrap(), "{status}");
            let after = store.get_messages(&id).unwrap();
            assert_eq!(serde_json::to_value(&after).unwrap(), serde_json::to_value(&expected).unwrap());
            assert_eq!(after.last().unwrap().id, pending);
        }
    }

    fn fixture() -> (SshConfig, NativeSnapshot) {
        let remote: SshConfig = serde_json::from_value(json!({"target":"test@127.0.0.1","port":22242})).unwrap();
        let row = |role, content, key: &str| json!({"key":key,"role":role,"uuids":[Uuid::new_v4().to_string()],"content":content,"timestamp":"2026-10-15T00:00:00Z"});
        let user = row("user", json!([{"type":"text","text":"question"}]), "user1");
        let mut answer = row("assistant", json!([{"type":"text","text":"complete answer"}]), "assistant1");
        answer["model"] = json!("deepseek-flash");
        answer["usage"] = json!({"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":0});
        let snapshot = serde_json::from_value(json!({"version":1,"nativeId":Uuid::new_v4().to_string(),"digest":"a".repeat(64),
            "messages":[user,answer],"todos":[{"content":"native task","status":"completed","priority":"medium"}],"incompleteTail":false,"pendingToolIds":[]})).unwrap();
        (remote,snapshot)
    }

    #[test]
    fn claude_compact_resume_preserves_marker_native_preview_and_pending_input() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let id = store.create_session("compact resume", None, None, "chat", None).unwrap();
        let (remote, mut snapshot) = fixture();
        let user = store.add_message(&id, MessageRole::User, "question").unwrap();
        let answer = store.add_message(&id, MessageRole::Assistant, "complete answer").unwrap();
        for (local, native) in [(&user, &snapshot.messages[0]), (&answer, &snapshot.messages[1])] {
            store.set_message_response_request_metadata(&id, local, &json!({
                "backend":"claude_code_cli", "claude_connection":remote,
                "claude_checkpoint":{"version":1,"uuid":native.uuids[0],"sessionId":snapshot.native_id}
            })).unwrap();
        }
        let summary = "This session is being continued from a previous conversation.\n\n## Summary\nNative result only.";
        let handoff = crate::compact::build_post_compact_message(summary, "", store.get_messages(&id).unwrap()[1].created_at, false, None);
        let receipt = json!({"backend":"claude_code_cli", "claude_connection":remote,
            "claude_checkpoint":{"version":1,"uuid":snapshot.messages[1].uuids[0],"sessionId":snapshot.native_id},
            "claude_compaction":{"type":"compact_done","summary":summary}});
        store.compact_messages_with_response_request(&id, &handoff, &answer, 0, Some(&receipt)).unwrap();
        let preview = store.get_compacted_context_output(&id, &handoff.id).unwrap().unwrap();
        assert_eq!(preview.compaction_kind, "claudeNative");
        assert_eq!(preview.messages.len(), 1);
        assert_eq!(preview.messages[0].content, summary);
        assert!(!preview.messages[0].prompt_prefix_placeholder);
        assert_eq!(preview.checkpoint, json!("empty"));
        let pending = store.add_message(&id, MessageRole::User, "continue").unwrap();

        let mut native_summary = snapshot.messages[0].clone();
        native_summary.key = "native-summary".into();
        native_summary.uuids = vec![Uuid::new_v4().to_string()];
        native_summary.content = json!(summary);
        native_summary.compact_summary = true;
        snapshot.messages.push(native_summary.clone());
        let expected = store.get_messages(&id).unwrap();
        assert!(expected[2].created_at < expected[1].created_at, "summary is backdated for the prompt");
        assert_eq!(expected[2].id, handoff.id, "transcript remains in insertion order");
        store.conn.lock().unwrap().execute("UPDATE messages SET content='concurrent edit' WHERE id=?1", [&answer]).unwrap();
        assert!(store.reconcile_claude_native(&id, &remote, &snapshot, &expected, Some(&pending))
            .unwrap_err().contains("content changed"));
        assert_eq!(store.get_messages(&id).unwrap()[1].content, "concurrent edit");
        store.conn.lock().unwrap().execute("UPDATE messages SET content='complete answer' WHERE id=?1", [&answer]).unwrap();
        assert!(store.reconcile_claude_native(&id, &remote, &snapshot, &expected, Some(&pending)).unwrap());
        let recovered = store.get_messages(&id).unwrap();
        assert_eq!(recovered.iter().map(|m| &m.id).collect::<Vec<_>>(), vec![&user, &answer, &handoff.id, &pending]);
        assert!(!store.reconcile_claude_native(&id, &remote, &snapshot, &recovered, Some(&pending)).unwrap());

        // The next CLI turn consumes the pending input and can prune old native
        // rows after compression. Local archives and the preview link survive.
        let mut continued = snapshot.messages[0].clone();
        continued.key = "continued-user".into();
        continued.uuids = vec![Uuid::new_v4().to_string()];
        continued.content = json!("continue");
        store.set_message_response_request_metadata(&id, &pending, &json!({
            "backend":"claude_code_cli", "claude_connection":remote,
            "claude_checkpoint":{"version":1,"uuid":continued.uuids[0],"sessionId":snapshot.native_id}
        })).unwrap();
        let mut reply = snapshot.messages[1].clone();
        reply.key = "continued-answer".into();
        reply.uuids = vec![Uuid::new_v4().to_string()];
        reply.content = json!([{"type":"text","text":"continued natively"}]);
        snapshot.messages = vec![native_summary, continued, reply];
        snapshot.digest = "b".repeat(64);
        assert!(store.reconcile_claude_native(&id, &remote, &snapshot, &store.get_messages(&id).unwrap(), None).unwrap());
        assert_eq!(store.get_messages(&id).unwrap().len(), 5);
        drop(store);
        let store = SessionStore::new(root.path()).unwrap();
        assert!(!store.reconcile_claude_native(&id, &remote, &snapshot, &store.get_messages(&id).unwrap(), None).unwrap());
        let preview = store.get_compacted_context_output(&id, &handoff.id).unwrap().unwrap();
        assert_eq!(preview.messages[0].content, summary);
        let output = root.path().join("compacted.yaml");
        crate::session::context_export::export_session_context_yaml(&store, &id, "F:/test", None, None, &output).unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(yaml["sessions"][0]["compactions"][0]["compactionKind"].as_str(), Some("claudeNative"));
        assert_eq!(yaml["sessions"][0]["compactions"][0]["messages"][0]["content"].as_str(), Some(summary));
    }

    #[test]
    fn claude_native_preview_does_not_invent_missing_historical_output() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let id = store.create_session("legacy compact", None, None, "chat", None).unwrap();
        let user = store.add_message(&id, MessageRole::User, "question").unwrap();
        let handoff = crate::compact::build_post_compact_message("local fallback must not be shown", "", 1, false, None);
        store.compact_messages_with_response_request(&id, &handoff, &user, 0,
            Some(&json!({"backend":"claude_code_cli","claude_compaction":{"type":"compact_done"}}))).unwrap();
        let preview = store.get_compacted_context_output(&id, &handoff.id).unwrap().unwrap();
        assert_eq!(preview.compaction_kind, "claudeNative");
        assert_eq!(preview.snapshot_status, "partial");
        assert_eq!(preview.messages[0].content, "empty");
        let output = root.path().join("legacy.yaml");
        crate::session::context_export::export_session_context_yaml(&store, &id, "F:/test", None, None, &output).unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(yaml["sessions"][0]["compactions"][0]["messages"][0]["content"].as_str(), Some("empty"));
    }

    #[test]
    fn claude_restore_commits_server_binding_with_history_and_session_tasks() {
        let root=tempfile::tempdir().unwrap();let store=SessionStore::new(root.path()).unwrap();
        let id=store.create_session("restore",None,None,"chat",None).unwrap();
        let (source,mut snapshot)=fixture();
        snapshot.task_snapshot=json!({"version":1,"kind":"tasks","items":snapshot.todos,"tasks":[]});
        store.reconcile_claude_native(&id,&source,&snapshot,&[],None).unwrap();
        let messages=store.get_messages(&id).unwrap();
        let mut target=source.clone();target.target="test@other".into();
        assert!(store.restore_claude_native_binding(&id,&target,&snapshot,&[]).is_err());
        assert!(store.claude_active_remote(&id).unwrap().is_none());
        store.restore_claude_native_binding(&id,&target,&snapshot,&messages).unwrap();
        assert_eq!(store.claude_active_remote(&id).unwrap(),Some(target.clone()));
        assert_eq!(store.get_messages(&id).unwrap().iter().map(|m|&m.id).collect::<Vec<_>>(),messages.iter().map(|m|&m.id).collect::<Vec<_>>());
        assert_eq!(store.latest_cli_session_id(&id,"claude_code").unwrap(),Some(snapshot.native_id));
        assert!(store.get_todos(&id).unwrap().session_scoped);
        assert!(store.get_response_request_metadata(&id).unwrap().values().all(|v|v["claude_connection"]==json!(target)));
    }
    #[test]
    fn claude_recovery_migrates_partial_history_idempotently_and_exports_after_reopen() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let id = store.create_session("recovery", None, None, "chat", None).unwrap();
        let user = store.add_message(&id, MessageRole::User, "question").unwrap();
        let answer = store.add_message(&id, MessageRole::Assistant, "complete").unwrap();
        let (remote, snapshot) = fixture();
        store.set_session_last_model_id(&id, "claude_code/opus").unwrap();
        // Previous checkpoint-only records are migrated explicitly by native UUID.
        for (local,native) in [(&user,&snapshot.messages[0]),(&answer,&snapshot.messages[1])] {
            store.set_message_response_request_metadata(&id, local, &json!({"model":"claude_code/opus","claude_connection":remote,
                "claude_checkpoint":{"version":1,"uuid":native.uuids[0],"sessionId":snapshot.native_id}})).unwrap();
        }
        let expected = store.get_messages(&id).unwrap();
        assert!(store.reconcile_claude_native(&id,&remote,&snapshot,&expected,None).unwrap());
        let recovered = store.get_messages(&id).unwrap();
        assert_eq!(recovered[1].id,answer);
        assert_eq!(recovered[1].content,"complete answer");
        let metadata = store.get_response_request_metadata(&id).unwrap();
        assert_eq!(metadata[&answer]["model"], "claude_code/opus");
        assert_eq!(metadata[&answer]["upstream_model"], "deepseek-flash");
        assert_eq!(store.load_session(&id).unwrap().last_model_id.as_deref(), Some("claude_code/opus"));
        assert_eq!(store.get_todos(&id).unwrap().items[0].status,"completed");
        assert_eq!(store.latest_cli_session_id(&id,"claude_code").unwrap(),Some(snapshot.native_id.clone()));
        assert!(!store.reconcile_claude_native(&id,&remote,&snapshot,&recovered,None).unwrap());
        assert_eq!(store.get_token_usage(&id).unwrap().total_output_tokens,7);
        drop(store);
        let store = SessionStore::new(root.path()).unwrap();
        assert!(!store.reconcile_claude_native(&id,&remote,&snapshot,&store.get_messages(&id).unwrap(),None).unwrap());
        let output=root.path().join("recovered.yaml");
        crate::session::context_export::export_session_context_yaml(&store,&id,"F:/test",None,None,&output).unwrap();
        let yaml:serde_yaml::Value=serde_yaml::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(yaml["sessions"][0]["messages"][1]["claudeNative"]["version"].as_u64(),Some(1));
        assert_eq!(yaml["sessions"][0]["messages"][1]["content"].as_str(),Some("complete answer"));
    }

    #[test]
    fn claude_recovery_rejects_foreign_and_concurrent_history_preserves_pending_input() {
        let root=tempfile::tempdir().unwrap();
        let store=SessionStore::new(root.path()).unwrap();
        let id=store.create_session("guard",None,None,"chat",None).unwrap();
        let old=store.add_message(&id,MessageRole::User,"question").unwrap();
        let (remote,snapshot)=fixture();
        let expected=store.get_messages(&id).unwrap();
        assert!(store.reconcile_claude_native(&id,&remote,&snapshot,&expected,None).is_err());
        store.set_message_response_request_metadata(&id,&old,&json!({"backend":"claude_code_cli","claude_connection":remote})).unwrap();
        let pending=store.add_message(&id,MessageRole::User,"new unsent input").unwrap();
        assert!(store.reconcile_claude_native(&id,&remote,&snapshot,&expected,None).unwrap_err().contains("changed"));
        assert!(store.reconcile_claude_native(&id,&remote,&snapshot,&store.get_messages(&id).unwrap(),Some(&pending)).unwrap());
        let recovered=store.get_messages(&id).unwrap();
        assert_eq!(recovered.len(),3);
        assert_eq!(recovered[2].id,pending);
        let mut unsafe_snapshot=snapshot.clone();
        unsafe_snapshot.pending_tool_ids.push("unknown-side-effect".into());
        assert!(unsafe_snapshot.can_continue().is_err());
        unsafe_snapshot.pending_tool_ids.clear();
        unsafe_snapshot.incomplete_tail=true;
        assert!(unsafe_snapshot.can_continue().is_err());
    }

    #[test]
    fn claude_recovery_keeps_pre_compact_history_and_rebuilds_context_view() {
        let root=tempfile::tempdir().unwrap();
        let store=SessionStore::new(root.path()).unwrap();
        let id=store.create_session("compact recovery",None,None,"chat",None).unwrap();
        let (remote,mut snapshot)=fixture();
        let mut summary=snapshot.messages[0].clone();
        summary.key="summary".into();
        summary.uuids=vec![Uuid::new_v4().to_string()];
        summary.content=json!("native compact summary");
        summary.compact_summary=true;
        snapshot.messages.push(summary);
        assert!(store.reconcile_claude_native(&id,&remote,&snapshot,&[],None).unwrap());
        assert_eq!(store.get_messages(&id).unwrap().len(),3);
        let context=store.get_messages_for_prompt(&id).unwrap();
        assert_eq!(context.len(),1);
        assert!(context[0].content.contains("native compact summary"));
        assert_eq!(store.get_token_usage(&id).unwrap().total_output_tokens,7);
        let preview = store.get_compacted_context_output(&id,&context[0].id).unwrap().unwrap();
        assert_eq!(preview.compaction_kind, "claudeNative");
        assert_eq!(preview.messages[0].content, "native compact summary");
        // Resume can rewrite native JSONL to the summary and newer records.
        // Local archived rows and their already recorded usage must survive.
        let mut next=snapshot.messages[1].clone();
        next.key="after-summary".into();
        next.uuids=vec![Uuid::new_v4().to_string()];
        next.usage=Some(json!({"output_tokens":2}));
        snapshot.messages=vec![snapshot.messages[2].clone(),next];
        snapshot.digest="b".repeat(64);
        assert!(store.reconcile_claude_native(&id,&remote,&snapshot,&store.get_messages(&id).unwrap(),None).unwrap());
        assert_eq!(store.get_messages(&id).unwrap().len(),4);
        assert_eq!(store.get_messages_for_prompt(&id).unwrap().len(),2);
        assert_eq!(store.get_token_usage(&id).unwrap().total_output_tokens,9);
        assert!(!store.reconcile_claude_native(&id,&remote,&snapshot,&store.get_messages(&id).unwrap(),None).unwrap());
    }
}

impl SessionStore {
    /// Opening or explicitly repairing an interrupted conversation must keep
    /// its locally saved, unsubmitted user turn. Require a durable run event
    /// tying that exact row to the interruption; never exempt arbitrary API
    /// messages merely because they lack Claude metadata.
    pub(crate) fn claude_recovery_pending_message(
        &self, id: &str, messages: &[ChatMessage], metadata: &HashMap<String, Value>,
    ) -> Result<Option<String>, String> {
        Ok(self.claude_recovery_pending_messages(id,messages,metadata,None)?.pop())
    }

    pub(crate) fn claude_recovery_pending_messages(
        &self, id: &str, messages: &[ChatMessage], metadata: &HashMap<String, Value>, current: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let notifications = self.claude_notification_ids(id, messages, metadata)?;
        let conn = self.conn.lock().map_err(|e|e.to_string())?;
        let confirmed: HashSet<_> = metadata.values().filter(|v|
            v.get("claude_checkpoint").is_some_and(Value::is_object) || v.get("claude_native").is_some_and(Value::is_object))
            .filter_map(|v|v["claude_pending_input"]["token"].as_str()).collect();
        // Starting a retry creates a new running run before preparing input.
        // The saved user row belongs to its earlier interrupted run, not the
        // latest run. Keep using that exact durable event as its provenance.
        let mut pending = Vec::new();
        for message in messages.iter().rev() {
            if notifications.contains(&message.id) { continue; }
            if message.role != MessageRole::User { break; }
            let value = metadata.get(&message.id);
            let awaiting_attachments = value.and_then(|v|v["claude_pending_input"]["carrier"].as_str())
                .is_some_and(|carrier|!confirmed.contains(carrier));
            if !awaiting_attachments && value.is_some_and(|v| v.get("claude_checkpoint").is_some_and(Value::is_object)
                || v.get("claude_native").is_some_and(Value::is_object)) { break; }
            // Never reinterpret a saved API turn as an unsent Claude input.
            if value.and_then(|v|v["backend"].as_str()).is_some_and(|backend|backend!="claude_code_cli") { break; }
            let recorded: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM session_events e JOIN session_runs r ON r.run_id=e.run_id AND r.session_id=e.session_id WHERE e.session_id=?1 AND r.status IN ('error','cancelled') AND e.event_type='userMessage' AND json_extract(e.payload_json,'$.message.id')=?2)",
                params![id,message.id],|r|r.get(0)).map_err(|e|e.to_string())?;
            if current == Some(message.id.as_str()) || recorded || value.is_some_and(|v|
                v["backend"]=="claude_code_cli" && v["claude_pending_input"]["version"]==1 && v["claude_pending_input"]["token"].is_string()) {
                pending.push(message.id.clone());
            } else { break; }
        }
        pending.reverse();
        Ok(pending)
    }

    /// Versioned, repeatable migration from CLI checkpoint metadata to native
    /// record identities. No new SQL columns; historical missing fields remain
    /// explicit `empty` in exports until a verified native snapshot is applied.
    pub(crate) fn reconcile_claude_native(
        &self, id: &str, remote: &SshConfig, snapshot: &NativeSnapshot,
        expected: &[ChatMessage], pending: Option<&str>,
    ) -> Result<bool, String> {
        self.reconcile_claude_native_binding(id,remote,snapshot,expected,pending,false,false,false,false)
    }
    /// Passive view synchronization must yield to a newly started run. Check
    /// again inside the write transaction, atomically with history replacement.
    pub(crate) fn reconcile_claude_native_if_idle(
        &self, id: &str, remote: &SshConfig, snapshot: &NativeSnapshot,
        expected: &[ChatMessage],
    ) -> Result<bool, String> {
        if self.active_run_for_session(id)?.is_some() { return Ok(false); }
        self.reconcile_claude_native_binding(id,remote,snapshot,expected,None,false,false,false,true)
    }
    /// Remote-authoritative recovery deliberately drops unsubmitted local
    /// history. Only the current, explicitly submitted user input survives.
    pub(crate) fn rebuild_claude_native(
        &self,id:&str,remote:&SshConfig,snapshot:&NativeSnapshot,expected:&[ChatMessage],pending:Option<&str>,
    )->Result<bool,String> {
        if let Some((bound,native))=self.claude_native_binding(id)? {
            if &bound!=remote { return Err("Remote Claude server does not match the conversation binding".into()); }
            snapshot.validate(native.as_deref())?;
        }
        self.reconcile_claude_native_binding(id,remote,snapshot,expected,pending,true,true,false,false)
    }
    /// A transport retry is not an explicit history repair: keep all proven
    /// unsent inputs (including mid-turn steering and continuation receipts).
    pub(crate) fn rebuild_claude_native_for_retry(
        &self, id: &str, remote: &SshConfig, snapshot: &NativeSnapshot,
        expected: &[ChatMessage], pending: Option<&str>,
    ) -> Result<bool, String> {
        if let Some((bound, native)) = self.claude_native_binding(id)? {
            if &bound != remote { return Err("Remote Claude server does not match the conversation binding".into()); }
            snapshot.validate(native.as_deref())?;
        }
        self.reconcile_claude_native_binding(id, remote, snapshot, expected, pending, true, true, true, false)
    }
    pub(crate) fn restore_claude_native_binding(
        &self,id:&str,remote:&SshConfig,snapshot:&NativeSnapshot,expected:&[ChatMessage],
    )->Result<bool,String> {
        self.reconcile_claude_native_binding(id,remote,snapshot,expected,None,true,false,false,false)
    }
    fn reconcile_claude_native_binding(
        &self,id:&str,remote:&SshConfig,snapshot:&NativeSnapshot,expected:&[ChatMessage],pending:Option<&str>,bind:bool,authoritative:bool,preserve_pending:bool,idle_only:bool,
    )->Result<bool,String> {
        snapshot.validate(None)?;
        self.migrate_claude_queued_inputs(id,remote,snapshot,expected,&self.get_response_request_metadata(id)?)?;
        let metadata = self.get_response_request_metadata(id)?;
        let pending = if authoritative && !preserve_pending {
            expected.iter().filter(|m|m.role==MessageRole::User && Some(m.id.as_str())==pending).map(|m|m.id.clone()).collect()
        } else { self.claude_recovery_pending_messages(id,expected,&metadata,pending)? };
        if !authoritative { self.verify_claude_history_origin(id, expected, &metadata, &pending)?; }
        let notifications = if authoritative && !preserve_pending { HashSet::new() } else { self.claude_notification_ids(id, expected, &metadata)? };
        let notification_updates = Self::claude_notification_migration(expected, &metadata, &notifications, snapshot);
        let hidden: HashSet<_> = snapshot.hidden_user_uuids.iter().map(String::as_str).collect();
        // Older builds imported command echoes as user messages. Remove only
        // rows whose original native UUIDs are now proven hidden, even if a
        // later compaction archived them. Never classify local text heuristically.
        let obsolete: HashSet<_> = expected.iter().filter(|m| m.role == MessageRole::User
            && metadata.get(&m.id).and_then(|v| v["claude_native"]["uuids"].as_array()).is_some_and(|ids|
                !ids.is_empty() && ids.iter().all(|id| id.as_str().is_some_and(|id| hidden.contains(id)))))
            .map(|m| m.id.clone()).collect();
        let mut archived: HashSet<String> = if !authoritative && snapshot.messages.iter().any(|m|m.compact_summary) {
            let conn=self.conn.lock().map_err(|e|e.to_string())?;
            let mut stmt=conn.prepare("SELECT id FROM messages WHERE session_id=?1 AND include_in_prompt=0").map_err(|e|e.to_string())?;
            let rows=stmt.query_map([id],|r|r.get::<_,String>(0)).map_err(|e|e.to_string())?;
            rows.collect::<Result<_,_>>().map_err(|e|e.to_string())?
        } else { HashSet::new() };
        archived.retain(|id| !obsolete.contains(id));
        let mut selected = HashSet::new();
        let mut resolved = Vec::new();
        for native in &snapshot.messages {
            if authoritative {
                let matches:Vec<_>=expected.iter().filter(|m|!selected.contains(&m.id)
                    && matches_native(m,metadata.get(&m.id),native,true)).collect();
                let existing = if matches.len()==1 {Some(matches[0])} else {None};
                if let Some(message)=existing {selected.insert(message.id.clone());}
                resolved.push((native.clone(),existing));
                continue;
            }
            let candidates = |exact| expected.iter().filter(|m| !obsolete.contains(&m.id) && !notifications.contains(&m.id) && (!pending.contains(&m.id) || pending_input_matches(metadata.get(&m.id), native)) && !selected.contains(&m.id)
                && (exact || !archived.contains(&m.id))
                && matches_native(m, metadata.get(&m.id), native, exact)).collect::<Vec<_>>();
            let exact = candidates(true);
            let candidates = if exact.is_empty() { candidates(false) } else {
                let mut matches = exact;
                // A legacy tool round without checkpoint metadata can coexist
                // with already checkpointed parts of the same API response.
                if native.role == MessageRole::Assistant {
                    let parsed = recovery::assistant_content(&native.content);
                    for message in expected.iter().filter(|m| m.role == MessageRole::Assistant
                        && !pending.contains(&m.id) && !selected.contains(&m.id) && !archived.contains(&m.id)) {
                        if !matches.iter().any(|m| m.id == message.id) && message.tool_calls.iter().flatten()
                            .any(|tool| parsed.tool_calls.iter().any(|t| t.id == tool.id)) {
                            matches.push(message);
                        }
                    }
                }
                matches
            };
            if candidates.len() > 1 {
                let fragments = reconcile_assistant_parts(native, &candidates, &metadata, &snapshot.native_id)?;
                selected.extend(candidates.iter().map(|message| message.id.clone()));
                resolved.extend(fragments);
            } else {
                let existing = candidates.first().copied();
                if let Some(message) = existing { selected.insert(message.id.clone()); }
                resolved.push((native.clone(), existing));
            }
        }
        if !bind && notification_updates.is_empty() && resolved.iter().all(|(native, existing)| existing.is_some_and(|m| metadata.get(&m.id).is_some_and(|v|
            v["claude_native"]["key"] == native.key && v["claude_native"]["digest"] == snapshot.digest
                && v["claude_native"]["cacheAlgorithmVersion"] == 2)))
            && expected.iter().all(|m| notifications.contains(&m.id) || pending.contains(&m.id) || archived.contains(&m.id) || selected.contains(&m.id)) {
            return Ok(false);
        }
        let mut prepared = Vec::new();
        let resolved_len = resolved.len();
        for (position, (native, existing)) in resolved.into_iter().enumerate() {
            let message_id = existing.map(|m| m.id.clone()).unwrap_or_else(|| Uuid::new_v5(&Uuid::NAMESPACE_URL,
                format!("{id}:{}:{}", snapshot.native_id, native.key).as_bytes()).to_string());
            selected.insert(message_id.clone());
            let mut payload = if authoritative && !preserve_pending {json!({})} else {metadata.get(&message_id).cloned().unwrap_or_else(|| json!({}))};
            if authoritative && pending_input_matches(metadata.get(&message_id),&native) {
                payload["claude_pending_input"]=metadata[&message_id]["claude_pending_input"].clone();
            }
            if payload.get("claude_native").is_some_and(|v| v["version"] != 1) {
                return Err("Unsupported saved Claude recovery metadata version".into());
            }
            payload["backend"] = json!("claude_code_cli");
            payload["claude_connection"] = json!(remote);
            payload["claude_checkpoint"] = json!({"version":1,"uuid":native.uuids.last(),"sessionId":snapshot.native_id});
            payload["claude_native"] = json!({"version":1,"cacheAlgorithmVersion":2,"key":native.key,"uuids":native.uuids,"digest":snapshot.digest,
                "content":native.content,"usage":native.usage,"compactSummary":native.compact_summary,"queuedInput":native.queued_input,"timestamp":native.timestamp});
            if let Some(model) = &native.model {
                payload["upstream_model"] = json!(model);
            }
            if position + 1 == resolved_len && snapshot.task_snapshot.is_object() {
                payload["claude_todos"] = snapshot.task_snapshot.clone();
            }
            prepared.push((native, message_id, existing, payload));
        }

        // Keep a local, versioned backup before an authoritative replacement.
        // It is not used as model input or silently replayed into Claude.
        let backup_root = self.db_path.parent().ok_or("Recovery requires a writable session database")?
            .join("claude-recovery").join(Uuid::new_v5(&Uuid::NAMESPACE_URL, id.as_bytes()).to_string());
        std::fs::create_dir_all(&backup_root).map_err(|e| e.to_string())?;
        let backup = backup_root.join(format!("{}.json", Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&backup).map_err(|e|e.to_string())?;
        file.write_all(&serde_json::to_vec(&json!({"version":1,"nativeId":snapshot.native_id,
            "messages":expected,"metadata":metadata,"todos":self.get_todos(id)?,"usage":self.get_token_usage(id)?})).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        file.sync_all().map_err(|e|e.to_string())?;

        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
        if idle_only {
            let active: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_runs WHERE session_id=?1 AND status IN ('queued','starting','running','waiting_input','finishing','cancelling'))",
                [id], |row| row.get(0),
            ).map_err(|e| e.to_string())?;
            if active { return Ok(false); }
        }
        // Compare the same full-history projection captured by get_messages.
        // Compaction deliberately backdates its prompt summary, but appends
        // its transcript marker by rowid. Timestamp order is not a revision.
        let current = self.get_messages_with_conn_filtered(&tx,id,false)?;
        if current.iter().map(|m| &m.id).ne(expected.iter().map(|m| &m.id)) {
            return Err("Local conversation changed during native synchronization; retry".into());
        }
        if serde_json::to_value(&current).map_err(|e|e.to_string())?
            != serde_json::to_value(expected).map_err(|e|e.to_string())? {
            return Err("Local conversation content changed during native synchronization; retry".into());
        }
        let base: i64 = tx.query_row("SELECT COALESCE(MAX(rowid),0)+1 FROM messages", [], |r|r.get(0)).map_err(|e|e.to_string())?;
        let mut timestamp = 0i64;
        let mut totals = [0u64; 4];
        let mut timed = 0u64;
        let mut duration = 0u64;
        let mut context = 0u64;
        tx.execute(if authoritative {"DELETE FROM model_usage_events WHERE session_id=?1"} else {"DELETE FROM model_usage_events WHERE session_id=?1 AND provider='Claude Code CLI'"}, [id]).map_err(|e|e.to_string())?;
        // Claude may prune pre-compaction rows on resume. Keep verified local
        // archives for viewing/export only; they are never sent back to CLI.
        for message in expected.iter().filter(|m|archived.contains(&m.id) && !selected.contains(&m.id) && m.role==MessageRole::Assistant) {
            let payload=&metadata[&message.id];
            // Reconciled fragment usage is authoritative. The streaming
            // claude_usage field can repeat the full response on every part.
            let usage = if let Some(native) = payload.get("claude_native") {
                let Some(usage) = native.get("usage").filter(|v| v.is_object()) else { continue; };
                Some(usage)
            } else { payload.get("claude_usage").filter(|v| v.is_object()) };
            let Some(usage)=usage else { return Err("Pre-compaction usage is unavailable in this older conversation; cached history was retained without replacement".into()); };
            let counts=["input_tokens","output_tokens","cache_read_input_tokens","cache_creation_input_tokens"].map(|k|usage[k].as_u64().unwrap_or(0).min(u32::MAX as u64));
            for i in 0..4 { totals[i]=totals[i].saturating_add(counts[i]); }
            let model=payload["upstream_model"].as_str().or_else(||payload["model"].as_str()).unwrap_or("unknown");
            tx.execute("INSERT INTO model_usage_events(session_id,model_id,provider,request_kind,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,created_at) VALUES(?1,?2,'Claude Code CLI','completion',?3,?4,?5,?6,0,?7)",params![id,model,counts[0] as i64,counts[1] as i64,counts[2] as i64,counts[3] as i64,message.created_at]).map_err(|e|e.to_string())?;
        }
        for (position, (native, message_id, existing, payload)) in prepared.iter().enumerate() {
            let remote_time = native.timestamp.as_deref().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|t|t.timestamp())
                .or_else(|| existing.map(|m|m.created_at)).unwrap_or_else(Self::now_ts);
            timestamp = timestamp.max(remote_time);
            let mut parsed = recovery::assistant_content(&native.content);
            for tool in &mut parsed.tool_calls {
                if let Some(result) = snapshot.messages.iter().find(|r| r.tool_call_id.as_deref() == Some(&tool.id)) {
                    tool.outcome = Some(if result.is_error { crate::commands::ToolCallOutcome::Error } else { crate::commands::ToolCallOutcome::Done });
                    tool.recorded_output = Some(recovery::text_content(&result.content));
                } else if snapshot.pending_tool_ids.contains(&tool.id) {
                    tool.outcome = Some(crate::commands::ToolCallOutcome::Error);
                    tool.recorded_output = Some(crate::llm::claude_code_cli::recovery::failed_tool_message().into());
                }
            }
            let content = if native.compact_summary {
                crate::compact::build_conversation_checkpoint_content(&recovery::text_content(&native.content), "")
            } else if native.role == MessageRole::Assistant { parsed.text.clone() }
                else if native.role == MessageRole::User && authoritative { authoritative_user_text(native,*existing) }
                else if native.role == MessageRole::User && !authoritative { existing.map(|m|m.content.clone()).unwrap_or_else(||authoritative_user_text(native,None)) }
                else { recovery::text_content(&native.content) };
            let tool_calls = (!parsed.tool_calls.is_empty()).then(||serde_json::to_string(&parsed.tool_calls).unwrap());
            let raw_images = recovery::images(&native.content);
            let images = existing.filter(|_|!authoritative).and_then(|m|m.images.clone()).or_else(|| (!raw_images.is_empty()).then_some(raw_images));
            let mut display: MessageMetadata = tx.query_row("SELECT metadata_json FROM messages WHERE id=?1 AND session_id=?2", params![message_id,id], |r| r.get::<_,Option<String>>(0))
                .optional().map_err(|e|e.to_string())?.flatten().and_then(|s|serde_json::from_str(&s).ok()).unwrap_or_default();
            if authoritative { display=MessageMetadata::default(); }
            display.cli_session_ids.get_or_insert_with(HashMap::new).insert("claude_code".into(), snapshot.native_id.clone());
            // Existing render parts may contain a truncated streaming answer.
            display.render_parts = None;
            display.content_order = None;
            display.thinking_order = None;
            if native.compact_summary {
                display.conversation_checkpoint = crate::compact::parse_conversation_checkpoint(&content);
            }
            let payload_id = Self::persist_response_request_with_conn(&tx, Some(payload))?;
            let row_id = base + position as i64;
            if existing.is_none() {
                tx.execute("INSERT INTO messages(rowid,id,session_id,role,content,created_at) VALUES(?1,?2,?3,?4,'',?5) ON CONFLICT(id) DO NOTHING",params![row_id,message_id,id,native.role.as_str(),timestamp]).map_err(|e|e.to_string())?;
            }
            tx.execute("UPDATE messages SET rowid=?1,role=?2,content=?3,created_at=?4,tool_calls=?5,tool_call_id=?6,images=?7,thinking_content=?8,thinking_signature=?9,metadata_json=?10,response_request_id=?11 WHERE id=?12 AND session_id=?13",
                params![row_id,native.role.as_str(),content,timestamp,tool_calls,native.tool_call_id,
                    images.as_ref().map(serde_json::to_string).transpose().map_err(|e|e.to_string())?,
                    (!parsed.thinking_text.is_empty()).then_some(parsed.thinking_text),(!parsed.thinking_signature.is_empty()).then_some(parsed.thinking_signature),
                    serialize_message_metadata(&display)?,payload_id,message_id,id]).map_err(|e|e.to_string())?;
            if authoritative {
                tx.execute("UPDATE messages SET include_in_prompt=1,prompt_prefix=NULL,prompt_suffix=NULL,asset_refs=NULL WHERE id=?1 AND session_id=?2",params![message_id,id]).map_err(|e|e.to_string())?;
            }
            if let Some(usage) = &native.usage {
                let counts = ["input_tokens","output_tokens","cache_read_input_tokens","cache_creation_input_tokens"].map(|k|usage[k].as_u64().unwrap_or(0).min(u32::MAX as u64));
                for i in 0..4 { totals[i] = totals[i].saturating_add(counts[i]); }
                context = counts[0] + counts[2] + counts[3];
                let timing = &payload["claude_timing"];
                if timing["version"] == 1 && timing["source"] == "claude-turn-step" {
                    if let (Some(start),Some(stop)) = (timing["requestStartedAtMs"].as_u64(),timing["responseStoppedAtMs"].as_u64()) {
                        if stop > start && timing["resultReadyAtMs"].as_u64().is_some_and(|ready| ready >= stop) {
                            duration = duration.saturating_add(stop-start); timed = timed.saturating_add(counts[1]);
                        }
                    }
                }
                let model = format!("claude_code/{}", native.model.as_deref().unwrap_or("unknown"));
                tx.execute("INSERT INTO model_usage_events(session_id,model_id,provider,request_kind,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,created_at) VALUES(?1,?2,'Claude Code CLI','completion',?3,?4,?5,?6,0,?7)",params![id,model,counts[0] as i64,counts[1] as i64,counts[2] as i64,counts[3] as i64,timestamp]).map_err(|e|e.to_string())?;
            }
        }
        // Rebuild diagnostics in this same transaction, from API responses (not
        // local assistant fragments). Reconciliation must not add billing or
        // notifications a second time, nor compare replay with future live rows.
        tx.execute("DELETE FROM session_prompt_cache_checks WHERE session_id=?1 AND
            (provider='Claude Code CLI' OR model_id LIKE 'claude_code/%')", [id]).map_err(|e|e.to_string())?;
        Self::reset_prompt_cache_baseline(&tx, id)?;
        let mut cache_user = String::new();
        for message in expected.iter().filter(|m| archived.contains(&m.id) && !selected.contains(&m.id)) {
            if is_context_handoff_message(message) {
                Self::reset_prompt_cache_baseline(&tx, id)?;
                continue;
            }
            if message.role == MessageRole::User { cache_user = message.id.clone(); }
            if message.role != MessageRole::Assistant { continue; }
            let payload = &metadata[&message.id];
            let native = payload.get("claude_native");
            let usage = if let Some(native) = native {
                let Some(usage) = native.get("usage").filter(|v| v.is_object()) else { continue; };
                usage
            } else { &payload["claude_usage"] };
            let key = native.and_then(|v| v["key"].as_str()).map(|key| format!("native:{}:{key}", snapshot.native_id))
                .or_else(|| payload["claude_cache_request"]["key"].as_str().map(str::to_owned));
            // Old streaming parts can repeat the same response's usage. Without
            // a response identity, retain an unknown interval, not invented misses.
            let receipt = if key.is_some() { usage } else { &Value::Null };
            Self::record_claude_cache_check(&tx, id, key.unwrap_or_else(|| format!("archive:{}", message.id)),
                cache_user.clone(), payload["upstream_model"].as_str().or_else(|| payload["model"].as_str()).unwrap_or("unknown"),
                receipt, message.created_at)?;
        }
        for native in &snapshot.messages {
            let local_id = prepared.iter().rev().find(|(part, ..)| part.key == native.key
                || part.key.strip_prefix(&native.key).is_some_and(|suffix| suffix.starts_with(":part:")))
                .map(|(_, message_id, ..)| message_id.as_str()).unwrap_or("");
            if native.compact_summary {
                Self::reset_prompt_cache_baseline(&tx, id)?;
            }
            if native.role == MessageRole::User { cache_user = local_id.to_owned(); }
            if native.role == MessageRole::Assistant {
                let occurred_at = native.timestamp.as_deref().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp()).unwrap_or(timestamp);
                Self::record_claude_cache_check(&tx, id, format!("native:{}:{}", snapshot.native_id, native.key),
                    cache_user.clone(), native.model.as_deref().unwrap_or("unknown"),
                    native.usage.as_ref().unwrap_or(&Value::Null), occurred_at)?;
            }
        }
        let mut pending_position = prepared.len() as i64;
        for (message, payload) in notification_updates {
            let payload_id = Self::persist_response_request_with_conn(&tx, Some(&payload))?;
            tx.execute("UPDATE messages SET response_request_id=?1 WHERE session_id=?2 AND id=?3",params![payload_id,id,message]).map_err(|e|e.to_string())?;
        }
        for message in expected {
            if notifications.contains(&message.id) { continue; }
            if pending.contains(&message.id) && !selected.contains(&message.id) {
                timestamp = timestamp.max(message.created_at);
                tx.execute("UPDATE messages SET rowid=?1,created_at=?2 WHERE id=?3 AND session_id=?4",params![base+pending_position,timestamp,message.id,id]).map_err(|e|e.to_string())?;
                pending_position += 1;
            } else if !selected.contains(&message.id) && !archived.contains(&message.id) {
                tx.execute("DELETE FROM messages WHERE id=?1 AND session_id=?2",params![message.id,id]).map_err(|e|e.to_string())?;
            }
        }
        // Preserve pre-compaction transcript for viewing, while matching the
        // native summary boundary in Locus's context viewer and exports.
        if let Some((boundary, _)) = prepared.iter().enumerate().rev().find(|(_, (m, ..))| m.compact_summary) {
            tx.execute("UPDATE messages SET include_in_prompt=(rowid>=?1) WHERE session_id=?2",params![base+boundary as i64,id]).map_err(|e|e.to_string())?;
            for (position, (native, message_id, ..)) in prepared.iter().enumerate().filter(|(_, (m, ..))| m.compact_summary) {
                let _ = native;
                let entries = prepared.iter().skip(position).take_while(|(m,..)| !m.compact_summary || m.key == prepared[position].0.key)
                    .map(|(_,id,..)| CompactedContextSnapshotEntry { message_id:id.clone(),prompt_prefix_placeholder:false,prompt_suffix_placeholder:false }).collect();
                let raw:String=tx.query_row("SELECT metadata_json FROM messages WHERE id=?1",[message_id],|r|r.get(0)).map_err(|e|e.to_string())?;
                let mut display:MessageMetadata=serde_json::from_str(&raw).map_err(|e|e.to_string())?;
                display.compacted_context=Some(CompactedContextSnapshot{version:1,entries:Some(entries)});
                tx.execute("UPDATE messages SET metadata_json=?1 WHERE id=?2",params![serialize_message_metadata(&display)?,message_id]).map_err(|e|e.to_string())?;
            }
        }
        tx.execute("INSERT INTO token_usage(session_id,total_input_tokens,total_output_tokens,total_cache_read_tokens,total_cache_write_tokens,timed_output_tokens,model_active_duration_ms,last_context_tokens) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(session_id) DO UPDATE SET total_input_tokens=excluded.total_input_tokens,total_output_tokens=excluded.total_output_tokens,total_cache_read_tokens=excluded.total_cache_read_tokens,total_cache_write_tokens=excluded.total_cache_write_tokens,timed_output_tokens=excluded.timed_output_tokens,model_active_duration_ms=excluded.model_active_duration_ms,last_context_tokens=excluded.last_context_tokens",
            params![id,totals[0].min(i64::MAX as u64) as i64,totals[1].min(i64::MAX as u64) as i64,totals[2].min(i64::MAX as u64) as i64,totals[3].min(i64::MAX as u64) as i64,timed.min(i64::MAX as u64) as i64,duration.min(i64::MAX as u64) as i64,context.min(i64::MAX as u64) as i64]).map_err(|e|e.to_string())?;
        tx.execute("DELETE FROM todos WHERE session_id=?1", [id]).map_err(|e|e.to_string())?;
        for (position, todo) in snapshot.todos.iter().enumerate() {
            tx.execute("INSERT INTO todos(session_id,position,content,status,priority) VALUES(?1,?2,?3,?4,?5)",params![id,position as i64,todo.content,todo.status,todo.priority]).map_err(|e|e.to_string())?;
        }
        tx.execute("UPDATE sessions SET updated_at=?1 WHERE id=?2",params![Self::now_ts(),id]).map_err(|e|e.to_string())?;
        if bind {
            tx.execute("INSERT INTO claude_native_bindings(session_id,connection_json,native_id) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET connection_json=excluded.connection_json,native_id=excluded.native_id",
                params![id,serde_json::to_string(remote).map_err(|e|e.to_string())?,snapshot.native_id]).map_err(|e|e.to_string())?;
        }
        tx.commit().map_err(|e|e.to_string())?;
        Ok(true)
    }
}
