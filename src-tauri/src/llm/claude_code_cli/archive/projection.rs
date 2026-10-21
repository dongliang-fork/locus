//! Project a verified local copy of native JSONL without running any CLI.
use super::*;
use crate::llm::claude_code_cli::recovery::{NativeAssistantPart,NativeMessage,NativeSnapshot};
use crate::session::models::{MessageRole,TodoItem};
use serde_json::{json,Value};
use std::collections::{HashMap,HashSet};
#[cfg(test)]
#[path="projection_tests.rs"]
mod tests;

fn ancestry(start:&str,nodes:&HashMap<String,(usize,Value)>,selected:&mut HashSet<String>)->Result<(),String> {
    let mut current=Some(start.to_string());let mut visited=HashSet::new();
    while let Some(id)=current {
        if !visited.insert(id.clone()) {return Err("Native transcript contains a parent cycle".into());}
        if selected.contains(&id) {break;}
        let (index,row)=nodes.get(&id).ok_or("Native transcript has a missing parent")?;
        selected.insert(id);
        current=row["parentUuid"].as_str().map(str::to_string);
        if current.is_none() && row["subtype"]=="compact_boundary" {
            current=row["logicalParentUuid"].as_str().filter(|parent|nodes.get(*parent).is_some_and(|(i,_)|i<index)).map(str::to_string);
        }
    }
    Ok(())
}
fn blocks(value:&Value)->Vec<Value> {
    if let Some(text)=value.as_str() {vec![json!({"type":"text","text":text})]}
    else {value.as_array().cloned().unwrap_or_default()}
}
fn queued_input(row: &Value) -> bool {
    row["type"]=="attachment" && row["attachment"]["type"]=="queued_command"
        && (row["attachment"]["humanTurn"]==true || row["attachment"]["origin"]["kind"]=="human")
        && row["attachment"]["prompt"].is_string()
}
fn queued_content(row: &Value) -> Vec<Value> {
    let mut content=vec![json!({"type":"text","text":row["attachment"]["prompt"]})];
    // Native rendering owns attachment media; do not substitute cache paths
    // or include the CLI's surrounding system-reminder in visible user text.
    for rendered in row["rendered"].as_array().into_iter().flatten() {
        content.extend(blocks(&rendered["content"]).into_iter().filter(|b|b["type"]=="image"));
    }
    content
}
pub fn project(info:&ArchiveInfo,files:&BTreeMap<String,Vec<u8>>)->Result<NativeSnapshot,String> {
    let raw=files.get("transcript.jsonl").ok_or("Native archive has no transcript")?;
    let mut nodes=HashMap::new();let mut leaf=None;
    for (index,line) in raw.split(|b|*b==b'\n').filter(|line|!line.is_empty()).enumerate() {
        let row:Value=serde_json::from_slice(line).map_err(|_|"Invalid native JSONL record")?;
        if matches!(row["type"].as_str(),Some("user"|"assistant")) && !row["message"]["content"].is_array() && !row["message"]["content"].is_string() {
            return Err("Unsupported native message content".into());
        }
        if row["isSidechain"]==true {continue;}
        let Some(id)=row["uuid"].as_str().map(str::to_string) else {continue;};
        if matches!(row["type"].as_str(),Some("user"|"assistant")) || queued_input(&row) {leaf=Some(id.clone());}
        if nodes.insert(id,(index,row)).is_some() {return Err("Duplicate native message UUID".into());}
    }
    let mut selected=HashSet::new();
    if let Some(leaf)=leaf {
        ancestry(&leaf,&nodes,&mut selected)?;
        let last=nodes[&leaf].0;
        loop {
            let before=selected.len();
            let ids:HashSet<_>=selected.iter().filter_map(|id|{
                let row=&nodes[id].1;
                (row["type"]=="assistant").then(||row["message"]["id"].as_str()).flatten().map(str::to_string)
            }).collect();
            let mut tools=HashSet::new();
            for (id,(index,row)) in &nodes {
                if *index<=last && row["type"]=="assistant" && row["message"]["id"].as_str().is_some_and(|s|ids.contains(s)) {
                    ancestry(id,&nodes,&mut selected)?;
                    for block in blocks(&row["message"]["content"]) {
                        if block["type"]=="tool_use" {if let Some(id)=block["id"].as_str(){tools.insert(id.to_string());}}
                    }
                }
            }
            for (id,(index,row)) in &nodes {
                if *index<=last && blocks(&row["message"]["content"]).iter().any(|b|b["tool_use_id"].as_str().is_some_and(|id|tools.contains(id))) {
                    ancestry(id,&nodes,&mut selected)?;
                }
            }
            if selected.len()==before {break;}
        }
    }
    let mut ordered:Vec<_>=nodes.iter().filter(|(id,_)|selected.contains(*id)).collect();
    ordered.sort_by_key(|(_, (index,_))|*index);
    let mut messages:Vec<NativeMessage>=Vec::new();let mut groups=HashMap::new();
    let mut hidden_user_uuids=Vec::new();
    for (id,(_,row)) in ordered {
        // Select ancestry first: hidden command rows can be parents of real
        // prompts, summaries, and tool results.
        if super::super::recovery::hidden_native_user(row) {
            hidden_user_uuids.push(id.clone());
            continue;
        }
        if row["type"]!="user" && row["isMeta"]==true && row["isCompactSummary"]!=true && !queued_input(row)
            && super::super::recovery::pending_input_token(&super::super::recovery::text_content(&row["message"]["content"])).is_none() {continue;}
        // Claude persists CLI-generated error/interruption notices as synthetic
        // assistant rows. Keep them in the raw archive, not in model replies.
        if row["isApiErrorMessage"]==true || row["message"]["model"]=="<synthetic>" {continue;}
        let content=blocks(&row["message"]["content"]);
        let create=|key:String,role,content:Value|NativeMessage{key,role,uuids:vec![id.clone()],content,
            timestamp:row["timestamp"].as_str().map(str::to_string),model:None,usage:None,stop_reason:None,tool_call_id:None,is_error:false,compact_summary:false,queued_input:false,parts:Vec::new()};
        match row["type"].as_str() {
            Some("attachment") if queued_input(row)=>{
                let mut entry=create(id.clone(),MessageRole::User,json!(queued_content(row)));
                entry.queued_input=true;
                messages.push(entry);
            }
            Some("assistant")=>{
                let key=format!("assistant:{}",row["message"]["id"].as_str().unwrap_or(id));
                let position=if let Some(position)=groups.get(&key) {*position} else {
                    let position=messages.len();groups.insert(key.clone(),position);
                    let mut entry=create(key,MessageRole::Assistant,json!([]));entry.uuids.clear();
                    entry.model=row["message"]["model"].as_str().map(str::to_string);
                    messages.push(entry);position
                };
                let entry=&mut messages[position];entry.uuids.push(id.clone());
                entry.parts.push(NativeAssistantPart { uuid:id.clone(),content:content.clone(),timestamp:row["timestamp"].as_str().map(str::to_string) });
                entry.content.as_array_mut().unwrap().extend(content);
                if row["message"]["usage"].is_object() {entry.usage=Some(row["message"]["usage"].clone());}
                if let Some(reason) = row["message"]["stop_reason"].as_str() { entry.stop_reason = Some(reason.to_string()); }
            }
            Some("user")=>{
                let results:Vec<_>=content.iter().filter(|b|b["type"]=="tool_result").collect();
                if results.is_empty() {
                    let mut entry=create(id.clone(),MessageRole::User,json!(content));entry.compact_summary=row["isCompactSummary"]==true;messages.push(entry);
                } else {for block in results {
                    let call=block["tool_use_id"].as_str().ok_or("Native tool result has no identity")?;
                    let mut entry=create(format!("{id}:{call}"),MessageRole::Tool,block.get("content").cloned().unwrap_or_else(||json!("")));
                    entry.tool_call_id=Some(call.to_string());entry.is_error=block["is_error"]==true;messages.push(entry);
                }}
            }
            _=>{}
        }
    }
    let mut pending=HashSet::new();let mut results=HashSet::new();
    for message in &messages {
        if message.role==MessageRole::Assistant {for block in blocks(&message.content) {
            if block["type"]=="tool_use" {if let Some(id)=block["id"].as_str(){pending.insert(id.to_string());}}
        }}
        if let Some(id)=&message.tool_call_id {results.insert(id.clone());}
    }
    let mut todos=Vec::new();let mut tasks=Vec::new();
    for (name,bytes) in files {
        if name.starts_with("dispatch/") {
            let record:Value=serde_json::from_slice(bytes).map_err(|_|"Invalid native dispatch receipt")?;
            if !matches!(record["version"].as_u64(),Some(1|2)) {return Err("Unsupported native dispatch receipt".into());}
            if record["settled"]!=true {pending.insert(record["callId"].as_str().ok_or("Invalid native dispatch identity")?.to_string());}
        } else if name.starts_with("tasks/") {
            let task:Value=serde_json::from_slice(bytes).map_err(|_|"Invalid native task file")?;
            if let Some(status)=task["status"].as_str().filter(|s|matches!(*s,"pending"|"in_progress"|"completed")) {
                todos.push(TodoItem{content:task["subject"].as_str().or_else(||task["description"].as_str()).unwrap_or_default().into(),status:status.into(),priority:"medium".into()});tasks.push(task);
            }
        }
    }
    // Locus exposes todowrite through MCP, so these lists live in the
    // transcript rather than Claude's native tasks directory. Reconstruct
    // only confirmed successful writes; absent task files must not erase them.
    let mut todo_kind = "tasks";
    if tasks.is_empty() {
        for message in messages.iter().filter(|m| m.role == MessageRole::Assistant) {
            for block in blocks(&message.content) {
                if block["type"] != "tool_use" || !matches!(block["name"].as_str(), Some("mcp__locus__todowrite" | "TodoWrite")) { continue; }
                if !messages.iter().any(|m| m.role == MessageRole::Tool && !m.is_error && m.tool_call_id.as_deref() == block["id"].as_str()) { continue; }
                todos = block["input"]["todos"].as_array().ok_or("Invalid confirmed Claude todo list")?.iter()
                    .map(|item| Ok(TodoItem {
                        content:item["content"].as_str().ok_or("Invalid confirmed Claude todo content")?.into(),
                        status:item["status"].as_str().ok_or("Invalid confirmed Claude todo status")?.into(),
                        priority:item["priority"].as_str().unwrap_or("medium").into(),
                    })).collect::<Result<Vec<_>,String>>()?;
                todo_kind = "todos";
            }
        }
    }
    pending.retain(|id|!results.contains(id));let mut pending_tool_ids:Vec<_>=pending.into_iter().collect();pending_tool_ids.sort();
    let mut snapshot=NativeSnapshot{version:1,native_id:info.native_id.clone(),digest:String::new(),messages,
        task_snapshot:json!({"version":1,"kind":todo_kind,"tasks":tasks,"items":todos}),todos,incomplete_tail:info.incomplete_tail,pending_tool_ids,hidden_user_uuids};
    snapshot.digest=digest(&serde_json::to_vec(&snapshot).map_err(|e|e.to_string())?);
    snapshot.validate(Some(&info.native_id))?;
    Ok(snapshot)
}
