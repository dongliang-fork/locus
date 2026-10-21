//! Local at-most-once journal. A crashed call with an unknown outcome is never
//! silently re-executed when a Claude transcript is resumed.
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub(super) struct Journal {
    root: PathBuf,
}

pub(super) const INTERRUPTED_DISPATCH_MESSAGE: &str = "Interrupted by a new user input. Local execution was cancelled; verify local state before retrying a mutation. No remote fallback was executed.";
pub(super) const RECOVERY_FAILURE: &str = "Tool failed because its result was lost when Locus was interrupted. The operation may have partially changed state or may still be running. Do not retry or repeat this operation; continue with the user's current request.";

fn interrupted_outcome(outcome: &Value) -> bool {
    let interrupted_text = |value: &Value| value.as_str().is_some_and(|text|
        text == crate::session::history::INTERRUPTED_TOOL_RESULT || text == INTERRUPTED_DISPATCH_MESSAGE);
    let result = &outcome["result"];
    outcome["interrupted"] == true || interrupted_text(&outcome["deny"]) || interrupted_text(result)
        || interrupted_text(&result["stdout"]) || result["interrupted"] == true
        || result.as_array().is_some_and(|blocks| blocks.iter().any(|block|
            block["type"] == "text" && interrupted_text(&block["text"])))
}
impl Journal {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    fn path(&self, id: &str, suffix: &str) -> PathBuf {
        self.root.join(format!(
            "{}.{suffix}.json",
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, id.as_bytes())
        ))
    }
    pub fn begin(&self, call: &Value) -> Result<Option<Value>, String> {
        let id = call["callId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Missing call ID")?;
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        // A recovery tombstone is independent of the possibly torn intent or
        // result file. It also fences dispatches that never reached this client.
        if let Some(failure) = self.recovery_failure(id)? { return Ok(Some(failure)); }
        let intent = self.path(id, "intent");
        if intent.exists() {
            let previous: Value =
                serde_json::from_slice(&std::fs::read(&intent).map_err(|e| e.to_string())?)
                    .map_err(|_| "Incomplete local tool journal; execution outcome is unknown")?;
            if previous["version"] != 1
                || previous["callId"] != call["callId"]
                || previous["tool"] != call["tool"]
                || !super::native_tools::same_operation(
                    id,
                    call["tool"].as_str().unwrap_or_default(),
                    &previous["input"],
                    &call["input"],
                )
            {
                return Err("Tool ID conflicts with its persisted local operation".into());
            }
            let result = self.path(id, "result");
            if !result.exists() {
                return Err("This call was interrupted with an unknown execution outcome. Inspect local state before explicitly requesting a new operation; it was not repeated.".into());
            }
            let value: Value =
                serde_json::from_slice(&std::fs::read(result).map_err(|e| e.to_string())?)
                    .map_err(|_| {
                        "Incomplete local result journal; the operation was not repeated"
                    })?;
            if value["callId"] != call["callId"] {
                return Err("Invalid local result journal".into());
            }
            return Ok(Some(value));
        }
        write_once(
            &intent,
            &json!({"version":1,"callId":call["callId"],"tool":call["tool"],"input":call["input"]}),
        )?;
        Ok(None)
    }
    pub fn complete(&self, id: &str, result: &Value) -> Result<(), String> {
        write_once(&self.path(id, "result"), result)
    }
    fn recovery_failure(&self, id: &str) -> Result<Option<Value>, String> {
        let path = self.path(id, "failed");
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid recovered tool failure")?;
        if value["version"] != 1 || value["callId"] != id || value["deny"] != RECOVERY_FAILURE {
            return Err("Invalid recovered tool failure identity".into());
        }
        Ok(Some(value))
    }
    /// Close unknown outcomes without executing anything or editing the native
    /// transcript. Keep the original files for diagnosis, including torn writes.
    pub fn recover_dispatch(&self, id: &str) -> Result<Value, String> {
        if self.recovery_failure(id)?.is_none() {
            if let Ok(Some(outcome)) = self.dispatch_outcome(id) {
                if outcome["status"] != "not_started" { return Ok(outcome); }
            }
            std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
            let intent=std::fs::read(self.path(id,"intent")).ok()
                .and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok())
                .filter(|call|call["version"]==1 && call["callId"]==id);
            let failure = json!({"version":1,"callId":id,"deny":RECOVERY_FAILURE,
                "tool":intent.as_ref().and_then(|call|call["tool"].as_str()),
                "input":intent.as_ref().map(|call|call["input"].to_string().chars().take(4000).collect::<String>())});
            if let Err(error) = write_once(&self.path(id, "failed"), &failure) {
                if self.recovery_failure(id)?.is_none() { return Err(error); }
            }
        }
        let failure=self.recovery_failure(id)?.ok_or("Recovered failure was not persisted")?;
        Ok(json!({"callId":id,"status":"failed","tool":failure["tool"],"input":failure["input"],"recordedOutcome":RECOVERY_FAILURE}))
    }
    /// Stop can leave a dispatch receipt without a native assistant row.
    /// Report only facts proven by this client's write-ahead journal; a
    /// cancellation receipt does not prove that local side effects rolled back.
    pub fn dispatch_outcome(&self, id: &str) -> Result<Option<Value>, String> {
        if !self.root.is_dir() { return Ok(None); }
        let intent = self.path(id, "intent");
        let result = self.path(id, "result");
        if !intent.exists() {
            return Ok((!result.exists()).then(|| json!({"callId":id,"status":"not_started"})));
        }
        if std::fs::metadata(&intent).map_err(|e|e.to_string())?.len() > 32 * 1024 * 1024 {
            return Err("Invalid local tool journal size".into());
        }
        let call: Value = serde_json::from_slice(&std::fs::read(intent).map_err(|e|e.to_string())?)
            .map_err(|_|"Invalid local tool journal")?;
        if call["version"] != 1 || call["callId"] != id || !call["input"].is_object() || !call["tool"].is_string() {
            return Err("Invalid local tool journal identity".into());
        }
        if !result.exists() { return Ok(None); }
        if std::fs::metadata(&result).map_err(|e|e.to_string())?.len() > 32 * 1024 * 1024 {
            return Err("Invalid local result journal size".into());
        }
        let outcome: Value = serde_json::from_slice(&std::fs::read(result).map_err(|e|e.to_string())?)
            .map_err(|_|"Invalid local result journal")?;
        if outcome["callId"] != id || (outcome.get("result").is_none() && !outcome["deny"].is_string()) {
            return Err("Invalid local result journal identity".into());
        }
        let excerpt = |value: &Value| value.to_string().chars().take(4000).collect::<String>();
        let status = if interrupted_outcome(&outcome) { "interrupted" }
            else if outcome["deny"].is_string() { "failed" } else { "completed" };
        Ok(Some(json!({"callId":id,"status":status,"tool":call["tool"],
            "input":excerpt(&call["input"]),"recordedOutcome":excerpt(&outcome)})))
    }
    /// Catch local effects that occurred after the last remote backup, including
    /// the crash window before an assistant row reached the Locus database.
    pub fn verify_archive(&self, confirmed:&std::collections::HashSet<String>, updated_at:i64)->Result<(),String> {
        let entries=match std::fs::read_dir(&self.root) {
            Ok(entries)=>entries,
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(()),
            Err(e)=>return Err(e.to_string()),
        };
        for entry in entries {
            let entry=entry.map_err(|e|e.to_string())?;
            if !entry.file_name().to_string_lossy().ends_with(".intent.json"){continue;}
            let metadata=entry.metadata().map_err(|e|e.to_string())?;
            if metadata.len()>32*1024*1024{return Err("Invalid local tool journal size".into());}
            let record:Value=serde_json::from_slice(&std::fs::read(entry.path()).map_err(|e|e.to_string())?).map_err(|_|"Invalid local tool journal")?;
            let id=record["callId"].as_str().ok_or("Invalid local tool journal identity")?;
            if confirmed.contains(id){continue;}
            let recent=metadata.modified().map_err(|e|e.to_string())?.duration_since(std::time::UNIX_EPOCH).map_err(|e|e.to_string())?.as_secs() as i64>=updated_at;
            if recent || !self.path(id,"result").is_file() {
                return Err("Local tool operations are newer than, or unconfirmed by, the native backup. Synchronize the source server before moving this conversation; tools were not repeated.".into());
            }
        }
        Ok(())
    }
}
fn write_once(path: &Path, value: &Value) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing journal directory")?)
        .map_err(|e| e.to_string())?;
    file.write_all(value.to_string().as_bytes())
        .and_then(|_| file.as_file().sync_all()).map_err(|e| e.to_string())?;
    file.persist_noclobber(path).map_err(|e| e.to_string())?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "child process for crash boundary test"]
    fn journal_crash_child() {
        let root=PathBuf::from(std::env::var("LOCUS_JOURNAL_CRASH_CHILD").unwrap());
        let journal=Journal::new(root.clone());
        journal.begin(&json!({"callId":"crash-write","tool":"Write","input":{"file_path":"effect","content":"written"}})).unwrap();
        // An external effect has completed, but no result has been committed.
        std::fs::write(root.join("effect"),"written once").unwrap();
        std::fs::write(root.join("ready"),"ready").unwrap();
        loop {std::thread::sleep(std::time::Duration::from_millis(20));}
    }

    #[test]
    fn killed_executor_is_recovered_as_failed_and_never_reexecuted() {
        let root=tempfile::tempdir().unwrap();
        let mut command=std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact","llm::claude_code_cli::native_journal::tests::journal_crash_child","--ignored"])
            .env("LOCUS_JOURNAL_CRASH_CHILD",root.path()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
        let mut child=command.spawn().unwrap();
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(20);
        while !root.path().join("ready").is_file() && std::time::Instant::now()<deadline {
            if child.try_wait().unwrap().is_some(){break;}
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ready=root.path().join("ready").is_file();
        let _=child.kill();let _=child.wait();
        assert!(ready,"isolated executor failed to reach the crash boundary");
        let restarted=Journal::new(root.path().into());
        assert_eq!(restarted.recover_dispatch("crash-write").unwrap()["status"],"failed");
        for _ in 0..3 {
            let reply=restarted.begin(&json!({"callId":"crash-write","tool":"Write","input":{"file_path":"effect","content":"written"}})).unwrap().unwrap();
            assert_eq!(reply["deny"],RECOVERY_FAILURE);
        }
        assert_eq!(std::fs::read_to_string(root.path().join("effect")).unwrap(),"written once");
    }
    #[test]
    fn dispatch_receipts_distinguish_interruption_failure_and_completion() {
        let cancelled = crate::session::history::INTERRUPTED_TOOL_RESULT;
        for (payload, status) in [
            (json!({"result":[{"type":"text","text":cancelled}]}), "interrupted"),
            (json!({"result":{"stdout":cancelled,"interrupted":false}}), "interrupted"),
            (json!({"result":{"stdout":"partial","interrupted":true}}), "interrupted"),
            (json!({"deny":cancelled}), "interrupted"),
            (json!({"deny":INTERRUPTED_DISPATCH_MESSAGE}), "interrupted"),
            (json!({"deny":"tool failed"}), "failed"),
            (json!({"result":{"type":"create","content":cancelled}}), "completed"),
        ] {
            let root = tempfile::tempdir().unwrap(); let journal = Journal::new(root.path().into());
            let call = json!({"callId":"call","tool":"mcp__locus__unity_execute","input":{"code":"inspect();"}});
            journal.begin(&call).unwrap();
            let mut result = payload; result["callId"] = json!("call"); journal.complete("call",&result).unwrap();
            assert_eq!(journal.dispatch_outcome("call").unwrap().unwrap()["status"],status);
        }
    }

    #[test]
    fn restart_replays_completed_calls_and_refuses_uncertain_or_changed_writes() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::new(root.path().to_owned());
        let call =
            json!({"callId":"write-1","tool":"Write","input":{"file_path":"a","content":"new"}});
        assert!(journal.begin(&call).unwrap().is_none());
        let restarted = Journal::new(root.path().to_owned());
        assert!(restarted.begin(&call).unwrap_err().contains("unknown"));
        let result = json!({"callId":"write-1","result":{"type":"create"}});
        journal.complete("write-1", &result).unwrap();
        assert_eq!(restarted.begin(&call).unwrap(), Some(result));
        let mut changed = call;
        changed["input"]["content"] = json!("changed");
        assert!(restarted.begin(&changed).is_err());
    }
    #[test]
    fn claude_archive_blocks_effects_missing_from_stale_backup() {
        let root=tempfile::tempdir().unwrap();let journal=Journal::new(root.path().into());
        let call=json!({"callId":"effect","tool":"Write","input":{"content":"one"}});
        journal.begin(&call).unwrap();
        assert!(journal.verify_archive(&Default::default(),i64::MAX).is_err());
        journal.complete("effect",&json!({"callId":"effect"})).unwrap();
        assert!(journal.verify_archive(&Default::default(),0).is_err());
        assert!(journal.verify_archive(&std::collections::HashSet::from(["effect".into()]),0).is_ok());
        assert!(journal.verify_archive(&Default::default(),i64::MAX).is_ok());
    }
}
