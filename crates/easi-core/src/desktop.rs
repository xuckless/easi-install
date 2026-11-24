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
const SYSTEMD_TIMER_NAME: &str = "easi-check-updates.timer";
const SYSTEMD_SERVICE_NAME: &str = "easi-check-updates.service";

/// Check whether the optional `systemd --user` background update-check timer is installed.
pub fn is_background_update_timer_enabled(roots: &InstallRoots) -> bool {
    roots
        .home_dir
        .join(".config/systemd/user")
        .join(SYSTEMD_TIMER_NAME)
        .exists()
}

/// Enable or disable the lightweight `systemd --user` daily update-check timer (0 MB RAM when idle).
pub fn set_background_update_timer(
    roots: &InstallRoots,
    easi_bin: &Path,
    enabled: bool,
) -> Result<bool, EasiError> {
    let unit_dir = roots.home_dir.join(".config/systemd/user");
    let service_path = unit_dir.join(SYSTEMD_SERVICE_NAME);
    let timer_path = unit_dir.join(SYSTEMD_TIMER_NAME);

    if enabled {
        fs::create_dir_all(&unit_dir)?;
        let exec = easi_bin.to_string_lossy();
        let service = format!(
            "[Unit]\n\
             Description=Check upstream updates for easi-managed apps\n\
             After=network-online.target\n\n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart=\"{exec}\" check-updates\n"
        );
        let timer = "[Unit]\n\
             Description=Daily easi app update check\n\n\
             [Timer]\n\
             OnCalendar=daily\n\
             RandomizedDelaySec=15m\n\
             Persistent=true\n\n\
             [Install]\n\
             WantedBy=timers.target\n";
        fs::write(&service_path, service)?;
        fs::write(&timer_path, timer)?;

        let out = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        if out.is_err() {
            return Err(EasiError::Desktop(
                "systemd user session not available on this distro; unit files were saved but timer could not be enabled.".into(),
            ));
        }
        let enable = Command::new("systemctl")
            .args(["--user", "enable", "--now", SYSTEMD_TIMER_NAME])
            .output()
            .map_err(|e| EasiError::Desktop(format!("systemctl failed: {e}")))?;
        if !enable.status.success() {
            return Err(EasiError::Desktop(
                "systemctl --user enable failed (is systemd --user running?).".into(),
            ));
        }
        Ok(true)
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", SYSTEMD_TIMER_NAME])
            .output();
        let _ = fs::remove_file(&timer_path);
        let _ = fs::remove_file(&service_path);
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        Ok(false)
    }
}

/// MIME types easi claims as default double-click handler on `easi --setup`.
/// Only AppImages are claimed by default (an AppImage is always an app, never photos/source code).
pub const SUPPORTED_MIMES: &[&str] =
    &["application/x-iso9660-appimage", "application/vnd.appimage"];

/// MIME types advertised in `easi.desktop` for "Right-Click -> Open With -> easi" ONLY by default,
/// so double-clicking random `.zip`/`.tar.*`/`.sh` files never hijacks the user's archive manager
/// or text editor. Users can opt into double-click archive handling via `easi --setup --claim-archives`.
pub const OPEN_WITH_ONLY_MIMES: &[&str] = &[
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

const EASI_ICON_SVG: &str = include_str!("../../../assets/easi.svg");
const EASI_SYMBOLIC_SVG: &str = include_str!("../../../assets/easi-symbolic.svg");

/// Ensure `easi`'s own Drop SVG icon is installed and any existing `easi.desktop` has
/// `Icon=easi` and `StartupWMClass=easi` (called on GUI startup and after `self-update`).
pub fn ensure_easi_desktop_and_icon(roots: &InstallRoots, easi_bin: &Path) {
    let icon_dir = roots.icons_dir.join("scalable/apps");
    let icon_path = icon_dir.join("easi.svg");
    let sym_path = icon_dir.join("easi-symbolic.svg");
    let mut changed = false;

    if fs::create_dir_all(&icon_dir).is_ok() {
        if fs::read_to_string(&icon_path).ok().as_deref() != Some(EASI_ICON_SVG)
            && fs::write(&icon_path, EASI_ICON_SVG).is_ok()
        {
            changed = true;
        }
        if fs::read_to_string(&sym_path).ok().as_deref() != Some(EASI_SYMBOLIC_SVG)
            && fs::write(&sym_path, EASI_SYMBOLIC_SVG).is_ok()
        {
            changed = true;
        }
    }

    let desktop_path = roots.desktop_dir.join(EASI_DESKTOP_ID);
    let expected_icon_line = format!("Icon={}", icon_path.display());
    let needs_desktop = match fs::read_to_string(&desktop_path) {
        Ok(text) => !text.contains(&expected_icon_line) || !text.contains("StartupWMClass=easi"),
        Err(_) => true,
    };
    if needs_desktop && fs::create_dir_all(&roots.desktop_dir).is_ok() {
        let mimes = SUPPORTED_MIMES
            .iter()
            .chain(OPEN_WITH_ONLY_MIMES.iter())
            .copied()
            .collect::<Vec<_>>()
            .join(";");
        let exec_str = easi_bin.to_string_lossy();
        let exec_quoted = if exec_str.contains(' ') {
            format!("\"{exec_str}\"")
        } else {
            exec_str.into_owned()
        };
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=easi\n\
             GenericName=App Installer\n\
             Comment=Install downloaded Linux apps\n\
             {expected_icon_line}\n\
             Exec={exec_quoted} %f\n\
             Terminal=false\n\
             StartupNotify=true\n\
             StartupWMClass=easi\n\
             Categories=System;Utility;\n\
             MimeType={mimes};\n"
        );
        if fs::write(&desktop_path, content).is_ok() {
            changed = true;
        }
    }

    if changed {
        refresh_desktop_caches(roots);
    }
}

/// Register `easi.desktop`, install the Drop brand SVG icon, claim AppImages as default
/// double-click handler, and advertise archives/scripts under Right-Click -> Open With.
pub fn run_setup(roots: &InstallRoots, easi_bin: &Path) -> Result<SavedHandlers, EasiError> {
    run_setup_with_options(roots, easi_bin, false)
}

/// Same as `run_setup`, with opt-in `claim_archives` to also claim `.zip`/`.tar.*`/`.7z`/`.jar`
/// as default double-click handlers. When `claim_archives` is false, any previously claimed
/// archive handlers are automatically restored to the user's archive manager.
pub fn run_setup_with_options(
    roots: &InstallRoots,
    easi_bin: &Path,
    claim_archives: bool,
) -> Result<SavedHandlers, EasiError> {
    fs::create_dir_all(&roots.desktop_dir)?;
    let icon_dir = roots.icons_dir.join("scalable/apps");
    let icon_path = icon_dir.join("easi.svg");
    if fs::create_dir_all(&icon_dir).is_ok() {
        let _ = fs::write(&icon_path, EASI_ICON_SVG);
        let _ = fs::write(icon_dir.join("easi-symbolic.svg"), EASI_SYMBOLIC_SVG);
    }
    let desktop_path = roots.desktop_dir.join(EASI_DESKTOP_ID);
    let mimes = SUPPORTED_MIMES
        .iter()
        .chain(OPEN_WITH_ONLY_MIMES.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(";");
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
         Icon={}\n\
         Exec={exec_quoted} %f\n\
         Terminal=false\n\
         StartupNotify=true\n\
         StartupWMClass=easi\n\
         Categories=System;Utility;\n\
         MimeType={mimes};\n",
        icon_path.display()
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
    let mimes_to_claim: Vec<&str> = if claim_archives {
        SUPPORTED_MIMES
            .iter()
            .chain(
                OPEN_WITH_ONLY_MIMES
                    .iter()
                    .filter(|m| **m != "application/x-shellscript"),
            )
            .copied()
            .collect()
    } else {
        // Restore any archive MIME types that an earlier version of easi may have claimed.
        for mime in OPEN_WITH_ONLY_MIMES {
            if let Some(prev) = saved.defaults.get(*mime)
                && !prev.is_empty()
                && prev != EASI_DESKTOP_ID
            {
                let _ = Command::new("xdg-mime")
                    .args(["default", prev, mime])
                    .output();
            }
        }
        SUPPORTED_MIMES.to_vec()
    };

    for mime in mimes_to_claim {
        if let Ok(out) = Command::new("xdg-mime")
            .args(["query", "default", mime])
            .output()
            && out.status.success()
        {
            let current = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !current.is_empty() && current != EASI_DESKTOP_ID {
                saved.defaults.entry(mime.to_string()).or_insert(current);
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

/// Launch an installed app (prefers `gtk-launch <app_id>.desktop` for GUI apps,
/// falls back to the `~/.local/bin/<slug>` symlink).
pub fn launch_installed_app(roots: &InstallRoots, query: &str) -> Result<(), EasiError> {
    let m = crate::manifest::resolve_installed(roots, query)?;
    let desktop_id = format!("{}.desktop", m.app_id);
    if roots.desktop_dir.join(&desktop_id).exists()
        && Command::new("gtk-launch").arg(&desktop_id).spawn().is_ok()
    {
        return Ok(());
    }
    if let Some(bin) = m
        .created_files
        .iter()
        .find(|p| p.starts_with(&roots.bin_dir))
        && Command::new(bin).spawn().is_ok()
    {
        return Ok(());
    }
    Err(EasiError::Desktop(format!("could not launch {}", m.name)))
}

/// Open an installed app's directory (`~/Applications/<App>/`) in the user's file manager.
pub fn open_app_folder(roots: &InstallRoots, query: &str) -> Result<(), EasiError> {
    let m = crate::manifest::resolve_installed(roots, query)?;
    Command::new("xdg-open")
        .arg(&m.install_dir)
        .spawn()
        .map_err(|e| EasiError::Desktop(format!("failed to open folder: {e}")))?;
    Ok(())
}

/// Copy `text` to the Linux Wayland/X11 clipboard using `wl-copy`, `xclip`, or `xsel`.
pub fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    let tools: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (bin, args) in tools {
        if let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            && let Some(mut stdin) = child.stdin.take()
            && stdin.write_all(text.as_bytes()).is_ok()
        {
            drop(stdin);
            let _ = child.wait();
            return true;
        }
    }
    false
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
