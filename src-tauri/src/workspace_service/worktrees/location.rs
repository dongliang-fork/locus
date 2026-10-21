//! Where Locus places checkouts it creates.
//!
//! The base directory is resolved per request: the process override
//! (`--locus-worktree-root` / `LOCUS_WORKTREE_ROOT`), then the persisted
//! setting, then the main repository's parent. Each repository gets one
//! `<name>.worktrees` container below that base; merge validation slots use
//! `<base>/.locus-merge-validation/<projectId>`. Existing checkouts keep their
//! recorded paths, so changing the base only affects new directories.
use super::*;
use crate::workspace_service::pool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorktreeRootSource {
    /// `--locus-worktree-root` / `LOCUS_WORKTREE_ROOT` for this process.
    Process,
    /// Persisted application setting.
    Settings,
    /// Beside the main repository.
    Repository,
}

/// Effective worktree configuration for one request.
#[derive(Debug, Clone)]
pub struct WorktreePolicy {
    root: Option<PathBuf>,
    root_source: WorktreeRootSource,
    /// Optional physical-slot budget explicitly supplied by the caller.
    pub max_slots: Option<usize>,
}

impl WorktreePolicy {
    pub fn beside_repository(max_slots: impl Into<Option<usize>>) -> Self {
        Self {
            root: None,
            root_source: WorktreeRootSource::Repository,
            max_slots: max_slots.into(),
        }
    }

    pub fn with_root(root: PathBuf, root_source: WorktreeRootSource, max_slots: impl Into<Option<usize>>) -> Self {
        Self {
            root: Some(root),
            root_source,
            max_slots: max_slots.into(),
        }
    }

    /// Process override first, then the persisted setting.
    pub fn resolve(settings_root: Option<PathBuf>, max_slots: impl Into<Option<usize>>) -> Result<Self, String> {
        let max_slots = max_slots.into();
        if let Some(root) = crate::runtime_paths::runtime_worktree_root_from_env()? {
            return Ok(Self::with_root(root, WorktreeRootSource::Process, max_slots));
        }
        Ok(match settings_root {
            Some(root) => Self::with_root(root, WorktreeRootSource::Settings, max_slots),
            None => Self::beside_repository(max_slots),
        })
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn root_source(&self) -> WorktreeRootSource {
        self.root_source
    }

    /// Returns the base directory, the main repository and its directory name.
    fn base(&self, source: &Path) -> Result<(PathBuf, PathBuf, String), String> {
        let common = resolve_git_common_dir(source).ok_or("Workspace is not a Git checkout")?;
        let main = canonical(common.parent().ok_or("Git directory has no parent")?)?;
        let name = main
            .file_name()
            .ok_or("Repository directory has no name")?
            .to_string_lossy()
            .into_owned();
        let base = match &self.root {
            // A configured directory that disappeared is reported, never
            // recreated together with an arbitrary ancestor chain.
            Some(root) if !root.is_dir() => {
                return Err(format!(
                    "Worktree directory {} is unavailable",
                    root.display()
                ))
            }
            Some(root) => canonical(root)?,
            None => main
                .parent()
                .ok_or("Repository has no parent for worktrees")?
                .to_path_buf(),
        };
        Ok((base, main, name))
    }

    /// Container for new checkouts of `source`. Read-only: previews never create it.
    pub fn container(&self, source: &Path) -> Result<PathBuf, String> {
        let (base, main, name) = self.base(source)?;
        let container = base.join(format!("{name}.worktrees"));
        ensure_outside_repository(&main, &container)?;
        Ok(container)
    }

    pub fn ensure_container(&self, source: &Path) -> Result<PathBuf, String> {
        ensure_container_directory(&self.container(source)?)
    }

    /// A fresh, unused checkout directory inside the container.
    pub fn new_checkout(&self, source: &Path, prefix: &str) -> Result<PathBuf, String> {
        Ok(self
            .ensure_container(source)?
            .join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple())))
    }

    /// Pool root for exact-commit merge validation, kept apart from the
    /// container so validation slots are not mixed with session checkouts.
    pub fn merge_validation_root(&self, source: &Path, project_id: &str) -> Result<PathBuf, String> {
        if project_id.is_empty() || project_id.contains(['/', '\\']) || project_id.starts_with('.') {
            return Err("Invalid project identity for merge validation".into());
        }
        let (base, main, _) = self.base(source)?;
        let root = base.join(".locus-merge-validation").join(project_id);
        ensure_outside_repository(&main, &root)?;
        Ok(root)
    }

    /// Pool root for a slot at `commit` when the caller did not choose one:
    /// a released compatible slot is reused where it lives, otherwise a new
    /// slot goes to the container.
    pub fn pool_root_for_commit(&self, source: &Path, commit: &str) -> Result<PathBuf, String> {
        let editor = editor_version_at(source, commit)?;
        match reusable_pool_container(&list(source)?, &editor) {
            Some(path) => canonical(&path),
            None => self.ensure_container(source),
        }
    }
}

fn ensure_outside_repository(repo: &Path, path: &Path) -> Result<(), String> {
    if prospective_path_relative(repo, path)?.is_some() {
        return Err(format!(
            "Worktree directory {} must be outside the repository {}",
            path.display(),
            repo.display()
        ));
    }
    Ok(())
}

/// Create only the final container component; its parent must already exist.
/// An existing file or link at that path is rejected and left untouched.
pub(crate) fn ensure_container_directory(container: &Path) -> Result<PathBuf, String> {
    match std::fs::create_dir(container) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "Could not create worktree container {}: {error}",
                container.display()
            ))
        }
    }
    let metadata = std::fs::symlink_metadata(container).map_err(|error| {
        format!(
            "Could not inspect worktree container {}: {error}",
            container.display()
        )
    })?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || path_key(&canonical(container)?) != path_key(container)
    {
        return Err(format!(
            "Worktree container must be a directory without links: {}",
            container.display()
        ));
    }
    Ok(container.to_path_buf())
}

/// Validate a user-chosen base directory before it is persisted.
pub fn normalize_root_setting(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Worktree directory must be an absolute path".into());
    }
    if !path.is_dir() {
        return Err(format!(
            "Worktree directory {} is not an existing directory",
            path.display()
        ));
    }
    canonical(path)
}

/// Unity Editor version declared by `source`'s project at `revision`.
/// Non-Unity revisions yield an empty string.
pub(crate) fn editor_version_at(source: &Path, revision: &str) -> Result<String, String> {
    if revision.starts_with('-') {
        return Err("Invalid revision".into());
    }
    let oid = git_text(
        source,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )?;
    let repo = canonical(Path::new(&git_text(source, &["rev-parse", "--show-toplevel"])?))?;
    let relative = existing_path_relative(&repo, source)?.ok_or("Project is outside repository")?;
    let version_path = relative
        .join("ProjectSettings/ProjectVersion.txt")
        .to_string_lossy()
        .replace('\\', "/");
    let version = git_text(&repo, &["show", &format!("{oid}:{version_path}")]).unwrap_or_default();
    Ok(version
        .lines()
        .find_map(|line| line.strip_prefix("m_EditorVersion:").map(str::trim))
        .unwrap_or("")
        .to_string())
}

/// Released compatible slots are reused wherever they were created.
pub(crate) fn reusable_pool_container(records: &[ManagedWorktree], editor: &str) -> Option<PathBuf> {
    let mut candidates: Vec<_> = records
        .iter()
        .filter(|item| item.pool_slot && item.editor_version.as_deref() == Some(editor))
        .collect();
    candidates.sort_by(|a, b| a.checkout_id.cmp(&b.checkout_id));
    candidates
        .into_iter()
        .find(|item| pool::can_reuse(item))
        .and_then(|item| Path::new(&item.repo_root).parent().map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn default_container_is_beside_the_main_repository_for_linked_checkouts() {
        let (temp, source) = fixture();
        let policy = WorktreePolicy::beside_repository(2);
        let container = policy.container(&source).unwrap();
        let name = source.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            container,
            canonical(source.parent().unwrap()).unwrap().join(format!("{name}.worktrees"))
        );
        assert!(!container.exists());
        let linked = temp.path().join("linked");
        git(&source, &["worktree", "add", "-b", "feature/linked", linked.to_str().unwrap()]).unwrap();
        assert_eq!(policy.container(&linked).unwrap(), container);
        assert_eq!(
            policy.merge_validation_root(&linked, "project-a").unwrap(),
            canonical(source.parent().unwrap()).unwrap().join(".locus-merge-validation/project-a")
        );
    }

    #[test]
    fn configured_root_moves_new_directories_and_rejects_unsafe_roots() {
        let (temp, source) = fixture();
        let root = temp.path().join("configured");
        std::fs::create_dir(&root).unwrap();
        let policy = WorktreePolicy::with_root(root.clone(), WorktreeRootSource::Settings, 2);
        let name = source.file_name().unwrap().to_string_lossy().into_owned();
        let destination = policy.new_checkout(&source, "chat").unwrap();
        assert_eq!(
            destination.parent().unwrap(),
            canonical(&root).unwrap().join(format!("{name}.worktrees"))
        );
        assert!(destination.parent().unwrap().is_dir() && !destination.exists());

        let missing = WorktreePolicy::with_root(temp.path().join("missing"), WorktreeRootSource::Settings, 2);
        assert!(missing.container(&source).unwrap_err().contains("unavailable"));
        assert!(!temp.path().join("missing").exists());

        let inside = WorktreePolicy::with_root(source.join("Assets"), WorktreeRootSource::Settings, 2);
        assert!(inside.container(&source).unwrap_err().contains("outside the repository"));
        assert!(normalize_root_setting(Path::new("relative")).is_err());
        assert!(normalize_root_setting(&temp.path().join("missing")).is_err());
        assert_eq!(normalize_root_setting(&root).unwrap(), canonical(&root).unwrap());
    }

    #[test]
    fn container_rejects_existing_files_and_directory_links() {
        let temp = tempfile::tempdir().unwrap();
        let base = canonical(temp.path()).unwrap();
        let file = base.join("project.worktrees");
        std::fs::write(&file, b"preserve existing file").unwrap();
        let error = ensure_container_directory(&file).unwrap_err();
        assert!(error.contains("without links") && error.contains("project.worktrees"));
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve existing file");

        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = base.join("linked.worktrees");
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_dir(&target, &link);
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&target, &link);
        match created {
            Ok(()) => {
                assert!(ensure_container_directory(&link).unwrap_err().contains("without links"));
                assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(error) => panic!("Could not create directory link fixture: {error}"),
        }
    }
}
