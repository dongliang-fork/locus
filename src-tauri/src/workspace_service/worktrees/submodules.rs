//! Submodules of a managed checkout are linked worktrees of the source
//! checkout's populated submodule repositories. Nothing is fetched, objects and
//! LFS storage stay shared, and no Git configuration changes. A gitlink the
//! source has not populated stays an empty directory, matching the source.
//!
//! Git neither updates submodules on a plain superproject checkout nor removes
//! a worktree around populated submodules, so callers prune them before
//! `worktree remove` or a pool checkout and materialize them afterwards.
use super::*;

/// A source-populated gitlink, resolved before any checkout is mutated.
#[derive(Debug, Clone)]
pub(crate) struct PlannedSubmodule {
    path: String,
    commit: String,
    repository: PathBuf,
    nested: Vec<PlannedSubmodule>,
}

/// Whether `path` lies in a planned submodule, which the checkout materializes
/// separately from the superproject tree.
pub(crate) fn covers(planned: &[PlannedSubmodule], path: &str) -> bool {
    planned.iter().any(|submodule| {
        path == submodule.path
            || path
                .strip_prefix(submodule.path.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Gitlinks of a commit tree. Symbolic links are rejected: a link target may
/// alias files outside the isolated checkout.
fn tree_gitlinks(root: &Path, oid: &str) -> Result<Vec<(String, String)>, String> {
    let tree = git(root, &["ls-tree", "-r", "-z", oid])?;
    let mut links = Vec::new();
    for entry in tree.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("Invalid Git tree entry")?;
        let (header, name) = (&entry[..tab], &entry[tab + 1..]);
        if header.starts_with(b"120000 ") {
            return Err(format!(
                "Worktree creation does not support symbolic links ({}); a link could alias files outside the isolated checkout",
                String::from_utf8_lossy(name)
            ));
        }
        if header.starts_with(b"160000 ") {
            let header = std::str::from_utf8(header).map_err(io_error)?;
            let commit = header
                .rsplit(' ')
                .next()
                .ok_or("Invalid Git tree entry")?
                .to_string();
            let name = String::from_utf8(name.to_vec()).map_err(io_error)?;
            safe_relative(&name)?;
            links.push((name, commit));
        }
    }
    Ok(links)
}

/// Gitlinks recorded in a checkout's index, i.e. those Git considers when it
/// decides whether the worktree contains populated submodules.
fn index_gitlinks(checkout: &Path) -> Result<Vec<String>, String> {
    let index = git(checkout, &["ls-files", "--stage", "-z"])?;
    let mut links = Vec::new();
    for entry in index.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if !entry.starts_with(b"160000 ") {
            continue;
        }
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("Invalid Git index entry")?;
        let name = String::from_utf8(entry[tab + 1..].to_vec()).map_err(io_error)?;
        safe_relative(&name)?;
        links.push(name);
    }
    Ok(links)
}

fn rev_parse_path(checkout: &Path, flag: &str) -> Result<PathBuf, String> {
    let raw = git_text(checkout, &["rev-parse", flag])?;
    let path = Path::new(&raw);
    canonical(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        checkout.join(path)
    })
}

/// The repository behind a populated submodule checkout, or None when Git left
/// the gitlink unpopulated (an empty or absent directory).
fn populated_repository(checkout: &Path) -> Result<Option<PathBuf>, String> {
    match std::fs::symlink_metadata(checkout.join(".git")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    if !path_components_equal(
        &rev_parse_path(checkout, "--show-toplevel")?,
        &canonical(checkout)?,
    )? {
        return Err(format!(
            "Submodule checkout {} resolves to another repository",
            checkout.display()
        ));
    }
    Ok(Some(rev_parse_path(checkout, "--git-common-dir")?))
}

/// Commands addressed to a submodule repository itself. Its shared config
/// carries core.worktree for the source checkout, which may be absent; an
/// explicit work tree keeps Git from resolving it.
fn repository_git(repository: &Path, work_tree: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let repository = repository.to_string_lossy();
    let work_tree_text = work_tree.to_string_lossy();
    let mut command = vec!["--git-dir", &repository, "--work-tree", &work_tree_text];
    command.extend_from_slice(args);
    git(work_tree, &command)
}

/// Resolve the gitlinks of `oid` that `source` has populated. Missing commits
/// fail here, before any destination is touched.
pub(crate) fn plan(source: &Path, oid: &str) -> Result<Vec<PlannedSubmodule>, String> {
    let mut planned = Vec::new();
    for (path, commit) in tree_gitlinks(source, oid)? {
        let checkout = source.join(safe_relative(&path)?);
        let Some(repository) = populated_repository(&checkout)? else {
            continue;
        };
        if git(&checkout, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
            return Err(format!(
                "Submodule {path} commit {commit} is not available in the source checkout; fetch it in that submodule first"
            ));
        }
        let nested = plan(&checkout, &commit)
            .map_err(|error| format!("Submodule {path}: {error}"))?;
        planned.push(PlannedSubmodule {
            path,
            commit,
            repository,
            nested,
        });
    }
    Ok(planned)
}

/// Bring `checkout`'s submodules to the planned commits. A populated entry must
/// already belong to the planned repository; Git's checkout refuses to
/// overwrite local changes in it.
pub(crate) fn materialize(checkout: &Path, planned: &[PlannedSubmodule]) -> Result<(), String> {
    for submodule in planned {
        let path = &submodule.path;
        let target = checkout.join(safe_relative(path)?);
        let context = |error: String| format!("Submodule {path}: {error}");
        match populated_repository(&target).map_err(context)? {
            Some(repository) => {
                if !path_components_equal(&repository, &submodule.repository)? {
                    return Err(format!(
                        "Submodule {path} is populated from another repository; remove it before reusing this checkout"
                    ));
                }
                if git_text(&target, &["rev-parse", "HEAD"]).map_err(context)? != submodule.commit {
                    git(
                        &target,
                        &["checkout", "--detach", "--no-overwrite-ignore", &submodule.commit, "--"],
                    )
                    .map_err(context)?;
                }
            }
            None => {
                if target.exists()
                    && std::fs::read_dir(&target).map_err(io_error)?.next().is_some()
                {
                    return Err(format!("Unpopulated submodule directory {path} is not empty"));
                }
                std::fs::create_dir_all(&target).map_err(io_error)?;
                let target_text = target.to_string_lossy();
                repository_git(
                    &submodule.repository,
                    checkout,
                    &["worktree", "add", "--detach", "--", &target_text, &submodule.commit],
                )
                .map_err(context)?;
            }
        }
        materialize(&target, &submodule.nested).map_err(context)?;
        ensure_lfs_materialized(&target).map_err(context)?;
    }
    Ok(())
}

/// Remove populated submodule checkouts under `checkout` that `keep` does not
/// retain at the same path and repository; retained ones are pruned
/// recursively. Without --force, Git verifies each removed checkout is clean.
pub(crate) fn prune(checkout: &Path, keep: &[PlannedSubmodule]) -> Result<(), String> {
    for path in index_gitlinks(checkout)? {
        let target = checkout.join(safe_relative(&path)?);
        let context = |error: String| format!("Submodule {path}: {error}");
        let Some(repository) = populated_repository(&target).map_err(context)? else {
            continue;
        };
        let mut retained = None;
        for submodule in keep.iter().filter(|s| s.path == path) {
            if path_components_equal(&submodule.repository, &repository)? {
                retained = Some(submodule);
            }
        }
        if let Some(submodule) = retained {
            prune(&target, &submodule.nested).map_err(context)?;
            continue;
        }
        prune(&target, &[]).map_err(context)?;
        if path_components_equal(&rev_parse_path(&target, "--git-dir")?, &repository)? {
            return Err(format!(
                "Submodule {path} has its own repository instead of a Locus linked checkout; remove it manually"
            ));
        }
        let target_text = target.to_string_lossy();
        repository_git(&repository, checkout, &["worktree", "remove", "--", &target_text])
            .map_err(context)?;
        // Git represents an unpopulated gitlink as an empty directory.
        std::fs::create_dir_all(&target).map_err(io_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;
    use crate::workspace_service::pool::{self, AcquirePoolRequest};

    fn commit(root: &Path, message: &str) {
        git(
            root,
            &[
                "-c",
                "user.name=Worktree tests",
                "-c",
                "user.email=worktree-test@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                message,
            ],
        )
        .unwrap();
    }

    fn origin(temp: &Path, name: &str) -> PathBuf {
        let origin = temp.join(name);
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init"]).unwrap();
        git(&origin, &["config", "core.autocrlf", "false"]).unwrap();
        std::fs::write(origin.join("bank.txt"), "bank v1").unwrap();
        git(&origin, &["add", "."]).unwrap();
        commit(&origin, "bank v1");
        origin
    }

    /// `Audio/wwise` is populated in the source; `Reference/lib` is not.
    fn fixture_with_submodules() -> (tempfile::TempDir, PathBuf) {
        let (temp, repo) = fixture();
        for (name, path) in [("wwise-origin", "Audio/wwise"), ("lib-origin", "Reference/lib")] {
            let origin = origin(temp.path(), name);
            git(
                &repo,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "--",
                    &origin.to_string_lossy(),
                    path,
                ],
            )
            .unwrap();
        }
        commit(&repo, "add submodules");
        git(&repo, &["submodule", "deinit", "-f", "--", "Reference/lib"]).unwrap();
        (temp, repo)
    }

    fn linked_checkouts(repository_checkout: &Path) -> usize {
        git(repository_checkout, &["worktree", "list", "--porcelain", "-z"])
            .unwrap()
            .split(|b| *b == 0)
            .filter(|part| part.starts_with(b"worktree "))
            .count()
    }

    #[test]
    fn create_follows_source_population_and_remove_retires_submodule_checkouts() {
        let (temp, repo) = fixture_with_submodules();
        let config_before = std::fs::read(repo.join(".git/config")).unwrap();
        let record = create(&CreateWorktreeRequest {
            source_root: repo.to_string_lossy().into(),
            destination: temp.path().join("target").to_string_lossy().into(),
            branch: "codex/submodules".into(),
            start_ref: None,
            include_dirty: false,
        })
        .unwrap();
        let root = Path::new(&record.root);
        assert_eq!(
            std::fs::read_to_string(root.join("Audio/wwise/bank.txt")).unwrap(),
            "bank v1"
        );
        assert!(root.join("Reference/lib").is_dir());
        assert!(std::fs::read_dir(root.join("Reference/lib")).unwrap().next().is_none());
        assert!(!record.dirty);
        assert_eq!(linked_checkouts(&repo.join("Audio/wwise")), 2);
        assert_eq!(std::fs::read(repo.join(".git/config")).unwrap(), config_before);

        remove(&repo, &record.checkout_id, 1).unwrap();
        assert!(!root.exists());
        assert_eq!(linked_checkouts(&repo.join("Audio/wwise")), 1);
        assert!(git(&repo.join("Audio/wwise"), &["status", "--porcelain"])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dirty_submodule_checkout_blocks_removal_without_losing_changes() {
        let (temp, repo) = fixture_with_submodules();
        let record = create(&CreateWorktreeRequest {
            source_root: repo.to_string_lossy().into(),
            destination: temp.path().join("target").to_string_lossy().into(),
            branch: "codex/dirty-submodule".into(),
            start_ref: None,
            include_dirty: false,
        })
        .unwrap();
        let bank = Path::new(&record.root).join("Audio/wwise/bank.txt");
        std::fs::write(&bank, "local edit\n").unwrap();
        assert!(remove(&repo, &record.checkout_id, 1).is_err());
        assert_eq!(std::fs::read_to_string(&bank).unwrap(), "local edit\n");
    }

    #[test]
    fn pool_reuse_moves_and_retires_submodule_checkouts_with_the_assigned_commit() {
        let (temp, repo) = fixture_with_submodules();
        let pool_root = temp.path().join("pool");
        std::fs::create_dir(&pool_root).unwrap();
        let acquire = |assignment: &str| {
            pool::acquire(&AcquirePoolRequest {
                source_root: repo.to_string_lossy().into(),
                pool_root: pool_root.to_string_lossy().into(),
                commit: "HEAD".into(),
                branch: None,
                max_slots: Some(1),
                assignment_id: Some(assignment.into()),
            })
            .unwrap()
            .worktree
        };
        let first = acquire("first");
        let slot = PathBuf::from(&first.root);
        pool::release(&repo, &first.checkout_id, "first", first.materialization_epoch).unwrap();

        let source_module = repo.join("Audio/wwise");
        std::fs::write(source_module.join("bank.txt"), "bank v2").unwrap();
        git(&source_module, &["add", "."]).unwrap();
        commit(&source_module, "bank v2");
        git(&repo, &["add", "Audio/wwise"]).unwrap();
        commit(&repo, "bump bank");
        let second = acquire("second");
        assert_eq!(second.checkout_id, first.checkout_id);
        assert_eq!(
            std::fs::read_to_string(slot.join("Audio/wwise/bank.txt")).unwrap(),
            "bank v2"
        );
        assert!(git(&slot, &["status", "--porcelain", "--untracked-files=all"])
            .unwrap()
            .is_empty());
        pool::release(&repo, &second.checkout_id, "second", second.materialization_epoch).unwrap();

        git(&repo, &["rm", "-q", "Audio/wwise"]).unwrap();
        commit(&repo, "drop bank");
        let third = acquire("third");
        assert_eq!(third.checkout_id, first.checkout_id);
        assert!(!slot.join("Audio/wwise/.git").exists());
        assert!(git(&slot, &["status", "--porcelain", "--untracked-files=all"])
            .unwrap()
            .is_empty());
        let module = rev_parse_path(&repo, "--git-common-dir").unwrap().join("modules/Audio/wwise");
        let listed = repository_git(&module, &repo, &["worktree", "list", "--porcelain", "-z"]).unwrap();
        assert_eq!(listed.split(|b| *b == 0).filter(|p| p.starts_with(b"worktree ")).count(), 1);
    }

    #[test]
    fn unavailable_submodule_commits_and_symbolic_links_fail_before_checkout() {
        let (temp, repo) = fixture_with_submodules();
        let missing = "1111111111111111111111111111111111111111";
        git(
            &repo,
            &["update-index", "--cacheinfo", &format!("160000,{missing},Audio/wwise")],
        )
        .unwrap();
        commit(&repo, "missing bank commit");
        let destination = temp.path().join("target");
        let error = create(&CreateWorktreeRequest {
            source_root: repo.to_string_lossy().into(),
            destination: destination.to_string_lossy().into(),
            branch: "codex/missing-submodule".into(),
            start_ref: None,
            include_dirty: false,
        })
        .unwrap_err();
        assert!(error.contains("not available"), "{error}");
        assert!(!destination.exists());

        let (_temp, repo) = fixture();
        std::fs::write(repo.join("link-target.txt"), "Assets").unwrap();
        let blob = git_text(&repo, &["hash-object", "-w", "link-target.txt"]).unwrap();
        std::fs::remove_file(repo.join("link-target.txt")).unwrap();
        git(
            &repo,
            &["update-index", "--add", "--cacheinfo", &format!("120000,{blob},Assets/link")],
        )
        .unwrap();
        commit(&repo, "symbolic link");
        let oid = git_text(&repo, &["rev-parse", "HEAD"]).unwrap();
        let error = ensure_tree_supported(&repo, &oid, "").unwrap_err();
        assert!(error.contains("symbolic links"), "{error}");
    }

    #[test]
    fn local_changes_inside_a_submodule_are_not_silently_dropped() {
        let (temp, repo) = fixture_with_submodules();
        std::fs::write(repo.join("Audio/wwise/bank.txt"), "local bank\n").unwrap();
        let error = create(&CreateWorktreeRequest {
            source_root: repo.to_string_lossy().into(),
            destination: temp.path().join("target").to_string_lossy().into(),
            branch: "codex/dirty-source-submodule".into(),
            start_ref: None,
            include_dirty: true,
        })
        .unwrap_err();
        assert!(error.contains("inside submodule Audio/wwise"), "{error}");
    }
}
