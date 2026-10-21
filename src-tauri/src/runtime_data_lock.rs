use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::{FileExt, TryLockError};

const LOCK_FILE_NAME: &str = ".locus-instance.lock";
const OWNER_FILE_NAME: &str = ".locus-instance.owner";

#[derive(Debug, thiserror::Error)]
pub(crate) enum RuntimeDataDirLockError {
    #[error("Locus data directory '{}' is already in use by another process ({owner}). Close that Locus instance or use an isolated LOCUS_RUNTIME_DATA_DIR.", data_dir.display())]
    InUse { data_dir: PathBuf, owner: String },
    #[error("{0}")]
    Io(String),
}

impl From<String> for RuntimeDataDirLockError {
    fn from(error: String) -> Self {
        Self::Io(error)
    }
}

impl RuntimeDataDirLockError {
    pub(crate) fn user_message(&self, is_zh: bool) -> String {
        match self {
            Self::InUse { data_dir, .. } if is_zh => format!(
                "数据目录正在被其他进程使用：\n{}\n\n请关闭使用此目录的 Locus 后重试。",
                data_dir.display()
            ),
            Self::InUse { data_dir, .. } => format!(
                "The data directory is in use by another process:\n{}\n\nClose the Locus instance using this directory and try again.",
                data_dir.display()
            ),
            Self::Io(error) if is_zh => format!("无法访问数据目录。\n\n{error}"),
            Self::Io(error) => format!("Unable to access the data directory.\n\n{error}"),
        }
    }
}

pub(crate) struct RuntimeDataDirLock {
    _file: File,
    path: PathBuf,
    owner_path: PathBuf,
}

impl RuntimeDataDirLock {
    pub(crate) fn acquire(data_dir: &Path) -> Result<Self, RuntimeDataDirLockError> {
        std::fs::create_dir_all(data_dir).map_err(|error| {
            format!(
                "Failed to create runtime data directory '{}': {}",
                data_dir.display(),
                error
            )
        })?;
        let path = data_dir.join(LOCK_FILE_NAME);
        let owner_path = data_dir.join(OWNER_FILE_NAME);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "Failed to open runtime data directory lock '{}': {}",
                    path.display(),
                    error
                )
            })?;

        match FileExt::try_lock(&file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                // Windows byte-range locks prevent reads through another handle.
                // Keep diagnostics outside the locked file; legacy runtimes may
                // only have metadata inside the lock file.
                let owner = std::fs::read_to_string(&owner_path)
                    .or_else(|_| std::fs::read_to_string(&path))
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "owner metadata unavailable".to_string());
                return Err(RuntimeDataDirLockError::InUse {
                    data_dir: data_dir.to_path_buf(),
                    owner,
                });
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!(
                    "Failed to lock runtime data directory '{}': {}",
                    data_dir.display(),
                    error
                )
                .into());
            }
        }

        file.set_len(0).map_err(|error| {
            format!(
                "Failed to reset runtime data directory lock '{}': {}",
                path.display(),
                error
            )
        })?;
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            format!(
                "Failed to seek runtime data directory lock '{}': {}",
                path.display(),
                error
            )
        })?;
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let executable = std::env::current_exe()
            .map(|value| value.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let owner = format!(
            "pid={} started_at={} executable={}",
            std::process::id(),
            started_at,
            executable
        );
        file.write_all(owner.as_bytes()).map_err(|error| {
            format!(
                "Failed to write runtime data directory lock '{}': {}",
                path.display(),
                error
            )
        })?;
        file.flush().map_err(|error| {
            format!(
                "Failed to flush runtime data directory lock '{}': {}",
                path.display(),
                error
            )
        })?;

        // Publish only after taking the OS lock. Metadata is diagnostic only;
        // stale files never authorize bypassing the lock.
        std::fs::write(&owner_path, owner).map_err(|error| {
            format!(
                "Failed to write runtime data directory owner '{}': {}",
                owner_path.display(),
                error
            )
        })?;

        Ok(Self {
            _file: file,
            path,
            owner_path,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RuntimeDataDirLock {
    fn drop(&mut self) {
        // Remove diagnostics while we still own the lock, before another
        // process can acquire it and publish its own metadata. Keep the lock
        // file itself so all contenders continue to lock the same file.
        let _ = std::fs::remove_file(&self.owner_path);
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeDataDirLock, RuntimeDataDirLockError, LOCK_FILE_NAME, OWNER_FILE_NAME};

    #[test]
    fn exclusive_lock_blocks_a_second_runtime_and_recovers_after_drop() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = RuntimeDataDirLock::acquire(root.path()).expect("first lock");
        let error = RuntimeDataDirLock::acquire(root.path())
            .err()
            .expect("second lock should fail");

        assert!(matches!(error, RuntimeDataDirLockError::InUse { .. }));
        assert!(error
            .to_string()
            .contains(&format!("pid={}", std::process::id())));
        assert!(error.user_message(true).contains("请关闭"));
        assert!(error.user_message(false).contains("try again"));
        assert!(first.path().is_file());

        drop(first);
        assert!(!root.path().join(OWNER_FILE_NAME).exists());
        RuntimeDataDirLock::acquire(root.path()).expect("lock after release");
    }

    #[test]
    fn stale_metadata_does_not_block_startup() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(LOCK_FILE_NAME), "pid=old").unwrap();
        std::fs::write(root.path().join(OWNER_FILE_NAME), "pid=old").unwrap();
        let _lock = RuntimeDataDirLock::acquire(root.path()).expect("stale lock recovery");
        let owner = std::fs::read_to_string(root.path().join(OWNER_FILE_NAME)).unwrap();
        assert!(owner.contains(&format!("pid={}", std::process::id())));
    }

    #[test]
    fn missing_owner_metadata_does_not_bypass_lock() {
        let root = tempfile::tempdir().expect("tempdir");
        let _first = RuntimeDataDirLock::acquire(root.path()).expect("first lock");
        std::fs::remove_file(root.path().join(OWNER_FILE_NAME)).unwrap();
        let error = RuntimeDataDirLock::acquire(root.path())
            .err()
            .expect("conflict");
        assert!(matches!(error, RuntimeDataDirLockError::InUse { .. }));
        assert!(error
            .user_message(true)
            .contains(&root.path().display().to_string()));
    }

    #[test]
    fn separate_data_directories_can_be_locked_independently() {
        let root = tempfile::tempdir().expect("tempdir");
        let _first = RuntimeDataDirLock::acquire(&root.path().join("first")).unwrap();
        let _second = RuntimeDataDirLock::acquire(&root.path().join("second")).unwrap();
    }

    #[test]
    fn invalid_directory_reports_io_error_instead_of_lock_conflict() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("file");
        std::fs::write(&path, "not a directory").unwrap();
        let error = RuntimeDataDirLock::acquire(&path)
            .err()
            .expect("invalid directory");
        assert!(matches!(error, RuntimeDataDirLockError::Io(_)));
        assert!(error.user_message(true).contains("无法访问"));
    }
}
