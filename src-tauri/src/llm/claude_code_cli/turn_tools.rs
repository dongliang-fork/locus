//! Per-turn tool identities. Streaming starts are not completed calls, and a
//! second observer of a native result must never duplicate persistence/execution.
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct TurnTools {
    pub calls: HashMap<String, Value>,
    started: HashSet<String>,
    completed: HashSet<String>,
}
impl TurnTools {
    pub fn start(&mut self, id: &str) { self.started.insert(id.into()); }
    pub fn announce(&mut self, block: &Value) -> Result<bool, String> {
        let id = block["id"].as_str().filter(|id| !id.is_empty()).ok_or("Claude tool has no identity")?;
        if let Some(previous) = self.calls.get(id) {
            if previous["name"] != block["name"] || previous["input"] != block["input"] {
                return Err("Claude reused a tool identity for a different operation; no tool was repeated".into());
            }
            return Ok(false);
        }
        self.calls.insert(id.into(), block.clone());
        self.start(id);
        Ok(true)
    }
    pub fn complete(&mut self, id: &str) -> Result<bool, String> {
        if !self.calls.contains_key(id) {
            return Err("Claude returned a result without its tool envelope. Resume from native history; no result was fabricated.".into());
        }
        Ok(self.completed.insert(id.into()))
    }
    pub fn has_pending(&self) -> bool {
        self.started.iter().any(|id| !self.completed.contains(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn concurrent_results_and_native_history_fallback_are_recorded_exactly_once() {
        let mut tools = TurnTools::default();
        for id in ["a", "b", "c", "d"] {
            tools.start(id);
            tools.announce(&json!({"id":id,"name":"WebSearch","input":{"query":id}})).unwrap();
        }
        for id in ["d", "b", "c", "a"] {
            assert!(tools.complete(id).unwrap());
            assert!(!tools.complete(id).unwrap()); // session.append / .catch replay
        }
        assert!(!tools.has_pending());
    }
    #[test]
    fn an_incomplete_streamed_call_cannot_leave_a_running_card_after_complete() {
        let mut tools = TurnTools::default();
        tools.start("only-streamed");
        assert!(tools.has_pending());
        assert!(tools.complete("only-streamed").is_err());
    }
    #[test]
    fn a_reserved_dispatch_cannot_execute_twice_or_change_its_arguments() {
        let mut tools = TurnTools::default();
        let call = json!({"id":"write","name":"Write","input":{"content":"one"}});
        assert!(tools.announce(&call).unwrap());
        assert!(tools.complete("write").unwrap());
        assert!(!tools.announce(&call).unwrap());
        assert!(!tools.complete("write").unwrap());
        assert!(tools.announce(&json!({"id":"write","name":"Write","input":{"content":"two"}})).is_err());
    }
}
