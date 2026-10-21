//! Opt-in P0: official agy -> local runner -> the real Locus read tool.
//! The model is a deterministic loopback fixture, not subscription inference.
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
#[ignore = "requires bun and ref/antigravity-cli-1.4.2/agy.exe; run alone"]
async fn real_agy_local_read_roundtrip() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut command = tokio::process::Command::new("bun");
    command
        .arg(repo.join("scripts/antigravity-cli-probe.mjs"))
        .arg("--host-stdio")
        .current_dir(&repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().expect("start Antigravity probe with bun");
    let pid = child.id().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut registry = crate::tool::ToolRegistry::new();
    crate::tool::builtins::register_all(&mut registry);
    let run = tokio::time::timeout(Duration::from_secs(120), async {
        let mut root = None;
        let mut count = 0;
        let mut report = None;
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            let event: Value =
                serde_json::from_str(&line).map_err(|e| format!("invalid probe event: {e}"))?;
            match event["event"].as_str() {
                Some("process") => println!("AGY_PROBE_PROCESS {line}"),
                Some("started") => {
                    println!("AGY_PROBE_STARTED {line}");
                    root = event["root"].as_str().map(PathBuf::from);
                }
                Some("tool_request") => {
                    let root = root.as_ref().ok_or("tool arrived before probe identity")?;
                    let workspace = root.join("workspace");
                    let proof = workspace.join("proof.txt");
                    if event["tool"] != "read"
                        || event["workspace"].as_str().map(PathBuf::from).as_ref()
                            != Some(&workspace)
                        || event["arguments"]["filePath"]
                            .as_str()
                            .map(PathBuf::from)
                            .as_ref()
                            != Some(&proof)
                        || count != 0
                    {
                        return Err("unexpected or duplicate probe tool request".to_string());
                    }
                    count += 1;
                    let result = registry
                        .execute_with_context(
                            "read",
                            &event["arguments"],
                            crate::tool::ToolExecutionContext {
                                working_dir: Some(workspace.to_string_lossy().into_owned()),
                                ..Default::default()
                            },
                        )
                        .await;
                    let reply = json!({"event":"tool_result", "id":event["id"],
                        "result":{"output":result.output, "isError":result.is_error}});
                    stdin
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .map_err(|e| e.to_string())?;
                    stdin.flush().await.map_err(|e| e.to_string())?;
                }
                Some("finished") => {
                    println!("AGY_PROBE_FINISHED {line}");
                    report = Some(event);
                    break;
                }
                _ => return Err(format!("unknown probe event: {line}")),
            }
        }
        drop(stdin);
        let status = child.wait().await.map_err(|e| e.to_string())?;
        let report = report.ok_or("missing probe report")?;
        if !status.success() || report["ok"] != true {
            return Err(format!("probe failed ({status}): {report}"));
        }
        if count != 1
            || report["executionCount"] != 1
            || report["turns"] != 2
            || report["executor"] != "locus-rust-host"
            || report["mcpConfigured"] != false
        {
            return Err(format!("incomplete real Locus evidence: {report}"));
        }
        Ok::<_, String>(())
    })
    .await;
    // Only this test's bun process and its descendants; never shared dev instances.
    if child.try_wait().ok().flatten().is_none() {
        #[cfg(windows)]
        {
            let mut kill = std::process::Command::new("taskkill");
            use std::os::windows::process::CommandExt;
            kill.creation_flags(0x08000000)
                .args(["/PID", &pid.to_string(), "/T", "/F"]);
            let _ = kill.output();
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    run.expect("Antigravity P0 exceeded 120 seconds")
        .expect("Antigravity P0 failed");
}
