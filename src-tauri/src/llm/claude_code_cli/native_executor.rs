//! Called by ToolRegistry only after the Agent's permission/path/plan/undo
//! checks. All nested operations retain the same checkout execution context.
use super::native_scope::NativeInvocation;
use crate::tool::{ToolExecutionContext, ToolRegistry, ToolResult};
use serde_json::{json, Value};

pub(crate) async fn execute(
    registry: &ToolRegistry,
    ctx: ToolExecutionContext,
    scope: NativeInvocation,
) -> ToolResult {
    let result = match scope.call.tool.as_str() {
        "Write" => return write(registry, &ctx, &scope).await.unwrap_or_else(|output| ToolResult { output, is_error: true }),
        "Glob" | "Grep" => search(&ctx, &scope).await,
        "TaskOutput" | "TaskStop" => task(&ctx, &scope).await,
        _ => {
            return ToolResult {
                output: "Unsupported native adapter".into(),
                is_error: true,
            }
        }
    };
    match result {
        Ok(value) => {
            let output = super::native_output::render(&scope.call.tool, &value);
            scope.set_result(value);
            ToolResult { output, is_error: false }
        }
        Err(output) => ToolResult {
            output,
            is_error: true,
        },
    }
}

async fn write(
    registry: &ToolRegistry,
    ctx: &ToolExecutionContext,
    scope: &NativeInvocation,
) -> Result<ToolResult, String> {
    let input = &scope.call.input;
    let path = ctx.resolve_path(
        input["file_path"]
            .as_str()
            .ok_or("Write requires file_path")?,
    )?;
    let content = input["content"].as_str().ok_or("Write requires content")?;
    let original = match tokio::fs::read_to_string(&path).await {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    // Existing files go through Locus's atomic, conflict-checked edit handler.
    // Its explicit native replacement entry handles empty files as well.
    let result = if let Some(original) = &original {
        NativeInvocation::without(Box::pin(registry.execute_with_context(
            "edit",
            &json!({"filePath":path,
            "edits":[{"oldString":original,"newString":content,"replaceAll":false}]}),
            ctx.clone(),
        )))
        .await
    } else {
        NativeInvocation::without(Box::pin(registry.execute_with_context(
            "write",
            &json!({"filePath":path,"content":content}),
            ctx.clone(),
        )))
        .await
    };
    if result.is_error {
        return Err(result.output);
    }
    let actual = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| e.to_string())?;
    scope.set_result(
        json!({"type":if original.is_some(){"update"}else{"create"},"filePath":path,
        "content":actual,"originalFile":original,"structuredPatch":[],"userModified":false}),
    );
    // Preserve the actual write/edit status, including generated frontmatter,
    // knowledge sync warnings and Unity/import diagnostics.
    Ok(result)
}

async fn search(ctx: &ToolExecutionContext, scope: &NativeInvocation) -> Result<Value, String> {
    let input = scope.call.input.clone();
    let root = ctx.resolve_path(input.get("path").and_then(Value::as_str).unwrap_or("."))?;
    let is_glob = scope.call.tool == "Glob";
    tokio::task::spawn_blocking(move || {
        if is_glob { glob_sync(&root, &input) }
        else { super::native_grep::run(&root, &input) }
    })
        .await
        .map_err(|e| e.to_string())?
}

fn glob_sync(root: &str, input: &Value) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let pattern = input["pattern"].as_str().ok_or("Search requires pattern")?;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_exclude(true);
    {
        let glob = pattern;
        let base = if std::path::Path::new(root).is_file() {
            std::path::Path::new(root).parent().unwrap()
        } else {
            std::path::Path::new(root)
        };
        let mut overrides = ignore::overrides::OverrideBuilder::new(base);
        overrides.add(glob).map_err(|e| e.to_string())?;
        builder.overrides(overrides.build().map_err(|e| e.to_string())?);
    }
    if let Some(kind) = input["type"].as_str() {
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults();
        types.select(kind);
        builder.types(types.build().map_err(|e| e.to_string())?);
    }
    let mut paths = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            paths.push(entry.into_path());
        }
        if paths.len() > 100000 {
            return Err("Search exceeded 100000 files; narrow the path or glob".into());
        }
    }
    paths.sort();
    let total = paths.len();
    let filenames: Vec<_> = paths.into_iter().take(100)
        .map(|path| path.to_string_lossy().into_owned()).collect();
    Ok(json!({"durationMs":started.elapsed().as_millis(),"numFiles":filenames.len(),"filenames":filenames,
        "truncated":total>100,"totalMatches":total,"countIsComplete":true}))
}

async fn task(ctx: &ToolExecutionContext, scope: &NativeInvocation) -> Result<Value, String> {
    use tauri::Manager;
    let app = ctx
        .app_handle
        .as_ref()
        .ok_or("Background task control requires a Locus runtime")?;
    let manager = app.state::<std::sync::Arc<crate::async_tasks::AsyncTaskManager>>();
    let requested = scope.call.input["task_id"]
        .as_str()
        .ok_or("task_id is required")?;
    let id = requested.strip_prefix("locus:").unwrap_or(requested);
    let current = manager.get_session_task(&scope.session_id, id)?;
    if scope.call.tool == "TaskStop" {
        manager.cancel(&current.task_id)?;
        return Ok(
            json!({"message":"Cancellation requested","task_id":requested,"task_type":"local_bash"}),
        );
    }
    let block = scope.call.input["block"].as_bool().unwrap_or(true);
    let timeout = scope.call.input["timeout"]
        .as_u64()
        .unwrap_or(30000)
        .min(600000);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
    let mut snapshot = current;
    while block && !snapshot.status.is_terminal() && tokio::time::Instant::now() < deadline {
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis() as u64;
        snapshot = manager.wait_task(&scope.session_id, id, remaining).await?;
    }
    let output = snapshot.output.clone().unwrap_or_default();
    let exit_code = output
        .strip_prefix("Exit code: ")
        .and_then(|s| s.lines().next())
        .and_then(|s| s.parse::<i32>().ok());
    Ok(
        json!({"retrieval_status":if snapshot.status.is_terminal(){"success"}else if block{"timeout"}else{"not_ready"},
        "task":{"task_id":requested,"task_type":"local_bash","status":snapshot.status,"description":snapshot.description,
        "output":output,"exitCode":exit_code}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn run(root: &std::path::Path, tool: &str, input: Value) -> (ToolResult, Value) {
        let call = super::super::native_tools::adapt("native_test", tool, &input).unwrap();
        let scope = NativeInvocation::new(call.clone(), "test_session".into());
        let registry = ToolRegistry::with_builtins();
        let identity = crate::workspace_service::identity::ProjectIdResolver::resolve(root).unwrap();
        let runtime = crate::workspace_service::WorkspaceRuntime::new(identity, Vec::new(), 1);
        let execution = std::sync::Arc::new(crate::workspace_service::AgentExecutionContext::new(runtime, std::collections::HashMap::new()));
        let result = scope
            .run(registry.execute_with_context(
                call.locus_name,
                &call.locus_input,
                ToolExecutionContext {
                    execution: Some(execution),
                    working_dir: Some(root.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            ))
            .await;
        (result, scope.result().unwrap_or(Value::Null))
    }
    #[tokio::test]
    async fn write_creates_overwrites_and_handles_empty_existing_files() {
        let root = tempfile::tempdir().unwrap();
        for original in [None, Some("old\n"), Some("")] {
            let file = root
                .path()
                .join(format!("file-{}.txt", original.map(str::len).unwrap_or(99)));
            if let Some(text) = original {
                std::fs::write(&file, text).unwrap();
            }
            let (result, raw) = run(
                root.path(),
                "Write",
                json!({"file_path":file,"content":"new\n"}),
            )
            .await;
            assert!(!result.is_error, "{}", result.output);
            assert!(!result.output.starts_with('{'), "{}", result.output);
            assert!(!result.output.contains("new\\n"));
            assert_eq!(raw["content"], "new\n");
            assert_eq!(raw["originalFile"], json!(original));
            assert_eq!(
                std::fs::read_to_string(file).unwrap().replace("\r\n", "\n"),
                "new\n"
            );
            assert_eq!(
                raw["type"],
                if original.is_some() {
                    "update"
                } else {
                    "create"
                }
            );
        }
    }
    #[tokio::test]
    async fn native_knowledge_write_retains_generated_metadata_and_sync_warning() {
        let root = tempfile::tempdir().unwrap();
        let input = json!({"file_path":"Locus/knowledge/plan/native.md","content":"# Plan\n"});
        let (result, raw) = run(root.path(), "Write", input.clone()).await;
        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains("Generated frontmatter:"));
        assert!(result.output.contains("contentStartLine:"));
        assert!(result.output.contains("Knowledge index: Locus application context required"));
        assert!(raw["content"].as_str().unwrap().starts_with("---\n"));
        let call = super::super::native_tools::adapt("native_test", "Write", &input).unwrap();
        let reply = super::super::native_tools::finish_result(&call, &result.into(), Some(raw));
        assert!(reply["context"][0].as_str().unwrap().contains("Knowledge index:"));
    }

    #[tokio::test]
    async fn search_metadata_tracks_the_returned_page() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..5 { std::fs::write(root.path().join(format!("{index}.txt")), "match\n").unwrap(); }
        for mode in ["content", "count", "files_with_matches"] {
            let (result, raw) = run(root.path(), "Grep", json!({"pattern":"match","output_mode":mode,"head_limit":1,"offset":2})).await;
            assert!(!result.is_error, "{}", result.output);
            assert!(result.output.starts_with("Found 5 matches in 5 files"));
            assert_eq!(raw["totalFiles"], 5);
            assert_eq!(raw["numFiles"], 1);
            assert_eq!(raw["filenames"].as_array().unwrap().len(), 1);
            assert!(raw["filenames"][0].as_str().unwrap().ends_with("2.txt"));
        }
    }

    #[tokio::test]
    async fn native_search_supports_globs_case_context_count_and_pagination() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(
            root.path().join("sub/a.cs"),
            "first\nNeedle\nlast\nneedle\n",
        )
        .unwrap();
        std::fs::write(root.path().join("b.txt"), "needle\n").unwrap();
        let (_, glob) = run(root.path(), "Glob", json!({"pattern":"**/*.cs"})).await;
        assert_eq!(glob["numFiles"], 1);
        let (_, grep) = run(
            root.path(),
            "Grep",
            json!({"pattern":"needle","-i":true,"glob":"*.cs","output_mode":"content","-C":1}),
        )
        .await;
        assert_eq!(grep["numMatches"], 2);
        assert!(grep["content"].as_str().unwrap().contains("first"));
        let (_, count) = run(
            root.path(),
            "Grep",
            json!({"pattern":"needle","output_mode":"count","head_limit":1}),
        )
        .await;
        assert_eq!(count["numFiles"], 1);
        assert_eq!(count["totalFiles"], 2);
        assert_eq!(count["numLines"], 1);
        let (bad, _) = run(root.path(), "Grep", json!({"pattern":"["})).await;
        assert!(bad.is_error);
    }
}
