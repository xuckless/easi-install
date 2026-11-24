//! Allowed install write roots and path-containment safety checks.

use std::path::{Component, Path, PathBuf};

use crate::error::EasiError;

/// The ONLY write roots easi is allowed to touch (AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRoots {
    pub home_dir: PathBuf,
    pub applications_dir: PathBuf,
    pub desktop_dir: PathBuf,
    pub icons_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub easi_dir: PathBuf,
    pub trash_dir: PathBuf,
}

impl InstallRoots {
    pub fn from_home(home: &Path) -> Self {
        let local_share = home.join(".local/share");
        Self {
            home_dir: home.to_path_buf(),
            applications_dir: home.join("Applications"),
            desktop_dir: local_share.join("applications"),
            icons_dir: local_share.join("icons/hicolor"),
            bin_dir: home.join(".local/bin"),
            easi_dir: local_share.join("easi"),
            trash_dir: local_share.join("Trash"),
        }
    }

    pub fn default_user() -> Result<Self, EasiError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| EasiError::Desktop("HOME environment variable is not set".into()))?;
        Ok(Self::from_home(&home))
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.easi_dir.join("installed")
    }

    pub fn previous_handlers_path(&self) -> PathBuf {
        self.easi_dir.join("previous_handlers.toml")
    }

    pub fn config_path(&self) -> PathBuf {
        self.easi_dir.join("config.toml")
    }
}

/// Normalize a relative path and verify it cannot escape `root` (blocks `..`, root prefixes,
/// and symlink traversal on existing ancestors).
pub fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf, EasiError> {
    let mut normalized = PathBuf::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(EasiError::PathEscape(rel.to_string_lossy().into_owned()));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(EasiError::PathEscape(rel.to_string_lossy().into_owned()));
    }

    let canon_root = std::fs::canonicalize(root)?;
    let target = canon_root.join(&normalized);

    // Check every existing ancestor inside root is not a symlink escaping root.
    let mut cur = target.as_path();
    while let Some(parent) = cur.parent() {
        if parent.exists() {
            let canon_parent = std::fs::canonicalize(parent)?;
            if !canon_parent.starts_with(&canon_root) {
                return Err(EasiError::PathEscape(rel.to_string_lossy().into_owned()));
            }
            break;
        }
        cur = parent;
    }

    Ok(target)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn safe_join_blocks_escapes_and_allows_nested_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let ok = safe_join(tmp.path(), Path::new("bin/app")).expect("valid join");
        assert!(ok.ends_with("bin/app"));

        assert!(safe_join(tmp.path(), Path::new("../outside")).is_err());
        assert!(safe_join(tmp.path(), Path::new("/etc/passwd")).is_err());
        assert!(safe_join(tmp.path(), Path::new("a/../../b")).is_err());
    }
}
