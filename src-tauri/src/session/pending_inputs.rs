use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use super::models::{
    AssetRefData, ImageData, PendingSessionInput, UserIntentPayload, UserIntentSkill,
};

const STATUS_QUEUED: &str = "queued";
const STATUS_DELIVERING: &str = "delivering";
/// Kept for the user to edit, resend or delete. Never claimed automatically.
pub const STATUS_RETAINED: &str = "retained";
pub const DELIVERY_AFTER_RUN: &str = "after_run";
pub const DELIVERY_IMMEDIATE: &str = "immediate";

#[derive(Default)]
pub struct PendingInputQueue {
    inputs: HashMap<(String, String), PendingSessionInput>,
    delivering: HashMap<String, PendingSessionInput>,
    // Claims whose consumption cannot be ruled out after an interruption, such
    // as input already handed to Claude's native session. Never resent.
    ambiguous: HashSet<String>,
    compact_runs: HashSet<(String, String)>,
    revision: u64,
}

/// Entries changed outside a command reply. `absorbed_ids` were merged into
/// another entry and no longer exist.
#[derive(Debug, Default)]
pub struct PendingInputChanges {
    pub inputs: Vec<PendingSessionInput>,
    pub absorbed_ids: Vec<String>,
}

impl PendingInputChanges {
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.absorbed_ids.is_empty()
    }

    fn record(&mut self, input: &PendingSessionInput, absorbed: Option<String>) {
        if let Some(id) = absorbed.filter(|id| id != &input.id) {
            self.inputs.retain(|item| item.id != id);
            self.absorbed_ids.push(id);
        }
        self.inputs.retain(|item| item.id != input.id);
        self.inputs.push(input.clone());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSettlement {
    /// The run finished normally. Unconsumed claims become the next turn.
    Completed,
    /// The run failed. Its undelivered inputs stay visible for the user.
    Failed,
}

fn touch(revision: &mut u64, input: &mut PendingSessionInput) {
    *revision += 1;
    input.revision = *revision;
    input.updated_at = now_ts();
}

impl PendingInputQueue {
    pub fn queue_input(&mut self, request: QueuePendingInputRequest) -> PendingSessionInput {
        let key = (request.session_id.clone(), request.run_id.clone());
        let now = now_ts();
        if let Some(existing) = self.inputs.get_mut(&key) {
            existing.text = join_pending_text(&existing.text, &request.text);
            existing.display_text =
                join_pending_text(&existing.display_text, &request.display_text);
            existing.delivery =
                merge_pending_delivery(&existing.delivery, request.delivery.as_deref()).to_string();
            existing.images = merge_optional_vec(existing.images.take(), request.images);
            existing.asset_refs =
                merge_optional_vec(existing.asset_refs.take(), request.asset_refs);
            existing.mode = merge_pending_mode(existing.mode.take(), request.mode);
            existing.client_message_id = existing
                .client_message_id
                .take()
                .or(request.client_message_id.clone());
            existing.user_intent = merge_user_intents(
                existing.user_intent.take(),
                request.user_intent,
                existing.client_message_id.clone(),
            );
            touch(&mut self.revision, existing);
            return existing.clone();
        }

        self.revision += 1;
        let input = PendingSessionInput {
            id: Uuid::new_v4().to_string(),
            session_id: request.session_id,
            run_id: request.run_id,
            merge_group_id: request.merge_group_id,
            status: STATUS_QUEUED.to_string(),
            delivery: normalize_delivery(request.delivery.as_deref()).to_string(),
            text: request.text,
            display_text: request.display_text,
            images: empty_to_none(request.images),
            asset_refs: empty_to_none(request.asset_refs),
            mode: request.mode,
            user_intent: request.user_intent,
            client_message_id: request.client_message_id,
            message_id: None,
            created_at: now,
            updated_at: now,
            revision: self.revision,
        };
        self.inputs.insert(key, input.clone());
        input
    }

    pub fn claim_immediate(&mut self, session_id: &str, run_id: &str) -> Vec<PendingSessionInput> {
        let key = (session_id.to_string(), run_id.to_string());
        match self.inputs.remove(&key) {
            Some(mut input)
                if input.status == STATUS_QUEUED && input.delivery == DELIVERY_IMMEDIATE =>
            {
                input.status = STATUS_DELIVERING.to_string();
                touch(&mut self.revision, &mut input);
                vec![input]
            }
            Some(input) => {
                self.inputs.insert(key, input);
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    pub fn claim_after_run(
        &mut self,
        session_id: &str,
        run_id: &str,
    ) -> Option<PendingSessionInput> {
        let key = (session_id.to_string(), run_id.to_string());
        match self.inputs.remove(&key) {
            Some(mut input)
                if input.status == STATUS_QUEUED && input.delivery == DELIVERY_AFTER_RUN =>
            {
                input.status = STATUS_DELIVERING.to_string();
                touch(&mut self.revision, &mut input);
                Some(input)
            }
            Some(input) => {
                self.inputs.insert(key, input);
                None
            }
            None => None,
        }
    }

    /// `ambiguous` marks a claim whose consumption cannot be disproved once it
    /// has been handed over, so settlement retains it instead of resending it.
    pub fn claim_for_steering(
        &mut self,
        session_id: &str,
        run_id: &str,
        ambiguous: bool,
    ) -> Option<PendingSessionInput> {
        let input = self.claim_immediate(session_id, run_id).pop()?;
        if ambiguous {
            self.ambiguous.insert(input.id.clone());
        }
        self.delivering.insert(input.id.clone(), input.clone());
        Some(input)
    }

    pub fn finish_steering(&mut self, id: &str) {
        self.delivering.remove(id);
        self.ambiguous.remove(id);
    }

    pub fn queue_compact(&mut self, session_id: &str, run_id: &str) -> bool {
        self.compact_runs
            .insert((session_id.to_string(), run_id.to_string()))
    }

    pub fn claim_compact(&mut self, session_id: &str, run_id: &str) -> bool {
        self.compact_runs
            .remove(&(session_id.to_string(), run_id.to_string()))
    }

    pub fn restore_compact(&mut self, session_id: &str, run_id: &str) {
        self.compact_runs
            .insert((session_id.to_string(), run_id.to_string()));
    }

    pub fn has_compact(&self, session_id: &str, run_id: &str) -> bool {
        self.compact_runs
            .contains(&(session_id.to_string(), run_id.to_string()))
    }

    pub fn rebind_input_run(
        &mut self,
        session_id: &str,
        from_run_id: &str,
        to_run_id: &str,
    ) -> Option<PendingSessionInput> {
        let from_key = (session_id.to_string(), from_run_id.to_string());
        let mut input = self.inputs.remove(&from_key)?;
        input.run_id = to_run_id.to_string();
        touch(&mut self.revision, &mut input);
        self.inputs.insert(
            (session_id.to_string(), to_run_id.to_string()),
            input.clone(),
        );
        Some(input)
    }

    pub fn promote_to_immediate(
        &mut self,
        session_id: &str,
        run_id: &str,
        pending_input_id: Option<&str>,
    ) -> Option<PendingSessionInput> {
        let key = (session_id.to_string(), run_id.to_string());
        let input = self.inputs.get_mut(&key)?;
        if input.status != STATUS_QUEUED {
            return None;
        }
        if pending_input_id.is_some_and(|id| id != input.id) {
            return None;
        }
        input.delivery = DELIVERY_IMMEDIATE.to_string();
        touch(&mut self.revision, input);
        Some(input.clone())
    }

    pub fn delete_input(
        &mut self,
        session_id: &str,
        run_id: &str,
        pending_input_id: Option<&str>,
    ) -> Option<PendingSessionInput> {
        let key = (session_id.to_string(), run_id.to_string());
        let input = self.inputs.get(&key)?;
        if let Some(id) = pending_input_id {
            if id != input.id {
                return None;
            }
        }
        self.inputs.remove(&key)
    }

    pub fn restore_claimed(&mut self, inputs: Vec<PendingSessionInput>) -> PendingInputChanges {
        let mut changes = PendingInputChanges::default();
        for mut input in inputs {
            self.delivering.remove(&input.id);
            self.ambiguous.remove(&input.id);
            input.status = STATUS_QUEUED.to_string();
            let key = (input.session_id.clone(), input.run_id.clone());
            let absorbed = self.inputs.remove(&key).map(|newer| {
                let id = newer.id.clone();
                absorb_pending_input(&mut input, newer);
                id
            });
            touch(&mut self.revision, &mut input);
            changes.record(&input, absorbed);
            self.inputs.insert(key, input);
        }
        changes
    }

    /// Returns steering claims that are still outstanding. A claim already
    /// committed, cleared or settled by a terminal run event is left alone, so
    /// late cleanup can never resurrect it.
    pub fn restore_steering(&mut self, inputs: Vec<PendingSessionInput>) -> PendingInputChanges {
        let inputs = inputs
            .into_iter()
            .filter(|input| self.delivering.contains_key(&input.id))
            .collect();
        self.restore_claimed(inputs)
    }

    /// Resolves the run's remaining inputs when it reaches a terminal state.
    /// Outstanding steering claims were not committed to the local history.
    pub fn settle_run(
        &mut self,
        session_id: &str,
        run_id: &str,
        settlement: RunSettlement,
    ) -> PendingInputChanges {
        let mut changes = PendingInputChanges::default();
        let mut leftovers = self
            .delivering
            .values()
            .filter(|input| input.session_id == session_id && input.run_id == run_id)
            .cloned()
            .collect::<Vec<_>>();
        leftovers.sort_by_key(|input| (input.created_at, input.revision));
        let retained = settlement == RunSettlement::Failed
            || leftovers.iter().any(|input| self.ambiguous.contains(&input.id));
        for input in &leftovers {
            self.delivering.remove(&input.id);
            self.ambiguous.remove(&input.id);
        }
        let key = (session_id.to_string(), run_id.to_string());
        let had_leftovers = !leftovers.is_empty();
        let mut parts = leftovers.into_iter().chain(self.inputs.remove(&key));
        let Some(mut merged) = parts.next() else {
            return changes;
        };
        let original = (merged.status.clone(), merged.delivery.clone());
        let mut absorbed = Vec::new();
        for newer in parts {
            absorbed.push(newer.id.clone());
            absorb_pending_input(&mut merged, newer);
        }
        if retained {
            merged.status = STATUS_RETAINED.to_string();
        } else {
            // The after-run scheduler delivers it as the next turn.
            merged.status = STATUS_QUEUED.to_string();
            merged.delivery = DELIVERY_AFTER_RUN.to_string();
        }
        let changed = had_leftovers
            || !absorbed.is_empty()
            || original != (merged.status.clone(), merged.delivery.clone());
        if changed {
            touch(&mut self.revision, &mut merged);
            changes.record(&merged, None);
            for id in absorbed {
                if id != merged.id {
                    changes.absorbed_ids.push(id);
                }
            }
        }
        self.inputs.insert(key, merged);
        changes
    }

    /// Clients re-send a cancelled run's queued inputs themselves. Input that
    /// was already in flight is never resent automatically, so it is retained
    /// for the user instead of disappearing with the run.
    pub fn settle_cancelled_run(&mut self, session_id: &str, run_id: &str) -> PendingInputChanges {
        let key = (session_id.to_string(), run_id.to_string());
        self.inputs.remove(&key);
        self.compact_runs.remove(&key);
        self.settle_run(session_id, run_id, RunSettlement::Failed)
    }

    pub fn clear_run(&mut self, session_id: &str, run_id: &str) {
        let key = (session_id.to_string(), run_id.to_string());
        self.inputs.remove(&key);
        self.compact_runs.remove(&key);
        let claimed = self
            .delivering
            .values()
            .filter(|input| input.session_id == session_id && input.run_id == run_id)
            .map(|input| input.id.clone())
            .collect::<Vec<_>>();
        for id in claimed {
            self.delivering.remove(&id);
            self.ambiguous.remove(&id);
        }
    }

    pub fn list_session(&self, session_id: &str) -> Vec<PendingSessionInput> {
        let mut inputs = self
            .delivering
            .values()
            .chain(self.inputs.values())
            .filter(|input| input.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        inputs.sort_by_key(|input| (input.created_at, input.updated_at));
        inputs
    }
}

fn absorb_pending_input(target: &mut PendingSessionInput, newer: PendingSessionInput) {
    target.text = join_pending_text(&target.text, &newer.text);
    target.display_text = join_pending_text(&target.display_text, &newer.display_text);
    target.images = merge_optional_vec(target.images.take(), newer.images.unwrap_or_default());
    target.asset_refs =
        merge_optional_vec(target.asset_refs.take(), newer.asset_refs.unwrap_or_default());
    target.mode = merge_pending_mode(target.mode.take(), newer.mode);
    target.user_intent = merge_user_intents(
        target.user_intent.take(),
        newer.user_intent,
        target.client_message_id.clone(),
    );
    target.delivery = merge_pending_delivery(&target.delivery, Some(&newer.delivery)).to_string();
}

pub struct QueuePendingInputRequest {
    pub session_id: String,
    pub run_id: String,
    pub merge_group_id: String,
    pub text: String,
    pub display_text: String,
    pub images: Vec<ImageData>,
    pub asset_refs: Vec<AssetRefData>,
    pub mode: Option<String>,
    pub user_intent: Option<UserIntentPayload>,
    pub client_message_id: Option<String>,
    pub delivery: Option<String>,
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn empty_to_none<T>(items: Vec<T>) -> Option<Vec<T>> {
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn merge_optional_vec<T>(existing: Option<Vec<T>>, next: Vec<T>) -> Option<Vec<T>> {
    let mut merged = existing.unwrap_or_default();
    merged.extend(next);
    empty_to_none(merged)
}

fn join_pending_text(existing: &str, next: &str) -> String {
    let existing_trimmed = existing.trim();
    let next_trimmed = next.trim();
    match (existing_trimmed.is_empty(), next_trimmed.is_empty()) {
        (true, true) => String::new(),
        (true, false) => next.to_string(),
        (false, true) => existing.to_string(),
        (false, false) => format!("{}\n{}", existing, next),
    }
}

fn merge_pending_mode(existing: Option<String>, next: Option<String>) -> Option<String> {
    if existing.as_deref() == Some("plan") || next.as_deref() == Some("plan") {
        return Some("plan".to_string());
    }
    next.filter(|value| !value.trim().is_empty()).or(existing)
}

fn normalize_delivery(value: Option<&str>) -> &'static str {
    match value {
        Some(DELIVERY_IMMEDIATE) => DELIVERY_IMMEDIATE,
        _ => DELIVERY_AFTER_RUN,
    }
}

fn merge_pending_delivery(existing: &str, next: Option<&str>) -> &'static str {
    if existing == DELIVERY_IMMEDIATE || normalize_delivery(next) == DELIVERY_IMMEDIATE {
        DELIVERY_IMMEDIATE
    } else {
        DELIVERY_AFTER_RUN
    }
}

fn merge_user_intents(
    existing: Option<UserIntentPayload>,
    next: Option<UserIntentPayload>,
    client_message_id: Option<String>,
) -> Option<UserIntentPayload> {
    match (existing, next) {
        (None, None) => client_message_id.map(|id| UserIntentPayload {
            kind: "user_intent_v1".to_string(),
            mode: "build".to_string(),
            skills: Vec::new(),
            client_message_id: Some(id),
        }),
        (Some(mut existing), None) => {
            if existing.client_message_id.is_none() {
                existing.client_message_id = client_message_id;
            }
            Some(existing)
        }
        (None, Some(mut next)) => {
            if next.client_message_id.is_none() {
                next.client_message_id = client_message_id;
            }
            Some(next)
        }
        (Some(mut existing), Some(next)) => {
            if next.mode == "plan" || existing.mode == "plan" {
                existing.mode = "plan".to_string();
            }
            let mut seen = HashMap::<String, ()>::new();
            let mut skills = Vec::<UserIntentSkill>::new();
            for skill in existing.skills.into_iter().chain(next.skills) {
                let key = format!("{}:{}", skill.source, skill.dir_name);
                if seen.insert(key, ()).is_some() {
                    continue;
                }
                skills.push(skill);
            }
            existing.skills = skills;
            if existing.client_message_id.is_none() {
                existing.client_message_id = next.client_message_id.or(client_message_id);
            }
            Some(existing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(run_id: &str, group_id: &str, text: &str) -> QueuePendingInputRequest {
        QueuePendingInputRequest {
            session_id: "session-1".to_string(),
            run_id: run_id.to_string(),
            merge_group_id: group_id.to_string(),
            text: text.to_string(),
            display_text: text.to_string(),
            images: Vec::new(),
            asset_refs: Vec::new(),
            mode: Some("build".to_string()),
            user_intent: None,
            client_message_id: Some(group_id.to_string()),
            delivery: Some(DELIVERY_AFTER_RUN.to_string()),
        }
    }

    #[test]
    fn queue_merges_by_run_even_with_different_groups() {
        let mut queue = PendingInputQueue::default();
        let first = queue.queue_input(request("run-1", "group-a", "first"));
        let second = queue.queue_input(request("run-1", "group-b", "second"));

        assert_eq!(second.id, first.id);
        assert_eq!(second.merge_group_id, "group-a");
        assert_eq!(second.text, "first\nsecond");
        assert_eq!(queue.list_session("session-1").len(), 1);
    }

    #[test]
    fn claim_removes_until_restored() {
        let mut queue = PendingInputQueue::default();
        let mut request = request("run-1", "group-a", "first");
        request.delivery = Some(DELIVERY_IMMEDIATE.to_string());
        let first = queue.queue_input(request);

        let claimed = queue.claim_immediate("session-1", "run-1");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, first.id);
        assert!(queue.list_session("session-1").is_empty());

        queue.restore_claimed(claimed);
        assert_eq!(queue.list_session("session-1").len(), 1);
    }

    #[test]
    fn after_run_inputs_are_not_claimed_until_promoted_or_completed() {
        let mut queue = PendingInputQueue::default();
        let first = queue.queue_input(request("run-1", "group-a", "first"));

        assert!(queue.claim_immediate("session-1", "run-1").is_empty());
        let promoted = queue
            .promote_to_immediate("session-1", "run-1", Some(&first.id))
            .expect("promote queued input");
        assert_eq!(promoted.delivery, DELIVERY_IMMEDIATE);
        let claimed = queue.claim_immediate("session-1", "run-1");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, first.id);
    }

    #[test]
    fn after_run_inputs_can_be_claimed_for_next_turn() {
        let mut queue = PendingInputQueue::default();
        let first = queue.queue_input(request("run-1", "group-a", "first"));

        let claimed = queue
            .claim_after_run("session-1", "run-1")
            .expect("claim after-run input");
        assert_eq!(claimed.id, first.id);
        assert!(queue.list_session("session-1").is_empty());
    }

    #[test]
    fn queued_input_can_be_deleted_by_id() {
        let mut queue = PendingInputQueue::default();
        let first = queue.queue_input(request("run-1", "group-a", "first"));

        assert!(queue
            .delete_input("session-1", "run-1", Some("different-id"))
            .is_none());
        assert_eq!(queue.list_session("session-1").len(), 1);

        let deleted = queue
            .delete_input("session-1", "run-1", Some(&first.id))
            .expect("delete queued input");
        assert_eq!(deleted.id, first.id);
        assert!(queue.list_session("session-1").is_empty());
    }

    #[test]
    fn promoted_input_can_be_deleted_before_claim() {
        let mut queue = PendingInputQueue::default();
        let first = queue.queue_input(request("run-1", "group-a", "first"));
        queue
            .promote_to_immediate("session-1", "run-1", Some(&first.id))
            .expect("promote queued input");

        let deleted = queue
            .delete_input("session-1", "run-1", Some(&first.id))
            .expect("delete promoted input");
        assert_eq!(deleted.delivery, DELIVERY_IMMEDIATE);
        assert!(queue.claim_immediate("session-1", "run-1").is_empty());
    }

    #[test]
    fn compact_requests_are_idempotent_and_claimed_once() {
        let mut queue = PendingInputQueue::default();

        assert!(queue.queue_compact("session-1", "run-1"));
        assert!(!queue.queue_compact("session-1", "run-1"));
        assert!(queue.has_compact("session-1", "run-1"));
        assert!(queue.claim_compact("session-1", "run-1"));
        assert!(!queue.claim_compact("session-1", "run-1"));
    }

    #[test]
    fn queued_input_can_follow_a_compact_run() {
        let mut queue = PendingInputQueue::default();
        let first = queue.queue_input(request("run-1", "group-a", "first"));

        let rebound = queue
            .rebind_input_run("session-1", "run-1", "run-compact")
            .expect("rebind pending input");

        assert_eq!(rebound.id, first.id);
        assert_eq!(rebound.run_id, "run-compact");
        assert!(queue.claim_after_run("session-1", "run-1").is_none());
        assert_eq!(
            queue
                .claim_after_run("session-1", "run-compact")
                .expect("claim rebound input")
                .id,
            first.id
        );
    }

    #[test]
    fn steering_claim_stays_visible_without_being_claimed_or_deleted_again() {
        let mut queue = PendingInputQueue::default();
        let mut next = request("run-1", "group-a", "first");
        next.delivery = Some(DELIVERY_IMMEDIATE.to_string());
        let original = queue.queue_input(next);
        let claimed = queue.claim_for_steering("session-1", "run-1", false).unwrap();
        assert_eq!(claimed.id, original.id);
        assert_eq!(queue.list_session("session-1")[0].status, STATUS_DELIVERING);
        assert!(queue.claim_immediate("session-1", "run-1").is_empty());
        assert!(queue.delete_input("session-1", "run-1", Some(&original.id)).is_none());
        assert!(queue.claim_after_run("session-1", "run-1").is_none());
        queue.finish_steering(&original.id);
        assert!(queue.list_session("session-1").is_empty());
    }

    #[test]
    fn restoring_steering_preserves_newer_input_and_does_not_schedule_automatic_replay() {
        let mut queue = PendingInputQueue::default();
        let mut next = request("run-1", "group-a", "first");
        next.delivery = Some(DELIVERY_IMMEDIATE.to_string());
        queue.queue_input(next);
        let claimed = queue.claim_for_steering("session-1", "run-1", false).unwrap();
        queue.queue_input(request("run-1", "group-b", "second"));
        assert_eq!(queue.list_session("session-1").len(), 2);
        assert_eq!(queue.list_session("session-1")[0].text, "first");
        queue.restore_claimed(vec![claimed]);
        let restored = queue.list_session("session-1");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].text, "first\nsecond");
        assert!(queue.claim_after_run("session-1", "run-1").is_none());
        queue.clear_run("session-1", "run-1");
        assert!(queue.list_session("session-1").is_empty());
    }

    fn immediate(run_id: &str, group_id: &str, text: &str) -> QueuePendingInputRequest {
        let mut next = request(run_id, group_id, text);
        next.delivery = Some(DELIVERY_IMMEDIATE.to_string());
        next
    }

    #[test]
    fn revisions_order_every_state_change() {
        let mut queue = PendingInputQueue::default();
        let queued = queue.queue_input(immediate("run-1", "group-a", "first"));
        let claimed = queue.claim_for_steering("session-1", "run-1", false).unwrap();
        let restored = queue.restore_steering(vec![claimed.clone()]);
        assert!(queued.revision < claimed.revision);
        assert!(claimed.revision < restored.inputs[0].revision);
        assert_eq!(restored.inputs[0].status, STATUS_QUEUED);
    }

    #[test]
    fn steering_restore_cannot_resurrect_settled_or_cleared_claims() {
        let mut queue = PendingInputQueue::default();
        queue.queue_input(immediate("run-1", "group-a", "first"));
        let claimed = queue.claim_for_steering("session-1", "run-1", false).unwrap();
        queue.clear_run("session-1", "run-1");
        assert!(queue.restore_steering(vec![claimed]).is_empty());
        assert!(queue.list_session("session-1").is_empty());

        queue.queue_input(immediate("run-2", "group-b", "second"));
        let claimed = queue.claim_for_steering("session-1", "run-2", false).unwrap();
        queue.finish_steering(&claimed.id);
        assert!(queue.restore_steering(vec![claimed]).is_empty());
        assert!(queue.list_session("session-1").is_empty());
    }

    #[test]
    fn completed_run_turns_uncommitted_steering_into_the_next_turn() {
        let mut queue = PendingInputQueue::default();
        queue.queue_input(immediate("run-1", "group-a", "first"));
        let claimed = queue.claim_for_steering("session-1", "run-1", false).unwrap();
        let newer = queue.queue_input(request("run-1", "group-b", "second"));

        let changes = queue.settle_run("session-1", "run-1", RunSettlement::Completed);
        assert_eq!(changes.absorbed_ids, vec![newer.id]);
        assert_eq!(changes.inputs.len(), 1);
        assert_eq!(changes.inputs[0].id, claimed.id);
        assert_eq!(changes.inputs[0].status, STATUS_QUEUED);
        assert_eq!(changes.inputs[0].delivery, DELIVERY_AFTER_RUN);

        let next = queue.claim_after_run("session-1", "run-1").unwrap();
        assert_eq!(next.text, "first\nsecond");
        assert!(queue.restore_steering(vec![claimed]).is_empty());
    }

    #[test]
    fn completed_run_leaves_plain_follow_ups_untouched() {
        let mut queue = PendingInputQueue::default();
        queue.queue_input(request("run-1", "group-a", "later"));
        assert!(queue.settle_run("session-1", "run-1", RunSettlement::Completed).is_empty());
        assert!(queue.claim_after_run("session-1", "run-1").is_some());
    }

    #[test]
    fn ambiguous_claims_are_retained_instead_of_resent() {
        let mut queue = PendingInputQueue::default();
        queue.queue_input(immediate("run-1", "group-a", "native"));
        let claimed = queue.claim_for_steering("session-1", "run-1", true).unwrap();
        let changes = queue.settle_run("session-1", "run-1", RunSettlement::Completed);
        assert_eq!(changes.inputs[0].status, STATUS_RETAINED);
        assert!(queue.claim_after_run("session-1", "run-1").is_none());
        assert!(queue.claim_immediate("session-1", "run-1").is_empty());
        assert!(queue.promote_to_immediate("session-1", "run-1", Some(&claimed.id)).is_none());
        assert!(queue.delete_input("session-1", "run-1", Some(&claimed.id)).is_some());
    }

    #[test]
    fn cancelled_run_drops_resendable_queue_but_retains_in_flight_input() {
        let mut queue = PendingInputQueue::default();
        queue.queue_input(immediate("run-1", "group-a", "in flight"));
        let claimed = queue.claim_for_steering("session-1", "run-1", false).unwrap();
        let queued = queue.queue_input(request("run-1", "group-b", "client resends this"));
        queue.queue_compact("session-1", "run-1");

        let changes = queue.settle_cancelled_run("session-1", "run-1");
        assert!(changes.absorbed_ids.is_empty());
        assert_eq!(changes.inputs.len(), 1);
        assert_eq!(changes.inputs[0].id, claimed.id);
        assert_eq!(changes.inputs[0].text, "in flight");
        assert_eq!(changes.inputs[0].status, STATUS_RETAINED);
        assert!(!queue.has_compact("session-1", "run-1"));
        assert!(queue.list_session("session-1").iter().all(|input| input.id != queued.id));
        assert!(queue.restore_steering(vec![claimed]).is_empty());
    }

    #[test]
    fn failed_run_retains_queued_and_claimed_inputs_for_the_user() {
        let mut queue = PendingInputQueue::default();
        queue.queue_input(immediate("run-1", "group-a", "claimed"));
        queue.claim_for_steering("session-1", "run-1", false).unwrap();
        queue.queue_input(request("run-1", "group-b", "queued"));

        let changes = queue.settle_run("session-1", "run-1", RunSettlement::Failed);
        let retained = &changes.inputs[0];
        assert_eq!(retained.status, STATUS_RETAINED);
        assert_eq!(retained.text, "claimed\nqueued");
        assert_eq!(changes.absorbed_ids.len(), 1);
        assert_eq!(queue.list_session("session-1").len(), 1);
        assert!(queue.claim_after_run("session-1", "run-1").is_none());
        assert!(queue.delete_input("session-1", "run-1", Some(&retained.id)).is_some());
    }
}
