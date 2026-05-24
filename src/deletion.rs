use anyhow::{Context, Result, bail};
use std::path::{Component, Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteMode {
    Trash,
    Permanent,
}

pub fn ensure_deletable(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("empty paths cannot be deleted");
    }

    if path.parent().is_none() || path.components().all(|c| matches!(c, Component::RootDir)) {
        bail!("filesystem roots cannot be deleted");
    }

    Ok(())
}

pub fn delete_path(path: &Path, mode: DeleteMode) -> Result<()> {
    ensure_deletable(path)?;
    match mode {
        DeleteMode::Trash => {
            trash::delete(path).with_context(|| format!("failed to trash {}", path.display()))
        }
        DeleteMode::Permanent => {
            if path.is_dir() {
                std::fs::remove_dir_all(path).with_context(|| {
                    format!("failed to permanently delete directory {}", path.display())
                })
            } else {
                std::fs::remove_file(path).with_context(|| {
                    format!("failed to permanently delete file {}", path.display())
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_roots_and_empty_paths() {
        assert!(ensure_deletable(Path::new("")).is_err());
        assert!(ensure_deletable(Path::new("/")).is_err());
    }

    #[test]
    fn permanently_deletes_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("victim.txt");
        std::fs::write(&file, "bye").expect("write temp file");

        delete_path(&file, DeleteMode::Permanent).expect("delete file");

        assert!(!file.exists());
    }
}
