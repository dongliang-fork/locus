use super::*;
fn row(n:u128,role:&str,parent:Option<u128>,content:Value)->Value {
    json!({"uuid":uuid::Uuid::from_u128(n),"type":role,"parentUuid":parent.map(uuid::Uuid::from_u128),"message":{"content":content}})
}
fn fixture(rows:&[Value])->(ArchiveInfo,BTreeMap<String,Vec<u8>>) {
    let raw=rows.iter().map(|r|format!("{r}\n")).collect::<String>().into_bytes();
    (ArchiveInfo{version:1,native_id:uuid::Uuid::from_u128(100).to_string(),source_root:"/test".into(),updated_at:0,transferred_bytes:raw.len() as u64,incomplete_tail:false,
        manifest:BTreeMap::from([("transcript.jsonl".into(),Cursor{size:raw.len() as u64,sha256:digest(&raw)})])},
     BTreeMap::from([("transcript.jsonl".into(),raw)]))
}
#[test]
fn claude_archive_preserves_native_completion_reason_without_inventing_one() {
    let mut answer = row(2, "assistant", Some(1), json!("done"));
    answer["message"]["stop_reason"] = json!("end_turn");
    let (info, files) = fixture(&[row(1, "user", None, json!("request")), answer.clone()]);
    assert_eq!(project(&info, &files).unwrap().messages[1].stop_reason.as_deref(), Some("end_turn"));
    answer["message"]["stop_reason"] = Value::Null;
    let (info, files) = fixture(&[row(1, "user", None, json!("request")), answer]);
    assert!(project(&info, &files).unwrap().messages[1].stop_reason.is_none());
}

#[test]
fn claude_archive_keeps_consumed_queued_input_with_media_even_as_the_last_record() {
    let queued=json!({"uuid":uuid::Uuid::from_u128(3),"parentUuid":uuid::Uuid::from_u128(2),"type":"attachment","isMeta":true,
        "attachment":{"type":"queued_command","prompt":"read thoroughly","humanTurn":true,"origin":{"kind":"human"}},
        "rendered":[{"content":[{"type":"text","text":"system reminder"},{"type":"image","source":{"type":"base64","data":"aQ==","media_type":"image/png"}}]}]});
    let rows=vec![row(1,"user",None,json!("first")),row(2,"assistant",Some(1),json!("working")),queued];
    let (info,files)=fixture(&rows);let projected=project(&info,&files).unwrap();
    let input=projected.messages.last().unwrap();
    assert!(input.queued_input);
    assert_eq!(input.role,MessageRole::User);
    assert_eq!(input.key,uuid::Uuid::from_u128(3).to_string());
    assert_eq!(super::super::super::recovery::text_content(&input.content),"read thoroughly");
    assert_eq!(super::super::super::recovery::images(&input.content).len(),1);
}
#[test]
fn claude_archive_keeps_pending_user_receipts_but_omits_other_meta_notes() {
    let token=uuid::Uuid::new_v4().to_string();
    let mut pending=row(1,"user",None,json!([{"type":"text","text":super::super::super::recovery::pending_input_text(&token,"first pending message")} ]));
    pending["isMeta"]=json!(true);
    let mut meta=row(2,"user",Some(1),json!("ordinary native reminder")); meta["isMeta"]=json!(true);
    let (info,files)=fixture(&[pending,meta,row(3,"user",Some(2),json!("latest pending message"))]);
    let snapshot=project(&info,&files).unwrap();
    assert_eq!(snapshot.messages.len(),2);
    assert_eq!(snapshot.messages[0].role,MessageRole::User);
    assert_eq!(snapshot.messages[0].uuids,vec![uuid::Uuid::from_u128(1).to_string()]);
}
#[test]
fn claude_archive_projects_native_parallel_tools_and_images_without_duplicate_usage() {
    let mut a=row(3,"assistant",Some(1),json!([{"type":"thinking","thinking":"reason"}]));
    a["message"]["id"]=json!("api-1");a["message"]["usage"]=json!({"output_tokens":7});
    let mut b=row(4,"assistant",Some(3),json!([{"type":"tool_use","id":"t1","name":"Read","input":{}},{"type":"tool_use","id":"t2","name":"Glob","input":{}}]));
    b["message"]["id"]=json!("api-1");b["message"]["usage"]=json!({"output_tokens":7});
    let rows=vec![row(1,"user",None,json!([{"type":"text","text":"question"},{"type":"image","source":{"type":"base64","data":"aQ==","media_type":"image/png"}}])),
        row(2,"assistant",Some(1),json!("abandoned")),a,b,
        row(5,"user",Some(4),json!([{"type":"tool_result","tool_use_id":"t1","content":"one"}])),
        row(6,"user",Some(4),json!([{"type":"tool_result","tool_use_id":"t2","content":"two"}])),
        row(7,"assistant",Some(6),json!("answer"))];
    let (info,mut files)=fixture(&rows);
    files.insert("tasks/1.json".into(),br#"{"subject":"todo","status":"pending"}"#.to_vec());
    let projected=project(&info,&files).unwrap();
    assert_eq!(projected.messages.len(),5);
    assert_eq!(projected.messages[1].uuids.len(),2);
    assert_eq!(projected.messages[1].parts.iter().map(|p|p.uuid.clone()).collect::<Vec<_>>(),projected.messages[1].uuids);
    assert_eq!(projected.messages[1].parts[0].content[0]["type"],"thinking");
    assert_eq!(projected.messages[1].parts[1].content.len(),2);
    assert_eq!(projected.messages.iter().filter_map(|m|m.usage.as_ref()).count(),1);
    assert_eq!(projected.messages[1].usage.as_ref().unwrap()["output_tokens"],7);
    assert_eq!(super::super::super::recovery::images(&projected.messages[0].content).len(),1);
    assert_eq!(projected.todos[0].content,"todo");
    assert!(projected.can_continue().is_ok());
    assert!(!serde_json::to_string(&projected).unwrap().contains("abandoned"));
}

#[test]
fn claude_archive_validates_source_part_boundaries_before_recovery() {
    let (info,files)=fixture(&[row(1,"user",None,json!("question")),row(2,"assistant",Some(1),json!("answer"))]);
    let original=project(&info,&files).unwrap();
    for corruption in ["uuid","content","role"] {
        let mut snapshot=original.clone();
        match corruption {
            "uuid"=>snapshot.messages[1].parts[0].uuid=uuid::Uuid::new_v4().to_string(),
            "content"=>snapshot.messages[1].parts[0].content.clear(),
            _=>snapshot.messages[1].role=MessageRole::User,
        }
        assert!(snapshot.validate(None).is_err(),"{corruption}");
    }
}
#[test]
fn claude_archive_blocks_torn_records_and_unconfirmed_dispatch_after_compaction() {
    let mut boundary=row(1,"system",None,json!([]));boundary["subtype"]=json!("compact_boundary");boundary["logicalParentUuid"]=json!(uuid::Uuid::from_u128(3));
    let mut summary=row(2,"user",Some(1),json!("summary"));summary["isCompactSummary"]=json!(true);
    let (mut info,mut files)=fixture(&[boundary,summary,row(3,"assistant",Some(2),json!("continue"))]);
    files.insert("dispatch/a.json".into(),br#"{"version":2,"callId":"old","settled":true}"#.to_vec());
    assert!(project(&info,&files).unwrap().can_continue().is_ok());
    files.insert("dispatch/b.json".into(),br#"{"version":1,"callId":"missing"}"#.to_vec());
    assert_eq!(project(&info,&files).unwrap().pending_tool_ids,vec!["missing"]);
    info.incomplete_tail=true;
    assert!(project(&info,&files).unwrap().can_continue().is_err());
}
#[test]
fn claude_archive_rejects_duplicate_and_missing_parent_records() {
    let user=row(1,"user",None,json!("one"));
    let (info,files)=fixture(&[user.clone(),user]);assert!(project(&info,&files).unwrap_err().contains("Duplicate"));
    let (info,files)=fixture(&[row(1,"user",Some(2),json!("one"))]);assert!(project(&info,&files).unwrap_err().contains("missing parent"));
}

#[test]
fn claude_archive_recovers_confirmed_mcp_todos_and_respects_native_tasks() {
    let todos=json!([{"content":"keep the checklist","status":"completed","priority":"high"}]);
    let tool=|n,parent,id: &str,list: Value|row(n,"assistant",Some(parent),json!([{"type":"tool_use","id":id,"name":"mcp__locus__todowrite","input":{"todos":list}}]));
    let mut failed=row(5,"user",Some(4),json!([{"type":"tool_result","tool_use_id":"failed","content":"refused","is_error":true}]));
    failed["timestamp"]=json!("2026-10-16T01:00:00Z");
    let rows=[row(1,"user",None,json!("question")),tool(2,1,"saved",todos.clone()),
        row(3,"user",Some(2),json!([{"type":"tool_result","tool_use_id":"saved","content":"done"}])),
        tool(4,3,"failed",json!([])),failed];
    let (info,mut files)=fixture(&rows);
    let snapshot=project(&info,&files).unwrap();
    assert_eq!(json!(snapshot.todos),todos);
    assert_eq!(snapshot.task_snapshot["kind"],"todos");
    files.insert("tasks/1.json".into(),br#"{"subject":"native task","status":"pending"}"#.to_vec());
    let snapshot=project(&info,&files).unwrap();
    assert_eq!(snapshot.task_snapshot["kind"],"tasks");
    assert_eq!(snapshot.todos[0].content,"native task");
}

#[test]
fn claude_archive_excludes_cli_diagnostics_without_filtering_actual_model_text() {
    let mut command=row(1,"system",None,json!("/model"));command["subtype"]=json!("local_command");
    let user=row(2,"user",Some(1),json!("question"));
    let mut error=row(3,"assistant",Some(2),json!("Run /model to pick a different model."));
    error["isApiErrorMessage"]=json!(true);
    let mut synthetic=row(4,"assistant",Some(3),json!("CLI interrupted"));
    synthetic["message"]["model"]=json!("<synthetic>");
    let mut notice=row(5,"user",Some(4),json!("internal reminder"));notice["isMeta"]=json!(true);
    let answer=row(6,"assistant",Some(5),json!("Run /model to pick a different model."));
    let (info,files)=fixture(&[command,user,error,synthetic,notice,answer]);
    let before=files["transcript.jsonl"].clone();
    let projected=project(&info,&files).unwrap();
    assert_eq!(projected.messages.len(),2);
    assert_eq!(projected.messages[1].uuids,vec![uuid::Uuid::from_u128(6).to_string()]);
    assert_eq!(projected.messages[1].content[0]["text"],"Run /model to pick a different model.");
    assert_eq!(files["transcript.jsonl"],before);
}

#[test]
fn claude_archive_filters_native_user_artifacts_without_breaking_ancestry() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../../../../tests/fixtures/claude_native_user_visibility.json")).unwrap();
    for case in cases {
        let mut candidate=case["row"].clone();
        let candidate_id=uuid::Uuid::from_u128(2).to_string();
        candidate["uuid"]=json!(candidate_id);
        candidate["parentUuid"]=json!(uuid::Uuid::from_u128(1));
        let (info,files)=fixture(&[row(1,"user",None,json!("before")),candidate,
            row(3,"user",Some(2),json!("after"))]);
        let before=files["transcript.jsonl"].clone();
        let snapshot=project(&info,&files).unwrap();
        let hidden=case["hidden"].as_bool().unwrap();
        assert_eq!(snapshot.messages.iter().any(|m|m.uuids.contains(&candidate_id)),!hidden,"{}",case["name"]);
        assert_eq!(snapshot.hidden_user_uuids.contains(&candidate_id),hidden,"{}",case["name"]);
        assert_eq!(snapshot.messages.first().unwrap().content[0]["text"],"before");
        assert_eq!(snapshot.messages.last().unwrap().content[0]["text"],"after");
        assert_eq!(files["transcript.jsonl"],before);
    }
}
