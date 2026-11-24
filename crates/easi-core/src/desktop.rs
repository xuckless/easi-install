//! Desktop integration (M4): MIME registration (`easi --setup`), previous-handler persistence,
//! and silent handoff of non-app archives.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::EasiError;
use crate::install::refresh_desktop_caches;
use crate::roots::InstallRoots;

pub const EASI_DESKTOP_ID: &str = "easi.desktop";

/// All MIME types easi handles by default after `easi --setup`.
pub const SUPPORTED_MIMES: &[&str] = &[
    "application/x-iso9660-appimage",
    "application/vnd.appimage",
    "application/x-compressed-tar",
    "application/x-xz-compressed-tar",
    "application/x-zstd-compressed-tar",
    "application/x-bzip-compressed-tar",
    "application/zip",
];

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SavedHandlers {
    pub defaults: BTreeMap<String, String>,
}

pub fn load_saved_handlers(roots: &InstallRoots) -> SavedHandlers {
    let path = roots.previous_handlers_path();
    fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_handlers(roots: &InstallRoots, handlers: &SavedHandlers) -> Result<PathBuf, EasiError> {
    fs::create_dir_all(&roots.easi_dir)?;
    let path = roots.previous_handlers_path();
    let text = toml::to_string_pretty(handlers).map_err(|e| EasiError::Desktop(e.to_string()))?;
    fs::write(&path, text)?;
    Ok(path)
}

/// Register `easi.desktop`, remember previous default handlers for supported MIME types,
/// and set `easi.desktop` as the default handler.
pub fn run_setup(roots: &InstallRoots, easi_bin: &Path) -> Result<SavedHandlers, EasiError> {
    fs::create_dir_all(&roots.desktop_dir)?;
    let desktop_path = roots.desktop_dir.join(EASI_DESKTOP_ID);
    let mimes = SUPPORTED_MIMES.join(";");
    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=easi\n\
         Comment=Install downloaded Linux apps\n\
         Exec={} %f\n\
         Terminal=false\n\
         NoDisplay=true\n\
         MimeType={mimes};\n",
        easi_bin.to_string_lossy()
    );
    fs::write(&desktop_path, content)?;

    let mut saved = load_saved_handlers(roots);
    for mime in SUPPORTED_MIMES {
        if let Ok(out) = Command::new("xdg-mime")
            .args(["query", "default", mime])
            .output()
            && out.status.success()
        {
            let current = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !current.is_empty() && current != EASI_DESKTOP_ID {
                saved.defaults.entry((*mime).to_string()).or_insert(current);
            }
        }
        let _ = Command::new("xdg-mime")
            .args(["default", EASI_DESKTOP_ID, mime])
            .output();
    }

    save_handlers(roots, &saved)?;
    refresh_desktop_caches(roots);
    Ok(saved)
}

/// Query the MIME type of `path` and silently hand it off to the remembered previous handler.
pub fn handoff_non_app(roots: &InstallRoots, path: &Path) -> Result<(), EasiError> {
    let saved = load_saved_handlers(roots);
    let mime = query_file_mime(path);
    if let Some(m) = mime.as_deref()
        && let Some(desktop_id) = saved.defaults.get(m)
        && desktop_id != EASI_DESKTOP_ID
    {
        if Command::new("gtk-launch")
            .arg(desktop_id)
            .arg(path)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        if Command::new("gio")
            .args(["launch"])
            .arg(desktop_id)
            .arg(path)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
    }

    // Fallback: try common archive managers directly to avoid looping back into easi.
    for fallback in ["file-roller", "ark", "engrampa", "xarchiver"] {
        if Command::new(fallback).arg(path).spawn().is_ok() {
            return Ok(());
        }
    }

    Err(EasiError::Desktop(format!(
        "no previous archive handler found for {}",
        path.display()
    )))
}

fn query_file_mime(path: &Path) -> Option<String> {
    let out = Command::new("xdg-mime")
        .args(["query", "filetype"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mime = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if mime.is_empty() { None } else { Some(mime) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn saved_handlers_roundtrip() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        let mut saved = SavedHandlers::default();
        saved.defaults.insert(
            "application/zip".into(),
            "org.gnome.FileRoller.desktop".into(),
        );
        save_handlers(&roots, &saved).expect("save");
        let loaded = load_saved_handlers(&roots);
        assert_eq!(
            loaded.defaults.get("application/zip").map(String::as_str),
            Some("org.gnome.FileRoller.desktop")
        );
    }
}
