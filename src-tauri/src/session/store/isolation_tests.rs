use super::*;

fn usage(id: &str) -> ModelUsageRecord {
    ModelUsageRecord {
        session_id: id.into(),
        model_id: "claude_code/test".into(),
        provider: "Claude Code CLI".into(),
        request_kind: "completion".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 30,
        cache_write_tokens: 0,
        model_active_duration_ms: 1000,
        cost_usd: 0.0,
        priced_rounds: 0,
        context_tokens: Some(130),
        context_limit: Some(200000),
        cache_request: None,
    }
}

fn event(id: &str) -> SessionEventAppend {
    SessionEventAppend {
        session_id: id.into(),
        run_id: format!("{id}-run"),
        event_type: "textDelta".into(),
        payload_json: "{}".into(),
    }
}

#[test]
fn failed_event_commit_does_not_break_other_sessions() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    // Deliberately use rollback journaling to force the exact production
    // COMMIT/BUSY failure. WAL normally prevents this reader/writer conflict.
    {
        let conn = store.conn.lock().unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA busy_timeout=0")
            .unwrap();
    }
    let reader = Connection::open(&store.db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM sessions;")
        .unwrap();
    let error = store.append_session_events_batch(&[event(&a)]).unwrap_err();
    assert!(
        error.contains("commit session event transaction"),
        "{error}"
    );
    assert!(store.conn.lock().unwrap().is_autocommit());
    reader.execute_batch("ROLLBACK").unwrap();
    assert!(store.record_model_usage_best_effort(usage(&b)).is_some());
    store.append_session_events_batch(&[event(&b)]).unwrap();
    assert!(store
        .list_session_events(&a, None, None)
        .unwrap()
        .is_empty());
    assert_eq!(store.list_session_events(&b, None, None).unwrap().len(), 1);
    assert_eq!(store.get_token_usage(&b).unwrap().total_input_tokens, 100);
}

#[test]
fn abandoned_transaction_and_writer_panic_are_isolated() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let conn = store.conn.lock().unwrap();
        conn.execute_batch("SAVEPOINT legacy_operation").unwrap();
        conn.execute("UPDATE sessions SET title='uncommitted' WHERE id=?1", [&a])
            .unwrap();
        panic!("injected failure while holding the writer");
    }));
    assert!(panic.is_err());
    assert_eq!(store.get_session_title(&a).unwrap().as_deref(), Some("A"));
    assert!(store.record_model_usage_best_effort(usage(&b)).is_some());
    assert!(store.conn.lock().unwrap().is_autocommit());
}

#[test]
fn wal_readers_keep_a_snapshot_without_blocking_other_session_writes() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    let reader = store.read_conn().unwrap();
    let count = || {
        reader
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    assert_eq!(count(), 0);
    store
        .add_message(&b, MessageRole::Assistant, "B remains usable")
        .unwrap();
    assert_eq!(
        count(),
        0,
        "the active read operation keeps a stable snapshot"
    );
    assert_eq!(
        store.get_messages(&b).unwrap().len(),
        1,
        "a second reader sees the commit"
    );
    drop(reader);
    assert_eq!(
        store.get_messages(&b).unwrap().len(),
        1,
        "pooled readers release old snapshots"
    );
    let writer = store.conn.lock().unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    writer
        .execute("UPDATE sessions SET title='pending' WHERE id=?1", [&a])
        .unwrap();
    assert_eq!(store.get_session_title(&a).unwrap().as_deref(), Some("A"));
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        writer
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

#[test]
fn usage_statistics_failure_preserves_messages_and_other_session_usage() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    let input = store
        .add_message(&a, MessageRole::User, "question")
        .unwrap();
    store
        .set_message_response_request_metadata(
            &a,
            &input,
            &serde_json::json!({"claude_usage":{"input_tokens":100,"output_tokens":20}}),
        )
        .unwrap();
    store.conn.lock().unwrap().execute_batch("CREATE TRIGGER reject_usage BEFORE INSERT ON model_usage_events BEGIN SELECT RAISE(ABORT,'injected statistics failure'); END;").unwrap();
    assert!(store.record_model_usage_best_effort(usage(&a)).is_none());
    store
        .add_message(&a, MessageRole::Assistant, "final answer after usage")
        .unwrap();
    assert_eq!(
        store.get_token_usage(&a).unwrap().total_input_tokens,
        0,
        "failed projection must roll back its counter update"
    );
    store
        .conn
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_usage")
        .unwrap();
    assert!(store.record_model_usage_best_effort(usage(&b)).is_some());
    let snapshot = store.create_export_snapshot().unwrap();
    assert_eq!(
        snapshot.get_messages(&a).unwrap().last().unwrap().content,
        "final answer after usage"
    );
    assert_eq!(
        snapshot.get_response_request_metadata(&a).unwrap()[&input]["claude_usage"]["input_tokens"],
        100
    );
    assert_eq!(
        snapshot.get_token_usage(&b).unwrap().total_output_tokens,
        20
    );
    drop(snapshot);
    drop(store);
    let reopened = SessionStore::new(root.path()).unwrap();
    assert_eq!(reopened.get_messages(&a).unwrap().len(), 2);
    assert_eq!(
        reopened.get_token_usage(&b).unwrap().total_input_tokens,
        100
    );
}

#[test]
fn busy_usage_is_retried_once_without_double_counting_or_blocking_a_turn() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let competing = Connection::open(&store.db_path).unwrap();
    competing.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        competing.execute_batch("ROLLBACK").unwrap();
    });
    assert!(store.record_model_usage_best_effort(usage(&a)).is_some());
    release.join().unwrap();
    store
        .add_message(&b, MessageRole::Assistant, "unrelated session continues")
        .unwrap();
    assert_eq!(store.get_token_usage(&a).unwrap().total_input_tokens, 100);
    assert_eq!(store.get_token_usage(&a).unwrap().total_output_tokens, 20);
    assert_eq!(
        store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM model_usage_events WHERE session_id=?1",
                [&a],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(store.get_messages(&b).unwrap().len(), 1);
    assert_eq!(
        store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[test]
fn concurrent_sessions_survive_one_sessions_failed_statistics_and_events() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(SessionStore::new(root.path()).unwrap());
    let ids = (0..3)
        .map(|i| {
            store
                .create_session(&format!("session {i}"), None, None, "chat", None)
                .unwrap()
        })
        .collect::<Vec<_>>();
    store.conn.lock().unwrap().execute_batch(&format!(
        "CREATE TRIGGER reject_one_session_usage BEFORE INSERT ON model_usage_events WHEN NEW.session_id='{}' BEGIN SELECT RAISE(ABORT,'statistics rejected'); END;
         CREATE TRIGGER reject_one_session_events BEFORE INSERT ON session_events WHEN NEW.session_id='{}' BEGIN SELECT RAISE(ABORT,'event rejected'); END;", ids[0], ids[0])).unwrap();
    let reader = store.read_conn().unwrap();
    assert_eq!(
        reader
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let store = store.clone();
            let id = id.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..5 {
                    assert_eq!(
                        store.record_model_usage_best_effort(usage(&id)).is_some(),
                        i != 0
                    );
                    assert_eq!(
                        store.append_session_events_batch(&[event(&id)]).is_ok(),
                        i != 0
                    );
                    store
                        .add_message(&id, MessageRole::Assistant, "answer retained")
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    drop(reader);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(store.get_messages(id).unwrap().len(), 5);
        assert_eq!(
            store.get_token_usage(id).unwrap().total_input_tokens,
            if i == 0 { 0 } else { 500 }
        );
        assert_eq!(
            store.list_session_events(id, None, None).unwrap().len(),
            if i == 0 { 0 } else { 5 }
        );
    }
    assert!(store.conn.lock().unwrap().is_autocommit());
}

#[test]
fn exhausted_usage_retries_leave_the_writer_available() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let other = Connection::open(&store.db_path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(store.record_model_usage_best_effort(usage(&a)).is_none());
    assert!(store.conn.lock().unwrap().is_autocommit());
    other.execute_batch("ROLLBACK").unwrap();
    assert!(store.record_model_usage_best_effort(usage(&b)).is_some());
    assert_eq!(store.get_token_usage(&a).unwrap().total_input_tokens, 0);
    assert_eq!(store.get_token_usage(&b).unwrap().total_input_tokens, 100);
}

#[test]
fn an_event_failure_does_not_discard_another_sessions_same_batch() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::new(root.path()).unwrap();
    let a = store.create_session("A", None, None, "chat", None).unwrap();
    let b = store.create_session("B", None, None, "chat", None).unwrap();
    store.conn.lock().unwrap().execute_batch(&format!(
        "CREATE TRIGGER reject_a BEFORE INSERT ON session_events WHEN NEW.session_id='{a}' BEGIN SELECT RAISE(ABORT,'injected event failure'); END;"
    )).unwrap();
    let queued = |id: &str| QueuedSessionEvent {
        event: event(id),
        merge: None,
        status_updates: Vec::new(),
    };
    assert!(SessionEventWriter::persist_batch(&store.conn, &[queued(&a), queued(&b)]).is_err());
    assert!(store
        .list_session_events(&a, None, None)
        .unwrap()
        .is_empty());
    assert_eq!(store.list_session_events(&b, None, None).unwrap().len(), 1);
    assert!(store.conn.lock().unwrap().is_autocommit());
}
