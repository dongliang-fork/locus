//! Provider-native web commands use the ordinary tool-result persistence path.
use super::*;

#[cfg(test)]
#[path = "codex_web_tests.rs"]
mod tests;

impl AgentInstance {
    pub(super) fn with_codex_web_tool(
        &self,
        mut tools: PreparedRequestTools,
    ) -> PreparedRequestTools {
        if matches!(self.backend, LlmBackend::OpenAiCodex { .. }) {
            tools.api_tools.push(codex::web::tool_spec());
            tools.request_tool_names.push(codex::web::TOOL_NAME.into());
        }
        tools
    }

    pub(super) async fn execute_codex_web(
        &self,
        store: &SessionStore,
        args: &serde_json::Value,
    ) -> ExecutedToolResult {
        let operation = async {
            let LlmBackend::OpenAiCodex { auth, base_url, .. } = &self.backend else {
                return Err("web.run is available only with Codex OAuth.".to_string());
            };
            let history = store.get_messages_for_prompt(&self.session_id)?;
            let body =
                codex::web::request_body(&self.session_id, &self.effective_model, &history, args)?;
            codex::web::execute(base_url.as_deref(), &body, |refresh| {
                resolve_codex_request_auth(auth, refresh)
            })
            .await
        };
        let mut cancel = self.cancel_waiter();
        tokio::select! {
            biased;
            _ = async {
                if cancel.wait_for(|cancel| *cancel).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => ExecutedToolResult::from_tool_result(ToolResult {
                output: crate::session::history::INTERRUPTED_TOOL_RESULT.into(), is_error: false,
            }),
            result = operation => ExecutedToolResult::from_tool_result(match result {
                Ok(output) => ToolResult { output, is_error: false },
                Err(output) => ToolResult { output, is_error: true },
            }),
        }
    }
}
