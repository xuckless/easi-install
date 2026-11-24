//! Reconcile installed app manifests after apps self-update in place inside `~/Applications/<App>/`.
//! Re-indexes files/dirs, strips stray setuid bits, refreshes version metadata, and repairs
//! renamed binary/AppImage wiring (`.desktop` `Exec=` and `~/.local/bin` symlink).

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::appid::slug_and_version;
use crate::error::EasiError;
use crate::manifest::{Manifest, list_manifests, save_manifest};
use crate::roots::InstallRoots;
use crate::self_update::extract_json_string_field;

/// Reconcile one manifest against disk. Saves updated manifest if anything drifted.
/// Returns `Ok(true)` if the manifest or wiring was updated.
pub fn reconcile_manifest(
    roots: &InstallRoots,
    manifest: &mut Manifest,
) -> Result<bool, EasiError> {
    if !manifest.install_dir.exists() {
        return Ok(false);
    }

    // Safety guard: never scan outside `~/Applications`.
    let canon_apps = match fs::canonicalize(&roots.applications_dir) {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };
    let canon_dir = fs::canonicalize(&manifest.install_dir)?;
    if !canon_dir.starts_with(&canon_apps) || canon_dir == canon_apps {
        return Ok(false);
    }

    let mut changed = false;

    // 1. Walk owned install_dir, strip stray setuid/setgid bits, collect files + dirs.
    let mut dir_files = Vec::new();
    let mut dir_dirs = vec![manifest.install_dir.clone()];
    scan_install_dir(&manifest.install_dir, &mut dir_files, &mut dir_dirs);
    dir_files.sort();
    dir_dirs.sort();

    // Preserve tracked files outside install_dir (.desktop, hicolor icons, ~/.local/bin symlink).
    let mut external_files: Vec<PathBuf> = manifest
        .created_files
        .iter()
        .filter(|p| !p.starts_with(&manifest.install_dir))
        .cloned()
        .collect();
    external_files.sort();

    let mut new_created_files = dir_files.clone();
    new_created_files.extend(external_files.iter().cloned());

    if new_created_files != manifest.created_files {
        manifest.created_files = new_created_files;
        changed = true;
    }
    if dir_dirs != manifest.created_dirs {
        manifest.created_dirs = dir_dirs;
        changed = true;
    }

    // 2. Refresh version if self-updater updated `version`, `package.json`, or AppImage name.
    if let Some(detected_ver) = detect_installed_version(&manifest.install_dir, &dir_files)
        && manifest.version.as_deref() != Some(detected_ver.as_str())
    {
        manifest.version = Some(detected_ver);
        changed = true;
    }

    // 3. Repair broken ~/.local/bin symlink and .desktop Exec= if binary/AppImage was renamed.
    if repair_renamed_binary_wiring(roots, &manifest.install_dir, &dir_files, &external_files) {
        changed = true;
    }

    if changed {
        save_manifest(roots, manifest)?;
    }
    Ok(changed)
}

/// Update `StartupWMClass=` in an installed app's `.desktop` file so GNOME/KDE dock icons
/// group properly without duplicates.
pub fn set_startup_wm_class(
    roots: &InstallRoots,
    query: &str,
    wm_class: &str,
) -> Result<PathBuf, EasiError> {
    let manifest = crate::manifest::resolve_installed(roots, query)?;
    let desktop_path = manifest
        .created_files
        .iter()
        .find(|p| {
            p.starts_with(&roots.desktop_dir)
                && p.extension().and_then(|s| s.to_str()) == Some("desktop")
        })
        .cloned()
        .ok_or_else(|| {
            EasiError::Desktop(format!("{} has no .desktop file to update", manifest.name))
        })?;
    let text = fs::read_to_string(&desktop_path)?;
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("StartupWMClass=") {
                found = true;
                format!("StartupWMClass={wm_class}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("StartupWMClass={wm_class}"));
    }
    fs::write(&desktop_path, lines.join("\n") + "\n")?;
    crate::install::refresh_desktop_caches(roots);
    Ok(desktop_path)
}

/// Detect the active window's WM_CLASS via `xprop` (X11/XWayland) when available,
/// or fall back to an explicit class passed by the caller.
pub fn probe_active_wm_class() -> Option<String> {
    let out = std::process::Command::new("xprop")
        .args(["-root", "_NET_ACTIVE_WINDOW"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root_text = String::from_utf8_lossy(&out.stdout);
    let win_id = root_text.split_whitespace().last()?;
    if !win_id.starts_with("0x") {
        return None;
    }
    let cls_out = std::process::Command::new("xprop")
        .args(["-id", win_id, "WM_CLASS"])
        .output()
        .ok()?;
    if !cls_out.status.success() {
        return None;
    }
    // WM_CLASS(STRING) = "instance", "ClassName"
    let cls_text = String::from_utf8_lossy(&cls_out.stdout);
    let last_quote = cls_text.rfind('"')?;
    let prev_quote = cls_text[..last_quote].rfind('"')?;
    let wm_class = cls_text[prev_quote + 1..last_quote].trim();
    if wm_class.is_empty() {
        None
    } else {
        Some(wm_class.to_string())
    }
}

/// Reconcile every installed manifest and return the refreshed list.
pub fn reconcile_all(roots: &InstallRoots) -> Result<Vec<Manifest>, EasiError> {
    let mut manifests = list_manifests(roots)?;
    for m in &mut manifests {
        let _ = reconcile_manifest(roots, m);
    }
    Ok(manifests)
}

fn scan_install_dir(dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            files.push(path);
        } else if meta.is_dir() {
            dirs.push(path.clone());
            scan_install_dir(&path, files, dirs);
        } else {
            let mode = meta.permissions().mode();
            if (mode & 0o6000) != 0 {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode & !0o6000));
            }
            files.push(path);
        }
    }
}

/// Detect version from common self-updated app files inside `install_dir`.
fn detect_installed_version(install_dir: &Path, files: &[PathBuf]) -> Option<String> {
    // 1. Plain `version` file (Electron / Antigravity / VS Code style)
    let version_file = install_dir.join("version");
    if let Ok(text) = fs::read_to_string(&version_file)
        && let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty())
        && line
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == 'v')
    {
        return Some(line.trim_start_matches('v').to_string());
    }

    // 2. `package.json` at root or `resources/app/package.json`
    for rel in ["package.json", "resources/app/package.json"] {
        if let Ok(json) = fs::read_to_string(install_dir.join(rel))
            && let Some(ver) = extract_json_string_field(&json, "version")
            && !ver.is_empty()
        {
            return Some(ver);
        }
    }

    // 3. Renamed `.AppImage` filename inside `install_dir`
    for f in files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if name.ends_with(".AppImage") || name.ends_with(".appimage") {
            let (_, ver) = slug_and_version(&name);
            if ver.is_some() {
                return ver;
            }
        }
    }

    None
}

fn repair_renamed_binary_wiring(
    roots: &InstallRoots,
    install_dir: &Path,
    dir_files: &[PathBuf],
    external_files: &[PathBuf],
) -> bool {
    // Find the tracked symlink in ~/.local/bin
    let Some(bin_link) = external_files
        .iter()
        .find(|p| p.starts_with(&roots.bin_dir))
    else {
        return false;
    };
    let Ok(target) = fs::read_link(bin_link) else {
        return false;
    };

    // If symlink points to an easi FUSE2 wrapper (`*-run.sh`), check whether the AppImage it
    // references was renamed in place and rewrite the wrapper if needed.
    if target.exists()
        && target
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.ends_with("-run.sh"))
        && let Ok(script) = fs::read_to_string(&target)
        && let Some(new_ai) = dir_files.iter().find(|f| {
            let n = f
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            n.ends_with(".AppImage") || n.ends_with(".appimage")
        })
    {
        let new_ai_str = new_ai.to_string_lossy();
        if !script.contains(new_ai_str.as_ref()) {
            let updated = format!(
                "#!/bin/sh\nexport APPIMAGE_EXTRACT_AND_RUN=1\nexec \"{new_ai_str}\" \"$@\"\n"
            );
            let _ = fs::write(&target, updated);
            return true;
        }
        return false;
    }

    if target.exists() {
        return false;
    }

    // Symlink target disappeared (e.g. AppImage updater renamed Foo-1.0.AppImage -> Foo-1.1.AppImage).
    let replacement = dir_files
        .iter()
        .find(|f| {
            let n = f
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            n.ends_with(".AppImage") || n.ends_with(".appimage")
        })
        .or_else(|| {
            dir_files.iter().find(|f| {
                f.parent() == Some(install_dir)
                    && fs::metadata(f)
                        .map(|m| (m.permissions().mode() & 0o111) != 0)
                        .unwrap_or(false)
            })
        });

    let Some(new_exec) = replacement else {
        return false;
    };

    if let Ok(meta) = fs::metadata(new_exec) {
        let mode = (meta.permissions().mode() & !0o6000) | 0o755;
        let _ = fs::set_permissions(new_exec, fs::Permissions::from_mode(mode));
    }

    let _ = fs::remove_file(bin_link);
    if symlink(new_exec, bin_link).is_err() {
        return false;
    }

    // Rewrite Exec= in the tracked .desktop file if present.
    if let Some(desktop_path) = external_files.iter().find(|p| {
        p.starts_with(&roots.desktop_dir)
            && p.extension().and_then(|s| s.to_str()) == Some("desktop")
    }) && let Ok(text) = fs::read_to_string(desktop_path)
    {
        let exec_str = new_exec.to_string_lossy();
        let exec_quoted = if exec_str.contains(' ') {
            format!("\"{exec_str}\"")
        } else {
            exec_str.into_owned()
        };
        let updated: String = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("Exec=") {
                    format!("Exec={exec_quoted}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let _ = fs::write(desktop_path, updated);
    }

    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use tempfile::TempDir;

    use super::*;
    use crate::inspect::classify_package;
    use crate::install::execute_install;
    use crate::manifest::find_manifest_by_id;
    use crate::model::Classified;

    #[test]
    fn reconciles_self_updated_app_files_version_perms_and_renamed_appimage() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        let dl = tmp.path().join("Downloads");
        fs::create_dir_all(&dl).expect("mkdir");

        // Install a synthetic AppImage v1.0.0
        let mut header = [0u8; 64];
        header[0..4].copy_from_slice(b"\x7fELF");
        header[4] = 2;
        header[5] = 1;
        header[8..11].copy_from_slice(b"AI\x01");
        let pkg = dl.join("MyApp-1.0.0.AppImage");
        fs::write(&pkg, header).expect("write appimage");

        let Classified::Ready { plan, .. } = classify_package(&pkg, |_| None).expect("classify")
        else {
            panic!("expected Ready");
        };
        let summary = execute_install(&roots, &plan, false, &AtomicBool::new(false), |_, _| {})
            .expect("install");

        // Simulate the app's own self-updater replacing files inside ~/Applications/Myapp/:
        // 1. Rename AppImage to 2.0.0
        let old_ai = summary.install_dir.join("MyApp-1.0.0.AppImage");
        let new_ai = summary.install_dir.join("MyApp-2.0.0.AppImage");
        fs::rename(&old_ai, &new_ai).expect("rename appimage");
        // 2. Drop a brand new helper file with a stray setuid bit
        let extra = summary.install_dir.join("cdn_update_payload.bin");
        fs::write(&extra, b"new-cdn-data").expect("write extra");
        fs::set_permissions(&extra, fs::Permissions::from_mode(0o4755)).expect("setuid");
        // 3. Write a `version` file
        fs::write(summary.install_dir.join("version"), "2.0.0\n").expect("write version");

        // Reconcile!
        let mut manifest = find_manifest_by_id(&roots, &summary.app_id).expect("manifest");
        let changed = reconcile_manifest(&roots, &mut manifest).expect("reconcile");
        assert!(changed);
        assert_eq!(manifest.version.as_deref(), Some("2.0.0"));
        assert!(manifest.created_files.contains(&extra));
        assert_eq!(
            fs::metadata(&extra).expect("meta").permissions().mode() & 0o6000,
            0,
            "stray setuid must be stripped"
        );

        // Verify symlink / FUSE2 wrapper and .desktop now point to 2.0.0 AppImage
        let link_target = fs::read_link(roots.bin_dir.join("myapp")).expect("readlink");
        if link_target.to_string_lossy().ends_with("-run.sh") {
            let wrapper_text = fs::read_to_string(&link_target).expect("read wrapper");
            assert!(wrapper_text.contains("MyApp-2.0.0.AppImage"));
        } else {
            assert_eq!(link_target, new_ai);
            let desktop_text =
                fs::read_to_string(roots.desktop_dir.join("easi.myapp.desktop")).expect("desktop");
            assert!(desktop_text.contains("MyApp-2.0.0.AppImage"));
        }

        // Test set_startup_wm_class updates .desktop cleanly
        let dp =
            set_startup_wm_class(&roots, &summary.app_id, "MyCustomWMClass").expect("set wmclass");
        assert!(
            fs::read_to_string(&dp)
                .expect("read desktop")
                .contains("StartupWMClass=MyCustomWMClass")
        );

        // Clean uninstall with purge_data removes app data dirs too!
        let cfg_dir = roots.home_dir.join(".config/myapp");
        fs::create_dir_all(&cfg_dir).expect("mkdir cfg");
        fs::write(cfg_dir.join("prefs.json"), "{}").expect("write prefs");
        crate::install::uninstall_app_with_options(&roots, &summary.app_id, true)
            .expect("uninstall");
        assert!(
            !extra.exists(),
            "self-updated file must be removed on uninstall"
        );
        assert!(!cfg_dir.exists(), "purged config dir must be removed");
        assert!(!new_ai.exists());
        assert!(!summary.install_dir.exists());
    }
}
