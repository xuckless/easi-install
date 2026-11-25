//! Allowed install write roots and path-containment safety checks.

use std::path::{Component, Path, PathBuf};

use crate::error::EasiError;

/// Shared directories under ~/.config / ~/.local/share that belong to no single app (easi's own
/// roots, desktop-wide registries and autostart/service hooks); never treated as app data.
pub const SHARED_XDG_DIRS: &[&str] = &[
    "easi",
    "applications",
    "icons",
    "Trash",
    "autostart",
    "systemd",
    "mime",
    "fonts",
];

/// The ONLY write roots easi is allowed to touch (AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRoots {
    pub home_dir: PathBuf,
    pub applications_dir: PathBuf,
    pub desktop_dir: PathBuf,
    pub icons_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub easi_dir: PathBuf,
    /// `$XDG_STATE_HOME/easi` (default `~/.local/state/easi`): the operation journal.
    pub state_dir: PathBuf,
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
            state_dir: home.join(".local/state/easi"),
            trash_dir: local_share.join("Trash"),
        }
    }

    pub fn default_user() -> Result<Self, EasiError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| EasiError::Desktop("HOME environment variable is not set".into()))?;
        // The XDG base-dir spec says to ignore a relative XDG_STATE_HOME.
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        Ok(Self {
            state_dir: state_home.map_or_else(
                || home.join(".local/state/easi"),
                |state| state.join("easi"),
            ),
            ..Self::from_home(&home)
        })
    }

    /// Per-user settings/data bases (`~/.config`, `~/.local/share`) that apps and consented
    /// installer scripts write their own top-level entries into.
    pub fn user_data_bases(&self) -> [PathBuf; 2] {
        [
            self.home_dir.join(".config"),
            self.home_dir.join(".local/share"),
        ]
    }

    /// Whether `path` may be purged as installer-created data: a direct child of a
    /// [`Self::user_data_bases`] entry that is not a shared XDG directory, so a tampered
    /// manifest cannot widen what a purge deletes.
    pub fn is_purgeable_user_data(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        path.parent()
            .is_some_and(|p| self.user_data_bases().iter().any(|b| b == p))
            && !SHARED_XDG_DIRS.contains(&name)
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

    /// Advisory lock every mutating easi operation holds (see `oplog::StateLock`).
    pub fn lock_path(&self) -> PathBuf {
        self.easi_dir.join(".lock")
    }

    pub fn journal_path(&self) -> PathBuf {
        self.state_dir.join("journal.log")
    }

    /// Check whether `path` is strictly inside one of easi's allowed per-user write roots
    /// without following symlinks on the leaf.
    pub fn is_within_allowed_write_roots(&self, path: &Path) -> bool {
        // `Path::starts_with` treats `..` as an ordinary component, so `root/../.bashrc` would
        // pass the prefix test below; refuse anything that is not plain names under `/`.
        // Relative paths are refused too: the canonicalize fallback would resolve them from cwd.
        if !path.is_absolute()
            || !path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return false;
        }
        let allowed = [
            &self.applications_dir,
            &self.desktop_dir,
            &self.icons_dir,
            &self.bin_dir,
            &self.easi_dir,
            &self.state_dir,
        ];
        allowed.iter().any(|root| {
            if path.starts_with(root) && path != root.as_path() {
                return true;
            }
            if let (Ok(cr), Ok(cp)) = (
                std::fs::canonicalize(root),
                path.parent()
                    .ok_or(())
                    .and_then(|p| std::fs::canonicalize(p).map_err(|_| ())),
            ) {
                return cp.starts_with(&cr);
            }
            false
        })
    }
}

/// Validate that an `app_id` cannot traverse directories or inject special characters.
pub fn validate_app_id(app_id: &str) -> Result<(), EasiError> {
    if app_id.is_empty()
        || app_id.len() > 128
        || app_id.starts_with('.')
        || app_id.contains("..")
        || !app_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(EasiError::PathEscape(format!("invalid app_id: {app_id}")));
    }
    Ok(())
}

/// Normalize a relative path and verify it cannot escape `root` (blocks `..`, root prefixes,
/// and symlink traversal — including dangling symlinks — on `target` and all existing ancestors).
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

    // Check every existing path component from `canon_root` down to `target` without skipping
    // dangling symlinks (`symlink_metadata` succeeds on broken symlinks where `exists()` is false).
    let mut cur = canon_root.clone();
    for comp in normalized.components() {
        cur.push(comp);
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                let canon = std::fs::canonicalize(&cur)
                    .map_err(|_| EasiError::PathEscape(rel.to_string_lossy().into_owned()))?;
                if !canon.starts_with(&canon_root) {
                    return Err(EasiError::PathEscape(rel.to_string_lossy().into_owned()));
                }
            }
        } else {
            break;
        }
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

        // Block leaf symlink and dangling symlink escapes.
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("leaf_escape")).unwrap();
        assert!(safe_join(tmp.path(), Path::new("leaf_escape")).is_err());
        std::os::unix::fs::symlink("/nonexistent/outside", tmp.path().join("dangling_dir"))
            .unwrap();
        assert!(safe_join(tmp.path(), Path::new("dangling_dir/payload")).is_err());

        // validate_app_id blocks traversal and control chars.
        assert!(validate_app_id("easi.my-app_1").is_ok());
        assert!(validate_app_id("../../.bashrc").is_err());
        assert!(validate_app_id(".hidden").is_err());
        assert!(validate_app_id("foo/bar").is_err());
    }

    #[test]
    fn only_direct_app_owned_children_of_data_bases_are_purgeable() {
        let roots = InstallRoots::from_home(Path::new("/home/u"));
        // (path, purgeable)
        let rows = [
            ("/home/u/.config/tool", true),
            ("/home/u/.local/share/tool-data", true),
            ("/home/u/.config/tool/sub", false),
            ("/home/u/.config", false),
            ("/home/u/.bashrc", false),
            ("/home/u/.ssh/id_ed25519", false),
            ("/home/u/.config/autostart", false),
            ("/home/u/.local/share/applications", false),
            ("/home/u/.local/share/easi", false),
            ("/home/u/.config/..", false),
            ("/home/u/.config/../.bashrc", false),
            ("/other/.config/tool", false),
        ];
        for (path, purgeable) in rows {
            assert_eq!(
                roots.is_purgeable_user_data(Path::new(path)),
                purgeable,
                "{path}"
            );
        }
    }

    #[test]
    fn write_root_guard_rejects_traversal_components() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        let apps = roots.applications_dir.to_string_lossy().into_owned();
        // (path, allowed)
        let rows: &[(String, bool)] = &[
            (format!("{apps}/App/bin/app"), true),
            // `Path` drops interior `.` while parsing, so it cannot change where the path points.
            (format!("{apps}/./App"), true),
            (format!("{apps}/../.bashrc"), false),
            (format!("{apps}/App/../../.bashrc"), false),
            ("./Applications/App".to_string(), false),
            ("../Applications/App".to_string(), false),
            ("Applications/App".to_string(), false),
            (apps.clone(), false),
            (
                tmp.path().join(".bashrc").to_string_lossy().into_owned(),
                false,
            ),
        ];
        for (path, allowed) in rows {
            assert_eq!(
                roots.is_within_allowed_write_roots(Path::new(path)),
                *allowed,
                "{path}"
            );
        }
    }
}
