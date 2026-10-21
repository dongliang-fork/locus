//! Unity-authored merge matrix: Prefab hierarchy, nested and variant overrides,
//! SerializeReference graphs, identity-less lists and scene roots. Unity writes
//! every base/target/source history (`LocusMergeMatrixApi`); each case merges
//! through the same job actions the SDK exposes and is judged by a real Unity
//! reimport. Cases run independently and report together, with the agent-side
//! call count, response size and timings used to judge efficiency.
use super::worktree_merge::{apply_plan, capture_json, git, patch_commit};
use super::*;
use crate::merge_jobs::{self, PrepareRequest, SourceSelection};

/// What the agent does after `include(path)`; the expected Unity facts follow.
#[derive(Clone, Copy)]
enum Plan {
    /// Every change merges; the plan must be ready immediately.
    Clean,
    /// One conflict whose path contains the text; resolved with `side`.
    ResolveOne(&'static str, &'static str),
    /// Every conflict resolved with `side`; at least one must exist.
    ResolveAll(&'static str),
    /// A list without element identity both sides appended to: set the union.
    UnionList(&'static str),
    /// A managed reference the destination deleted: restore it and its owner.
    RestoreReference(&'static str),
    /// A list without identity: keep the destination list, take one source element field.
    RefineList(&'static str, &'static str),
}

struct Case {
    name: &'static str,
    plan: Plan,
    expect: &'static [(&'static str, &'static str)],
    /// The Editor has the asset open (scene or Prefab Mode) during the merge.
    editor_open: Option<Write>,
}

/// How an apply reaches disk while the Editor has the asset open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    /// The plan also carries a text file: Rust writes, then the Editor
    /// reloads the open copy (`editor_sync`).
    Direct,
    /// An asset-only plan: the Editor's own disk transaction writes and reloads.
    Transaction,
}

const fn case(name: &'static str, plan: Plan, expect: &'static [(&'static str, &'static str)]) -> Case {
    Case { name, plan, expect, editor_open: None }
}

/// The open copy shows both sides' position edits and holds no unsaved state.
const OPEN: &[(&str, &str)] = &[("x", "1"), ("y", "2"), ("dirty", "false")];

const CASES: &[Case] = &[
    case("prefab_children", Plan::Clean,
        &[("children", "A|SourceChild|TargetChild"), ("a.x", "1"), ("source.y", "2")]),
    case("prefab_components", Plan::Clean,
        &[("components", "BoxCollider|LocusMergeFixtureComponent|Transform"), ("box.size.x", "2"), ("behavior.label", "added"), ("missingTypes", "false")]),
    case("prefab_nested_overrides", Plan::Clean,
        &[("x", "3"), ("y", "4"), ("health", "7"), ("speed", "9"), ("instance", "true")]),
    case("prefab_nested_conflict", Plan::ResolveOne("m_LocalPosition.x", "source"),
        &[("x", "5"), ("y", "4"), ("instance", "true")]),
    case("prefab_nested_added", Plan::Clean,
        &[("components", "AudioSource|BoxCollider|LocusMergeFixtureComponent|Transform"), ("children", "Leaf|SourceLeaf|TargetLeaf"), ("instance", "true")]),
    case("prefab_variant", Plan::Clean,
        &[("variant", "true"), ("health", "7"), ("collider", "false"), ("children", "Leaf|SourceChild|TargetChild"), ("leaf.z", "2")]),
    case("prefab_delete_modify", Plan::ResolveAll("target"),
        &[("children", "A")]),
    case("sr_list_append", Plan::UnionList("/data/children"),
        &[("labels", "n1|n2|t-new|s-new"), ("types", "MergeAction|MergeAction|MergeAction|MergeWeightedAction"), ("missingTypes", "false")]),
    case("sr_delete_modify", Plan::RestoreReference("/MonoBehaviour/optional"),
        &[("optional", "MergeAction"), ("health", "6"), ("missingTypes", "false")]),
    case("sr_type_replace", Plan::ResolveAll("source"),
        &[("optional", "MergeWeightedAction"), ("label", "weighted"), ("missingTypes", "false")]),
    case("sr_prefab_fields", Plan::Clean,
        &[("label", "source"), ("health", "7"), ("speed", "9")]),
    case("sr_retarget", Plan::Clean,
        &[("alias.label", "other2"), ("alias.isSecond", "true"), ("shared.health", "9")]),
    case("rows_elements", Plan::RefineList("/MonoBehaviour/rows", "/MonoBehaviour/rows/1/position"),
        &[("rows", "alpha:5:1,2,3|beta:2:4,9,6")]),
    case("scene_roots", Plan::Clean,
        &[("roots", "A|Inner|SourceRoot|TargetRoot"), ("a.y", "2"), ("x", "3"), ("y", "4")]),
    case("scene_same_id", Plan::ResolveAll("target"), &[("roots", "A|TargetRoot")]),
    Case { name: "scene_open_reload", plan: Plan::Clean, expect: OPEN, editor_open: Some(Write::Direct) },
    Case { name: "scene_open_transaction", plan: Plan::Clean, expect: OPEN, editor_open: Some(Write::Transaction) },
    Case { name: "prefab_stage_reload", plan: Plan::Clean, expect: OPEN, editor_open: Some(Write::Direct) },
];

/// The agent's view of one job: every call is counted with its latency and
/// response size, exactly as an SDK client would observe them.
struct Agent<'a> {
    root: &'a Path,
    job: String,
    path: String,
    calls: Vec<Value>,
}

impl Agent<'_> {
    fn call(&mut self, action: &str, params: Value) -> Result<Value, String> {
        let started = Instant::now();
        let result = merge_jobs::execute(self.root, &self.job, action, params);
        let bytes = result.as_ref().map(|v| v.to_string().len()).unwrap_or(0);
        self.calls.push(json!({"action":action,"ms":started.elapsed().as_millis(),"bytes":bytes,"ok":result.is_ok()}));
        result.map_err(|error| format!("{action}: {error}"))
    }

    fn changes(&mut self) -> Result<Vec<Value>, String> {
        let page = self.call("changes", json!({"limit":1000}))?;
        Ok(page["changes"]
            .as_array()
            .ok_or("changes omitted the catalog page")?
            .iter()
            .filter(|c| c["path"].as_str() == Some(self.path.as_str()))
            .cloned()
            .collect())
    }

    fn preview(&mut self) -> Result<Value, String> {
        self.call("preview", json!({}))
    }

    /// Managed-reference ids of a sequence of `{rid: N}` elements in one version.
    fn rids(&mut self, version: &str, object_id: &str, property_path: &str) -> Result<Vec<i64>, String> {
        let page = self.call(
            "inspect_asset",
            json!({"path":self.path,"version":version,"object_id":object_id,"property_path":property_path,"limit":1000}),
        )?;
        let fields = page["fields"].as_array().ok_or("inspect_asset omitted fields")?;
        fields
            .iter()
            .filter(|f| f["property_path"].as_str().is_some_and(|p| p.starts_with(property_path) && p.ends_with("/rid")))
            .map(|f| {
                f["scalar_text"]
                    .as_str()
                    .and_then(|t| t.trim().parse::<i64>().ok())
                    .ok_or_else(|| format!("unreadable rid field {f}"))
            })
            .collect()
    }
}

fn conflicts(changes: &[Value]) -> Vec<&Value> {
    changes.iter().filter(|c| c["status"] == "conflict").collect()
}

fn ids(changes: &[&Value]) -> Vec<Value> {
    changes.iter().map(|c| c["id"].clone()).collect()
}

/// Agent-side steps of one plan. Returns a short description of the decisions.
fn resolve(agent: &mut Agent<'_>, plan: Plan, changes: &[Value]) -> Result<String, String> {
    let open = conflicts(changes);
    if let Some(missing) = open.iter().find(|c| !c["reason"].as_str().is_some_and(|r| !r.is_empty())) {
        return Err(format!("conflict without an actionable reason: {missing}"));
    }
    let describe = |c: &&Value| format!("{} ({})", c["property_path"].as_str().unwrap_or(""), c["reason"].as_str().unwrap_or(""));
    match plan {
        Plan::Clean => {
            if !open.is_empty() {
                return Err(format!(
                    "expected an automatic merge, got {} conflict(s): {}",
                    open.len(),
                    open.iter().map(describe).collect::<Vec<_>>().join("; ")
                ));
            }
            Ok("include".into())
        }
        Plan::ResolveOne(needle, side) => {
            let [only] = open.as_slice() else {
                return Err(format!("expected exactly one conflict, got {}: {}", open.len(), open.iter().map(describe).collect::<Vec<_>>().join("; ")));
            };
            if !only["property_path"].as_str().is_some_and(|p| p.contains(needle)) {
                return Err(format!("conflict landed on the wrong element: {}", describe(only)));
            }
            agent.call("resolve", json!({"change_ids":[only["id"]],"side":side}))?;
            Ok(format!("resolve 1 {side}"))
        }
        Plan::ResolveAll(side) => {
            if open.is_empty() {
                return Err("expected conflicts requiring a decision, got none".into());
            }
            agent.call("resolve", json!({"change_ids":ids(&open),"side":side}))?;
            Ok(format!("resolve {} {side}: {}", open.len(), open.iter().map(describe).collect::<Vec<_>>().join("; ")))
        }
        Plan::UnionList(suffix) => {
            let list = open
                .iter()
                .find(|c| c["property_path"].as_str().is_some_and(|p| p.ends_with(suffix)))
                .ok_or_else(|| format!("no conflict on the appended list; conflicts: {}", open.iter().map(describe).collect::<Vec<_>>().join("; ")))?;
            if open.len() != 1 {
                return Err(format!("only the list order should need a decision: {}", open.iter().map(describe).collect::<Vec<_>>().join("; ")));
            }
            let object = list["object_id"].as_str().unwrap_or_default().to_string();
            let path = list["property_path"].as_str().unwrap_or_default().to_string();
            let mut union = agent.rids("target", &object, &path)?;
            for rid in agent.rids("source", &object, &path)? {
                if !union.contains(&rid) {
                    union.push(rid);
                }
            }
            let value: Vec<Value> = union.iter().map(|rid| json!({"rid":rid})).collect();
            agent.call("fields.set", json!({"path":agent.path,"object_id":object,"property_path":path,"value":value}))?;
            Ok(format!("fields.set union of {} references", union.len()))
        }
        Plan::RestoreReference(owner) => {
            let [deleted] = open.as_slice() else {
                return Err(format!("expected one deleted-reference conflict, got {}: {}", open.len(), open.iter().map(describe).collect::<Vec<_>>().join("; ")));
            };
            if !deleted["property_path"].as_str().is_some_and(|p| p.contains("/RefIds/@rid="))
                || !deleted["reason"].as_str().is_some_and(|r| r.contains("deleted"))
            {
                return Err(format!("unexpected conflict shape: {}", describe(deleted)));
            }
            agent.call("resolve", json!({"change_ids":[deleted["id"]],"side":"source"}))?;
            let object = deleted["object_id"].clone();
            agent.call("fields.take", json!({"path":agent.path,"object_id":object,"property_path":owner,"side":"source"}))?;
            Ok("resolve source + fields.take owner".into())
        }
        Plan::RefineList(list, element) => {
            let [only] = open.as_slice() else {
                return Err(format!("expected one list conflict, got {}: {}", open.len(), open.iter().map(describe).collect::<Vec<_>>().join("; ")));
            };
            if only["property_path"].as_str() != Some(list) {
                return Err(format!("unexpected conflict: {}", describe(only)));
            }
            agent.call("resolve", json!({"change_ids":[only["id"]],"side":"target"}))?;
            let object = only["object_id"].clone();
            agent.call("fields.take", json!({"path":agent.path,"object_id":object,"property_path":element,"side":"source"}))?;
            Ok("resolve target + fields.take element".into())
        }
    }
}

async fn run_case(
    project: &str,
    folder: &str,
    snapshots: &Path,
    head: &str,
    case: &Case,
    file: &str,
) -> Result<Value, String> {
    let root = Path::new(project);
    let read = |side: &str| {
        std::fs::read(snapshots.join(format!("{}-{side}.yaml", case.name))).map_err(|e| e.to_string())
    };
    let (base, target, source) = (read("base")?, read("target")?, read("source")?);
    if base == target || base == source {
        return Err("Unity did not produce independent target/source edits".into());
    }
    let meta = std::fs::read(root.join(format!("{file}.meta"))).map_err(|e| e.to_string())?;
    // A companion text change keeps a Direct plan off the Editor's asset-only
    // transaction, so it exercises the post-apply reload instead.
    let notes = format!("{file}.merge-notes.txt");
    let mut base_files = vec![(file.to_string(), base), (format!("{file}.meta"), meta)];
    let mut source_files = vec![(file.to_string(), source)];
    let direct = case.editor_open == Some(Write::Direct);
    if direct {
        base_files.push((notes.clone(), b"base\n".to_vec()));
        source_files.push((notes.clone(), b"source\n".to_vec()));
    }
    let base_commit = patch_commit(root, head, &base_files, &format!("merge matrix base: {}", case.name))?;
    let source_commit = patch_commit(root, &base_commit, &source_files, &format!("merge matrix source: {}", case.name))?;
    std::fs::write(root.join(file), &target).map_err(|e| e.to_string())?;
    let mut paths = vec![file.to_string()];
    if direct {
        std::fs::write(root.join(&notes), b"base\n").map_err(|e| e.to_string())?;
        paths.push(notes.clone());
    }
    if case.editor_open.is_some() {
        execute_capture(
            project,
            &format!("print(Locus.MergeTesting.LocusMergeMatrixApi.Open({}, {}));", json!(folder), json!(case.name)),
        )
        .await?;
    }

    let started = Instant::now();
    let job = merge_jobs::prepare(
        root,
        &PrepareRequest {
            sources: vec![SourceSelection { commits: vec![source_commit], ..Default::default() }],
            paths: Some(paths.clone()),
            ..Default::default()
        },
    )?;
    let prepare_ms = started.elapsed().as_millis();
    let mut agent = Agent { root, job: job.id.clone(), path: file.to_string(), calls: Vec::new() };
    let changes = agent.changes()?;
    if changes.is_empty() {
        return Err("catalog has no changes for the case asset".into());
    }
    for path in &paths {
        agent.call("include", json!({"path":path}))?;
    }
    let first = agent.preview()?;
    let decisions = resolve(&mut agent, case.plan, &changes)?;
    let preview = agent.preview()?;
    if preview["ready_to_apply"] != true {
        return Err(format!("plan not ready after `{decisions}`: {}", preview["issues"]));
    }
    let started = Instant::now();
    let applied = apply_plan(
        root,
        &job.id,
        json!({"expected_plan_hash":preview["plan_hash"],"index_policy":"preserve"}),
    )
    .await?;
    let apply_ms = started.elapsed().as_millis();
    let reloaded = match case.editor_open {
        None => true,
        Some(Write::Direct) => applied["editor_sync"]["status"] == "synced" && applied["warnings"].is_null(),
        Some(Write::Transaction) => applied["editor_applied"] == true,
    };
    if !reloaded {
        return Err(format!(
            "the open asset was not written/reloaded as expected: editor_applied={}, editor_sync={}, warnings={}",
            applied["editor_applied"], applied["editor_sync"], applied["warnings"]
        ));
    }
    let inspected = capture_json(
        &execute_capture(
            project,
            &format!(
                "print(Locus.MergeTesting.LocusMergeMatrixApi.{}({}, {}));",
                if case.editor_open.is_some() { "InspectOpen" } else { "Inspect" },
                json!(folder),
                json!(case.name)
            ),
        )
        .await?,
    )?;
    let facts: BTreeMap<String, String> = inspected["facts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some((f["key"].as_str()?.to_string(), f["value"].as_str()?.to_string())))
        .collect();
    if let Some(error) = inspected["error"].as_str().filter(|e| !e.is_empty()) {
        return Err(format!("Unity could not load the merged asset: {error}"));
    }
    if let Some(logs) = inspected["logs"].as_array().filter(|l| !l.is_empty()) {
        return Err(format!("Unity reported import problems: {}", json!(logs)));
    }
    let mismatches: Vec<String> = case
        .expect
        .iter()
        .filter(|(key, value)| facts.get(*key).map(String::as_str) != Some(*value))
        .map(|(key, value)| format!("{key}: expected {value}, got {}", facts.get(*key).map(String::as_str).unwrap_or("<missing>")))
        .collect();
    if !mismatches.is_empty() {
        return Err(format!("Unity facts differ after `{decisions}`: {}", mismatches.join("; ")));
    }
    let statuses = changes.iter().fold(BTreeMap::<String, usize>::new(), |mut counts, c| {
        *counts.entry(c["status"].as_str().unwrap_or("?").to_string()).or_default() += 1;
        counts
    });
    let mutating = agent
        .calls
        .iter()
        .filter(|c| !matches!(c["action"].as_str(), Some("changes" | "preview" | "inspect_asset")))
        .count();
    Ok(json!({
        "changes": changes.len(),
        "statuses": statuses,
        "first_preview_issues": first["issues"].as_array().map(|i| i.len()).unwrap_or(0),
        "decisions": decisions,
        "agent_calls": agent.calls.len() + 2,
        "agent_mutations": mutating,
        "response_bytes": agent.calls.iter().filter_map(|c| c["bytes"].as_u64()).sum::<u64>(),
        "prepare_ms": prepare_ms,
        "apply_ms": apply_ms,
        "calls": agent.calls,
        "editor_sync": applied["editor_sync"]["status"],
        "editor_applied": applied["editor_applied"],
        "warnings": applied["warnings"],
        "facts": facts,
    }))
}

/// Runs every case and returns the report. A failing case never stops the rest.
pub(super) async fn run(project: &str, sink: &DriverEventSink) -> Result<Value, String> {
    let root = Path::new(project);
    let token = uuid::Uuid::new_v4().simple().to_string();
    let folder = format!("Assets/LocusMergeMatrix-{token}");
    let head = String::from_utf8(git(root, &["rev-parse", "HEAD"])?)
        .map_err(|e| e.to_string())?
        .trim()
        .to_string();
    let index_before = git(root, &["ls-files", "--stage", "-z"])?;
    let started = Instant::now();
    let manifest = capture_json(
        &execute_capture(
            project,
            &format!("print(Locus.MergeTesting.LocusMergeMatrixApi.Generate({}));", json!(folder)),
        )
        .await?,
    )?;
    let generate_ms = started.elapsed().as_millis();
    let snapshots = Path::new(manifest["snapshots"].as_str().ok_or("Matrix omitted snapshots")?).to_path_buf();
    let generated: BTreeMap<String, (String, Option<String>)> = manifest["cases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some((
                c["name"].as_str()?.to_string(),
                (c["path"].as_str()?.to_string(), c["error"].as_str().filter(|e| !e.is_empty()).map(str::to_string)),
            ))
        })
        .collect();
    sink.emit("merge_matrix_generated", json!({"folder":folder,"snapshots":snapshots,"elapsedMs":generate_ms,"cases":generated.len()}));
    let mut results = Vec::new();
    for case in CASES {
        let started = Instant::now();
        let outcome = match generated.get(case.name) {
            None => Err("Unity fixture did not generate this case".to_string()),
            Some((_, Some(error))) => Err(format!("Unity fixture generation failed: {error}")),
            Some((file, None)) => run_case(project, &folder, &snapshots, &head, case, file).await,
        };
        let elapsed = started.elapsed().as_millis();
        if case.editor_open.is_some() && outcome.is_err() {
            let close = format!("print(Locus.MergeTesting.LocusMergeMatrixApi.Close({}, {}));", json!(folder), json!(case.name));
            let _ = execute_capture(project, &close).await;
        }
        let line = match &outcome {
            Ok(_) => format!("PASS  merge-matrix: {}", case.name),
            Err(error) => format!("FAIL  merge-matrix: {}: {}", case.name, clip(error, 600)),
        };
        sink.emit("suite_event", json!({"suite":"asset-merge-matrix","line":line,"passed":outcome.is_ok() as u32,"failed":outcome.is_err() as u32}));
        results.push(match outcome {
            Ok(mut detail) => {
                detail["name"] = json!(case.name);
                detail["passed"] = json!(true);
                detail["elapsed_ms"] = json!(elapsed);
                detail
            }
            Err(error) => json!({"name":case.name,"passed":false,"error":error,"elapsed_ms":elapsed}),
        });
    }
    let index_preserved = git(root, &["ls-files", "--stage", "-z"])? == index_before;
    let failed = results.iter().filter(|r| r["passed"] != true).count();
    if failed == 0 {
        // Leave failing fixtures in place for diagnosis; passing ones are removed.
        execute_capture(project, &format!("print(Locus.MergeTesting.LocusMergeMatrixApi.Remove({}));", json!(folder))).await?;
    }
    let report = json!({
        "suite":"asset-merge-matrix",
        "passed": results.len() - failed,
        "failed": failed + usize::from(!index_preserved),
        "unityVersion": manifest["unityVersion"],
        "fixture": folder,
        "snapshots": snapshots,
        "generateMs": generate_ms,
        "indexPreserved": index_preserved,
        "cases": results,
    });
    std::fs::write(
        snapshots.join("merge-matrix.json"),
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(report)
}

pub(super) async fn run_suite(project: &str, config: &CliDriverConfig, sink: &DriverEventSink) -> Result<(), String> {
    let (_idle_cancel_sender, mut idle_cancel) = watch::channel(false);
    super::wait_for_unity_editor_idle(project, config, sink, &mut idle_cancel).await?;
    sink.emit("suite_start", json!({"suite":"asset-merge-matrix","project":project}));
    let report = run(project, sink).await?;
    let failed = report["failed"].as_u64().unwrap_or(1);
    sink.emit("suite_result", &report);
    if failed > 0 {
        return Err(format!("asset-merge-matrix: {failed} case(s) failed; report: {}", report["snapshots"]));
    }
    Ok(())
}
