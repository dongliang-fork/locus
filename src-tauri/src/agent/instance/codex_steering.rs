use super::*;
use codex::steering::{SteeringInput, SteeringSource};

struct ClaimedInput {
    input: PendingSessionInput,
    suffix: Option<String>,
    // None is connection-owned or awaiting an acknowledgement. The bool
    // places committed input before or after the returned provider response.
    ready: Option<bool>,
    claude_receipt: Option<serde_json::Value>,
}

pub(super) struct RunSteering<'a> {
    instance: &'a AgentInstance,
    app: &'a AppHandle,
    store: &'a SessionStore,
    run_id: &'a str,
    env_prefix: Option<&'a str>,
    queue: crate::PendingInputQueueHandle,
    claimed: Mutex<Vec<ClaimedInput>>,
}

impl<'a> RunSteering<'a> {
    pub fn new(
        instance: &'a AgentInstance,
        app: &'a AppHandle,
        store: &'a SessionStore,
        run_id: &'a str,
        env_prefix: Option<&'a str>,
    ) -> Self {
        Self {
            instance,
            app,
            store,
            run_id,
            env_prefix,
            queue: app
                .state::<crate::PendingInputQueueHandle>()
                .inner()
                .clone(),
            claimed: Mutex::new(Vec::new()),
        }
    }

    /// Returns whether any input became a user message, so callers continue the
    /// run even when the model's own response would have ended it.
    pub fn persist_ready(&self, before_response: bool) -> Result<bool, String> {
        let mut claimed = self
            .claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut persisted = false;
        while let Some(index) = claimed
            .iter()
            .position(|item| item.ready == Some(before_response))
        {
            let item = &claimed[index];
            self.instance.persist_pending_input(
                self.app,
                self.store,
                self.run_id,
                self.env_prefix,
                item.input.clone(),
                Some(item.suffix.clone()),
                item.claude_receipt.as_ref(),
            )?;
            self.queue
                .lock()
                .map_err(|e| e.to_string())?
                .finish_steering(&item.input.id);
            claimed.remove(index);
            persisted = true;
        }
        Ok(persisted)
    }

    pub fn has_ready(&self, before_response: bool) -> bool {
        self.claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|item| item.ready == Some(before_response))
    }

    fn publish(&self, changes: crate::session::pending_inputs::PendingInputChanges) {
        if changes.is_empty() {
            return;
        }
        let Some(store) = self.app.try_state::<Arc<SessionStore>>() else {
            return;
        };
        crate::session::gateway::emit_pending_input_changes(
            self.app,
            store.inner().as_ref(),
            self.run_id,
            &self.instance.session_id,
            changes,
        );
    }

    /// Returns outstanding claims, newest first so restoring keeps their order.
    fn take_claims(&self, unresolved_only: bool) -> Vec<PendingSessionInput> {
        let mut claimed = self
            .claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut taken = Vec::new();
        claimed.retain(|item| {
            if unresolved_only && item.ready.is_some() {
                return true;
            }
            taken.push(item.input.clone());
            false
        });
        taken.reverse();
        taken
    }

    fn restore_claims(&self, inputs: Vec<PendingSessionInput>) {
        if inputs.is_empty() {
            return;
        }
        let changes = match self.queue.lock() {
            Ok(mut queue) => queue.restore_steering(inputs),
            Err(error) => {
                eprintln!("[Agent {}] failed to restore steering input: {error}", self.instance.id);
                return;
            }
        };
        self.publish(changes);
    }
}

impl RunSteering<'_> {
    pub(super) fn has_immediate_claude_input(&self) -> bool {
        self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .list_session(&self.instance.session_id).iter().any(|input|
                input.run_id == self.run_id && input.delivery == crate::session::pending_inputs::DELIVERY_IMMEDIATE)
    }

    /// The retry owns the same claimed inputs as the interrupted connection.
    /// A verified native token is an ACK even if input_consumed never arrived.
    /// Never restore/merge it into a newer draft and send it a second time.
    pub(super) fn reconcile_claude_history(&self) -> Result<(), String> {
        let mut claimed = self.claimed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if claimed.is_empty() { return Ok(()); }
        let messages = self.store.get_messages(&self.instance.session_id)?;
        let metadata = self.store.get_response_request_metadata(&self.instance.session_id)?;
        let ids = claimed.iter().map(|item| item.input.id.as_str()).collect::<Vec<_>>();
        let receipts = recovered_claude_input_receipts(&ids, &messages, &metadata)?;
        let mut queue = self.queue.lock().map_err(|e| e.to_string())?;
        for (pending, _) in &receipts { queue.finish_steering(pending); }
        claimed.clear();
        drop(queue);
        drop(claimed);
        for (pending, message) in receipts {
            emit_stream(self.app, self.run_id, StreamEvent::PendingInputAccepted {
                session_id: self.instance.session_id.clone(), pending_input_id: pending, message_id: message,
            });
        }
        Ok(())
    }

    pub(super) fn committed_claude(&self, id: &str, checkpoint: &serde_json::Value) -> Result<(), String> {
        crate::llm::claude_code_cli::recovery::validate_input_checkpoint(checkpoint)?;
        let mut claimed = self.claimed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(item) = claimed.iter_mut().find(|item| item.input.id == id) {
            item.claude_receipt = Some(checkpoint.clone());
            item.ready = Some(true);
        }
        drop(claimed);
        self.persist_ready(true).map(|_| ())
    }
    pub(super) fn claim_for_claude(&self) -> Result<Option<SteeringInput>, String> {
        self.claim_with_runtime_updates(true)
    }
    fn claim_with_runtime_updates(&self, update_runtime: bool) -> Result<Option<SteeringInput>, String> {
        let input = {
            let mut queue = self.queue.lock().map_err(|e| e.to_string())?;
            // Switching into Plan changes tool permissions and request settings.
            // It must use the ordinary boundary where those changes are enforced.
            let changes_mode = queue
                .list_session(&self.instance.session_id)
                .iter()
                .any(|input| {
                    input.run_id == self.run_id
                        && AgentInstance::pending_input_mode(input) == "plan"
                        && self.instance.plan_runtime_snapshot().is_none()
                });
            if changes_mode && !update_runtime {
                return Ok(None);
            }
            // Codex replays local history after any failed attempt, so an input
            // that was never saved there is provably undelivered. Claude's
            // native session may still hold it, which settlement must respect.
            let ambiguous = matches!(self.instance.backend, LlmBackend::ClaudeCodeCli);
            queue.claim_for_steering(&self.instance.session_id, self.run_id, ambiguous)
        };
        let Some(input) = input else {
            return Ok(None);
        };
        if update_runtime {
            let setup = self.instance.maybe_enter_plan_mode(self.app,self.store,self.run_id,&AgentInstance::pending_input_mode(&input));
            if let Err(error)=setup {
                self.queue.lock().map_err(|e|e.to_string())?.restore_claimed(vec![input]);
                return Err(error);
            }
            let names=self.instance.selected_skill_tool_names(input.user_intent.as_ref());
            self.instance.activate_document_skill_tool_names(&names.into_iter().collect::<Vec<_>>());
            for package in self.instance.selected_skill_package_ids(input.user_intent.as_ref()) {
                self.instance.activate_skill_package_runtime(&package);
            }
        }
        let suffix = self.instance.build_user_prompt_suffix(
            self.app,
            self.store,
            input.user_intent.as_ref(),
            &input.text,
        );
        let suffix = match self.store.prepare_capture_prompt_suffix(
            &self.instance.session_id,
            input.images.as_deref(),
            suffix.as_deref(),
        ) {
            Ok(suffix) => suffix,
            Err(error) => {
                self.queue
                    .lock()
                    .map_err(|e| e.to_string())?
                    .restore_claimed(vec![input]);
                return Err(error);
            }
        };
        let wire = codex::steering::user_input(
            &format!("{}{}", input.text, suffix.as_deref().unwrap_or_default()),
            input.images.as_deref(),
        );
        let result = SteeringInput {
            id: input.id.clone(),
            input: wire,
        };
        emit_stream(
            self.app,
            self.run_id,
            StreamEvent::PendingInputQueued {
                session_id: self.instance.session_id.clone(),
                input: input.clone(),
            },
        );
        self.claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ClaimedInput {
                input,
                suffix,
                ready: None,
                claude_receipt: None,
            });
        Ok(Some(result))
    }
}

fn recovered_claude_input_receipts(
    ids: &[&str], messages: &[ChatMessage], metadata: &HashMap<String, serde_json::Value>,
) -> Result<Vec<(String, String)>, String> {
    use crate::llm::claude_code_cli::recovery;
    ids.iter().map(|id| {
        let matches = messages.iter().filter(|message| message.role == MessageRole::User
            && metadata.get(&message.id).is_some_and(|value|
                value["claude_native"]["version"] == 1
                && recovery::pending_input_token(&recovery::text_content(&value["claude_native"]["content"])) == Some(*id)))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err("Claude could not verify whether an inserted input was consumed before disconnection. The input was retained; automatic recovery stopped without resending it. Review the native conversation before sending it again.".into());
        }
        Ok((id.to_string(), matches[0].id.clone()))
    }).collect()
}

#[cfg(test)]
mod claude_retry_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_recovery_acknowledges_only_unique_native_input_tokens() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let session = store.create_session("steering recovery", None, None, "chat", None).unwrap();
        let first = store.add_message(&session, MessageRole::User, "consumed instruction").unwrap();
        let token = uuid::Uuid::new_v4().to_string();
        let mut metadata = HashMap::from([(first.clone(), json!({"claude_native":{"version":1,
            "content":crate::llm::claude_code_cli::recovery::pending_input_text(&token, "consumed instruction")}}))]);
        let messages = store.get_messages(&session).unwrap();
        assert_eq!(recovered_claude_input_receipts(&[&token], &messages, &metadata).unwrap(), vec![(token.clone(), first.clone())]);
        // Matching visible text is not proof; an unrelated, newer queued draft
        // must remain pending rather than being silently dropped with this ACK.
        assert!(recovered_claude_input_receipts(&["unknown"], &messages, &metadata).unwrap_err().contains("without resending"));
        assert!(recovered_claude_input_receipts(&[&token, "newer-draft"], &messages, &metadata).is_err());
        let duplicate = store.add_message(&session, MessageRole::User, "duplicate native identity").unwrap();
        metadata.insert(duplicate, metadata[&first].clone());
        assert!(recovered_claude_input_receipts(&[&token], &store.get_messages(&session).unwrap(), &metadata).is_err());
    }
}

impl SteeringSource for RunSteering<'_> {
    fn claim(&self) -> Result<Option<SteeringInput>, String> {
        self.claim_with_runtime_updates(false)
    }

    fn committed(&self, id: &str, before_response: bool) -> Result<(), String> {
        if let Some(item) = self
            .claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
            .find(|item| item.input.id == id)
        {
            item.ready = Some(before_response);
        }
        // An explicit tool continuation starts after its parent and tool results
        // are already saved. Commit before emitting any successor text deltas.
        if before_response {
            self.persist_ready(true)?;
        }
        Ok(())
    }

    fn rejected(&self, id: &str) {
        let _ = self.committed(id, false);
    }

    /// Committed or rejected claims already have a defined place in history;
    /// only a claim still awaiting a receipt or successor is unresolved.
    fn has_unresolved_input(&self) -> bool {
        self.claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|item| item.ready.is_none())
    }

    fn release_unresolved(&self) {
        self.restore_claims(self.take_claims(true));
    }
}

impl Drop for RunSteering<'_> {
    fn drop(&mut self) {
        // Terminal run events settle claims (next turn or retained), so this
        // only returns claims abandoned before a failure is reported. A
        // cancelled run's claim stays in flight for the cancel settlement to
        // retain; making it queued here would let clients resend it.
        let claims = self.take_claims(false);
        if !self.instance.is_cancel_requested() {
            self.restore_claims(claims);
        }
    }
}
