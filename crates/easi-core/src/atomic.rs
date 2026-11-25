//! Crash-safe replacement of persistent files (manifests, config, launchers, wrappers, units).

use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

/// Replace `path` with `bytes` and exact `mode` so a crash leaves either the old or the new file,
/// never a truncated one. The randomly named temp file sits in the target's directory so the
/// rename stays on one filesystem; a symlink at `path` is replaced, never followed.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file()
        .set_permissions(Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    File::open(dir)?.sync_all()
}

/// Keep an unreadable state file as `<file>.corrupt-<n>` (first free `n`) and return that path.
/// `hard_link` never replaces an existing file, so concurrent recoveries (no lock: callers may
/// already hold it) cannot clobber an earlier copy, and the original stays until replaced.
pub fn keep_aside(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    (1..=1000)
        .map(|n| path.with_file_name(format!("{name}.corrupt-{n}")))
        .find(|aside| fs::hard_link(path, aside).is_ok())
}

/// Read a TOML state file: `missing` when it does not exist, its value when it parses. One that
/// does not parse is kept aside ([`keep_aside`]) and atomically replaced by `fallback`, so it is
/// never silently reset and later loads stay on the fallback; the second value is where the bad
/// file now is.
pub fn load_toml_checked<T: DeserializeOwned + Serialize>(
    path: &Path,
    missing: T,
    fallback: T,
) -> (T, Option<PathBuf>) {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (missing, None),
        Err(_) => return (fallback, Some(path.to_path_buf())),
    };
    if let Some(value) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| toml::from_str(s).ok())
    {
        return (value, None);
    }
    let Some(aside) = keep_aside(path) else {
        return (fallback, Some(path.to_path_buf()));
    };
    if let Ok(text) = toml::to_string_pretty(&fallback) {
        let _ = write_atomic(path, text.as_bytes(), 0o644);
    }
    (fallback, Some(aside))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn replaces_content_and_mode_without_leftovers_or_following_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        let dir = tmp.path().join("d");
        fs::create_dir(&dir).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let fresh = dir.join("fresh");
        fs::write(dir.join("existing"), "old").unwrap();

        // (target, mode)
        let rows = [
            (&fresh, 0o644),
            (&dir.join("existing"), 0o755),
            (&link, 0o600),
        ];
        for (path, mode) in rows {
            write_atomic(path, b"new", mode).unwrap();
            assert_eq!(fs::read(path).unwrap(), b"new", "{}", path.display());
            let got = fs::metadata(path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(got, mode, "{}", path.display());
        }
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert!(
            !fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
    }
}
