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
    "application/x-tar",
    "application/x-compressed-tar",
    "application/x-xz-compressed-tar",
    "application/x-zstd-compressed-tar",
    "application/x-bzip-compressed-tar",
    "application/x-bzip2-compressed-tar",
    "application/x-lz4-compressed-tar",
    "application/zip",
    "application/x-7z-compressed",
    "application/java-archive",
    "application/x-shellscript",
];

const APPIMAGE_MIME_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/vnd.appimage">
    <comment>AppImage application bundle</comment>
    <sub-class-of type="application/x-executable"/>
    <glob pattern="*.AppImage"/>
    <glob pattern="*.appimage"/>
    <magic priority="60">
      <match type="string" offset="8" value="AI\x02"/>
      <match type="string" offset="8" value="AI\x01"/>
    </magic>
  </mime-type>
</mime-info>
"#;

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

/// Register `easi.desktop`, ensure AppImage MIME definition exists, remember previous default
/// handlers for supported MIME types, and set `easi.desktop` as the default handler.
pub fn run_setup(roots: &InstallRoots, easi_bin: &Path) -> Result<SavedHandlers, EasiError> {
    fs::create_dir_all(&roots.desktop_dir)?;
    let desktop_path = roots.desktop_dir.join(EASI_DESKTOP_ID);
    let mimes = SUPPORTED_MIMES.join(";");
    let exec_str = easi_bin.to_string_lossy();
    let exec_quoted = if exec_str.contains(' ') {
        format!("\"{exec_str}\"")
    } else {
        exec_str.into_owned()
    };
    // Omit NoDisplay=true so file managers (GNOME Files / Dolphin) always show easi
    // in "Right-Click -> Open With" in addition to using it as the default handler.
    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=easi\n\
         GenericName=App Installer\n\
         Comment=Install downloaded Linux apps\n\
         Exec={exec_quoted} %f\n\
         Terminal=false\n\
         StartupNotify=true\n\
         Categories=System;Utility;\n\
         MimeType={mimes};\n"
    );
    fs::write(&desktop_path, content)?;

    // Install AppImage MIME XML for distros that don't ship shared-mime-info rules for it.
    let mime_root = roots.home_dir.join(".local/share/mime");
    let mime_packages = mime_root.join("packages");
    if fs::create_dir_all(&mime_packages).is_ok()
        && fs::write(mime_packages.join("easi-appimage.xml"), APPIMAGE_MIME_XML).is_ok()
    {
        let _ = Command::new("update-mime-database")
            .arg(&mime_root)
            .output();
    }

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

/// Undo `easi --setup`: restore all remembered previous default MIME handlers and remove
/// `easi.desktop` + `easi-appimage.xml`.
pub fn restore_handlers(roots: &InstallRoots) -> Result<usize, EasiError> {
    let saved = load_saved_handlers(roots);
    let mut restored = 0usize;
    for (mime, prev_desktop) in &saved.defaults {
        if !prev_desktop.is_empty() && prev_desktop != EASI_DESKTOP_ID {
            let _ = Command::new("xdg-mime")
                .args(["default", prev_desktop, mime])
                .output();
            restored += 1;
        }
    }
    let desktop_path = roots.desktop_dir.join(EASI_DESKTOP_ID);
    if desktop_path.exists() {
        let _ = fs::remove_file(&desktop_path);
    }
    let mime_xml = roots
        .home_dir
        .join(".local/share/mime/packages/easi-appimage.xml");
    if mime_xml.exists() {
        let _ = fs::remove_file(&mime_xml);
        let _ = Command::new("update-mime-database")
            .arg(roots.home_dir.join(".local/share/mime"))
            .output();
    }
    let prev_path = roots.previous_handlers_path();
    if prev_path.exists() {
        let _ = fs::remove_file(&prev_path);
    }
    refresh_desktop_caches(roots);
    Ok(restored)
}

/// Cleanly uninstall `easi` itself: restore previous MIME handlers, remove `easi` binary
/// from `~/.local/bin/easi`, and remove `easi` config/state files (keeps user's installed apps
/// in `~/Applications/` untouched).
pub fn self_uninstall(roots: &InstallRoots) -> Result<usize, EasiError> {
    let restored = restore_handlers(roots)?;
    let bin_path = roots.bin_dir.join("easi");
    if bin_path.exists() || bin_path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&bin_path);
    }
    let cfg_path = roots.config_path();
    if cfg_path.exists() {
        let _ = fs::remove_file(&cfg_path);
    }
    Ok(restored)
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

    // Fallback: try common archive/file managers directly (GNOME, KDE, Mint, COSMIC)
    // to avoid looping back into easi.
    for fallback in [
        "file-roller",
        "ark",
        "engrampa",
        "xarchiver",
        "nautilus",
        "dolphin",
        "nemo",
        "caja",
        "thunar",
        "cosmic-files",
        "pcmanfm",
        "pcmanfm-qt",
    ] {
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

/// Send a best-effort desktop notification (GNOME, KDE/SteamOS, Mint, CachyOS).
/// Silently skipped in tests, headless sessions, or when `EASI_NO_NOTIFY=1`.
pub fn notify(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    if std::env::var("EASI_NO_NOTIFY")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
    {
        return;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return;
    }
    if Command::new("notify-send")
        .args(["-a", "easi", title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
    {
        return;
    }
    let _ = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "easi",
            "0",
            "",
            title,
            body,
            "[]",
            "{}",
            "5000",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
