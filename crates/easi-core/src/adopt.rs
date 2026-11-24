//! Discover, adopt, and repair existing non-package-manager user apps in `$HOME`.
//! Strictly excludes distro packages (`/usr/`), Flatpak, Snap, Steam games, and Wine handlers.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::appid::{
    display_name_from_raw, display_name_from_slug, guess_startup_wm_class, slug_and_version,
};
use crate::detect::{ArchiveEntry, detect_file_format, inspect_contents};
use crate::error::EasiError;
use crate::inspect::{desktop_field, score_icons};
use crate::install::refresh_desktop_caches;
use crate::manifest::{
    Manifest, delete_manifest, dir_size_bytes, list_manifests, resolve_installed, save_manifest,
};
use crate::model::{AppKind, Format};
use crate::roots::{InstallRoots, safe_join};
use crate::self_update::extract_json_string_field;

/// `Exec=` substrings that indicate a system, distro, Flatpak, Snap, Nix, Steam, or Wine app
/// that `easi` must NEVER adopt.
const EXCLUDED_EXEC_NEEDLES: &[&str] = &[
    "flatpak ",
    "/var/lib/flatpak/",
    "/.local/share/flatpak/",
    "/snap/",
    "snap run",
    "/nix/store/",
    "/gnu/store/",
    "/app/bin/",
    "distrobox",
    "toolbox ",
    "steam://",
    "steam ",
    "umu-run",
    "wine ",
    "wine64 ",
    "env wine",
    "/usr/bin/",
    "/usr/sbin/",
    "/usr/libexec/",
];

/// `$HOME` subpaths owned by container/game/package/language-env managers that must never be adopted.
const EXCLUDED_HOME_SEGMENTS: &[&str] = &[
    "/.local/share/flatpak/",
    "/.var/app/",
    "/.local/share/Steam/",
    "/.steam/",
    "/.nix-profile/",
    "/.nix-defexpr/",
    "/.wine/",
    "/.local/share/lutris/",
    "/.local/share/Trash/",
    "/.cache/",
    "/.venv/",
    "/uv/tools/",
    "/uv/python/",
    "/node_modules/",
    "/.cargo/",
    "/.rustup/",
];

const EXCLUDED_DESKTOP_PREFIXES: &[&str] = &["wine-extension-", "easi.desktop"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppIssue {
    MissingIcon,
    BrokenExec,
    MissingDesktopEntry,
    MissingStartupWmClass,
    MissingBinSymlink,
}

impl AppIssue {
    pub fn label(self) -> &'static str {
        match self {
            Self::MissingIcon => "Missing icon",
            Self::BrokenExec => "Broken executable",
            Self::MissingDesktopEntry => "No launcher entry",
            Self::MissingStartupWmClass => "Missing StartupWMClass",
            Self::MissingBinSymlink => "No CLI symlink",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredApp {
    pub app_id: String,
    pub name: String,
    pub version: Option<String>,
    pub kind: AppKind,
    pub install_dir: PathBuf,
    pub exec_path: PathBuf,
    pub desktop_path: Option<PathBuf>,
    pub icon_value: Option<String>,
    pub size_bytes: u64,
    pub issues: Vec<AppIssue>,
}

/// Scan `$HOME` (`~/.local/share/applications/*.desktop` pointing inside `$HOME` + standalone
/// apps in `~/Applications/`) for unmanaged non-package-manager apps.
pub fn scan_unmanaged_apps(roots: &InstallRoots) -> Vec<DiscoveredApp> {
    let managed = list_manifests(roots).unwrap_or_default();
    let managed_ids: HashSet<String> = managed.iter().map(|m| m.app_id.clone()).collect();
    let managed_dirs: HashSet<PathBuf> = managed
        .iter()
        .filter_map(|m| fs::canonicalize(&m.install_dir).ok())
        .collect();
    let managed_desktops: HashSet<PathBuf> = managed
        .iter()
        .flat_map(|m| m.created_files.iter())
        .filter(|p| p.starts_with(&roots.desktop_dir))
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect();

    let Ok(canon_home) = fs::canonicalize(&roots.home_dir) else {
        return vec![];
    };
    let mut discovered = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_execs: HashSet<PathBuf> = HashSet::new();

    // 1. Scan user-owned ~/.local/share/applications/*.desktop entries pointing inside $HOME.
    if let Ok(entries) = fs::read_dir(&roots.desktop_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            let fname = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if EXCLUDED_DESKTOP_PREFIXES
                .iter()
                .any(|p| fname.starts_with(p))
            {
                continue;
            }
            if let Ok(canon_dp) = fs::canonicalize(&path)
                && managed_desktops.contains(&canon_dp)
            {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if desktop_field(&text, "NoDisplay").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
                continue;
            }
            let Some(exec_raw) = desktop_field(&text, "Exec") else {
                continue;
            };
            let exec_lower = exec_raw.to_ascii_lowercase();
            if EXCLUDED_EXEC_NEEDLES.iter().any(|n| exec_lower.contains(n)) {
                continue;
            }
            let Some(exec_path) = parse_exec_binary_path(&exec_raw, &roots.home_dir) else {
                continue;
            };
            // Must live inside $HOME and outside user-Flatpak/Steam/Nix/Wine/Cache dirs.
            let canon_exec = fs::canonicalize(&exec_path).unwrap_or_else(|_| exec_path.clone());
            if !canon_exec.starts_with(&canon_home) || is_excluded_home_path(&canon_exec) {
                continue;
            }
            let app_id = fname.trim_end_matches(".desktop").to_string();
            if managed_ids.contains(&app_id) {
                continue;
            }

            let install_dir = infer_install_dir(roots, &canon_exec);
            let canon_dir = fs::canonicalize(&install_dir).unwrap_or_else(|_| install_dir.clone());
            if managed_dirs.contains(&canon_dir) || seen_execs.contains(&canon_exec) {
                continue;
            }
            // Only deduplicate by install_dir when it is a dedicated app directory (not a single file).
            if canon_dir.is_dir() && !seen_dirs.insert(canon_dir) {
                continue;
            }
            seen_execs.insert(canon_exec.clone());

            let name =
                desktop_field(&text, "Name").unwrap_or_else(|| display_name_from_slug(&app_id));
            let terminal =
                desktop_field(&text, "Terminal").is_some_and(|v| v.eq_ignore_ascii_case("true"));
            let kind = if terminal {
                AppKind::Terminal
            } else {
                AppKind::Gui
            };
            let icon_value = desktop_field(&text, "Icon");
            let wm_class = desktop_field(&text, "StartupWMClass");
            let version = detect_dir_version(&install_dir, &canon_exec);

            let mut issues = Vec::new();
            if !exec_path.exists() {
                issues.push(AppIssue::BrokenExec);
            }
            if !is_icon_valid(roots, icon_value.as_deref()) {
                issues.push(AppIssue::MissingIcon);
            }
            if kind == AppKind::Gui && wm_class.is_none() {
                issues.push(AppIssue::MissingStartupWmClass);
            }
            let bin_slug = bin_slug_for_id(&app_id);
            if !roots.bin_dir.join(&bin_slug).exists() {
                issues.push(AppIssue::MissingBinSymlink);
            }

            let size_bytes = if install_dir.is_dir() {
                dir_size_bytes(&install_dir)
            } else {
                fs::metadata(&canon_exec).map(|m| m.len()).unwrap_or(0)
            };

            discovered.push(DiscoveredApp {
                app_id,
                name,
                version,
                kind,
                install_dir,
                exec_path: canon_exec,
                desktop_path: Some(path),
                icon_value,
                size_bytes,
                issues,
            });
        }
    }

    // 2. Scan ~/Applications/ for standalone AppImages or app directories without a .desktop entry.
    if let Ok(entries) = fs::read_dir(&roots.applications_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name().to_string_lossy().into_owned();
            if fname.starts_with('.') {
                continue;
            }
            let Ok(canon_path) = fs::canonicalize(&path) else {
                continue;
            };
            if managed_dirs.contains(&canon_path)
                || seen_dirs.contains(&canon_path)
                || seen_execs.contains(&canon_path)
            {
                continue;
            }
            if canon_path.is_file() {
                if let Some(app) = discover_from_file(&canon_path)
                    && !managed_ids.contains(&app.app_id)
                {
                    discovered.push(app);
                }
            } else if canon_path.is_dir()
                && let Some(app) = discover_from_dir(roots, &canon_path)
                && !managed_ids.contains(&app.app_id)
            {
                seen_dirs.insert(canon_path);
                discovered.push(app);
            }
        }
    }

    // 3. Scan ~/.local/bin/ for standalone user binaries, AppImages, and symlinks to $HOME apps
    // (e.g. agy, claude, uv, skink, zig, helium.AppImage) that don't already have a .desktop entry.
    if let Ok(entries) = fs::read_dir(&roots.bin_dir) {
        let managed_bin_files: HashSet<PathBuf> = managed
            .iter()
            .flat_map(|m| m.created_files.iter())
            .filter(|p| p.starts_with(&roots.bin_dir))
            .cloned()
            .collect();
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name().to_string_lossy().into_owned();
            if fname.starts_with('.')
                || fname == "easi"
                || fname.ends_with(".old")
                || fname.contains("(deleted)")
                || managed_bin_files.contains(&path)
            {
                continue;
            }
            let Ok(canon_exec) = fs::canonicalize(&path) else {
                continue;
            };
            if !canon_exec.is_file()
                || !canon_exec.starts_with(&canon_home)
                || is_excluded_home_path(&canon_exec)
                || seen_execs.contains(&canon_exec)
                || managed_dirs.contains(&canon_exec)
            {
                continue;
            }
            let install_dir = infer_install_dir(roots, &canon_exec);
            if install_dir.is_dir()
                && let Ok(canon_dir) = fs::canonicalize(&install_dir)
                && (managed_dirs.contains(&canon_dir) || seen_dirs.contains(&canon_dir))
            {
                continue;
            }
            let Ok(fmt) = detect_file_format(&canon_exec) else {
                continue;
            };
            if !matches!(
                fmt,
                Format::AppImageType1 | Format::AppImageType2 | Format::BareElf
            ) {
                continue;
            }
            let (slug, mut ver) = slug_and_version(&fname);
            if ver.is_none() {
                ver = detect_dir_version(&install_dir, &canon_exec);
            }
            let slug_clean = if slug.is_empty() { "app" } else { &slug };
            let app_id = format!("easi.{slug_clean}");
            if managed_ids.contains(&app_id) {
                continue;
            }
            seen_execs.insert(canon_exec.clone());
            let name = display_name_from_raw(&fname);
            let kind = if matches!(fmt, Format::AppImageType1 | Format::AppImageType2) {
                AppKind::Gui
            } else {
                AppKind::Terminal
            };
            let mut issues = Vec::new();
            if kind == AppKind::Gui {
                issues.push(AppIssue::MissingDesktopEntry);
                issues.push(AppIssue::MissingIcon);
            }
            let size_bytes = if install_dir.is_dir() {
                dir_size_bytes(&install_dir)
            } else {
                fs::metadata(&canon_exec).map(|m| m.len()).unwrap_or(0)
            };
            discovered.push(DiscoveredApp {
                app_id,
                name,
                version: ver,
                kind,
                install_dir,
                exec_path: canon_exec,
                desktop_path: None,
                icon_value: None,
                size_bytes,
                issues,
            });
        }
    }

    discovered.sort_by_key(|a| a.name.to_lowercase());
    discovered
}

/// Adopt an unmanaged app into `easi`'s manifest store without moving its files.
/// If `repair` is true, automatically fixes detected issues (missing icons, StartupWMClass, etc.).
pub fn adopt_app(
    roots: &InstallRoots,
    app: &DiscoveredApp,
    repair: bool,
) -> Result<Manifest, EasiError> {
    let mut created_files = Vec::new();
    let mut created_dirs = Vec::new();
    if app.install_dir.is_dir() {
        created_dirs.push(app.install_dir.clone());
        collect_tree(&app.install_dir, &mut created_files, &mut created_dirs);
    } else if app.install_dir.is_file() {
        created_files.push(app.install_dir.clone());
    }
    if let Some(dp) = &app.desktop_path
        && dp.exists()
        && !created_files.contains(dp)
    {
        created_files.push(dp.clone());
    }

    // Auto-detect upstream update source from AppImage .upd_info, on-disk config files,
    // or the curated KNOWN_APP_SOURCES table.
    let update_source = if app.exec_path.is_file()
        && let Ok(fmt) = detect_file_format(&app.exec_path)
        && fmt == Format::AppImageType2
        && let Ok(contents) = inspect_contents(&app.exec_path, fmt)
    {
        crate::upstream::detect_update_source(contents.appimage_upd_info.as_deref(), None, None)
    } else if app.install_dir.is_dir() {
        crate::upstream::detect_dir_update_source(&app.install_dir)
    } else {
        None
    }
    .or_else(|| crate::upstream::lookup_known_app_source(&app.app_id));

    let source_filename = app
        .exec_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.name.clone());

    let manifest = Manifest {
        app_id: app.app_id.clone(),
        name: app.name.clone(),
        version: app.version.clone(),
        source_filename,
        kind: app.kind,
        install_dir: app.install_dir.clone(),
        developer: None,
        origin_url: None,
        signature_summary: Some("Adopted from existing system install".into()),
        update_source,
        adopted: true,
        created_files,
        created_dirs,
    };
    save_manifest(roots, &manifest)?;

    if repair {
        let _ = repair_app(roots, &manifest.app_id);
        return resolve_installed(roots, &manifest.app_id);
    }
    Ok(manifest)
}

/// Adopt all discovered unmanaged apps in one go.
pub fn adopt_all(roots: &InstallRoots, repair: bool) -> Result<Vec<Manifest>, EasiError> {
    let discovered = scan_unmanaged_apps(roots);
    let mut adopted = Vec::new();
    for app in &discovered {
        if let Ok(m) = adopt_app(roots, app, repair) {
            adopted.push(m);
        }
    }
    Ok(adopted)
}

/// Repair broken/missing icon, `.desktop`, `StartupWMClass`, and `~/.local/bin` symlink
/// for any managed (or newly adopted) app. Returns a list of actions performed.
pub fn repair_app(roots: &InstallRoots, query: &str) -> Result<Vec<String>, EasiError> {
    let mut manifest = resolve_installed(roots, query)?;
    let mut fixes = Vec::new();

    // Find existing .desktop file if tracked.
    let mut desktop_path = manifest
        .created_files
        .iter()
        .find(|p| {
            p.starts_with(&roots.desktop_dir)
                && p.extension().and_then(|s| s.to_str()) == Some("desktop")
        })
        .cloned();

    let mut desktop_text = desktop_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok());

    // 1. Repair missing or broken icon for GUI apps.
    if manifest.kind == AppKind::Gui {
        let current_icon = desktop_text
            .as_deref()
            .and_then(|t| desktop_field(t, "Icon"));
        if !is_icon_valid(roots, current_icon.as_deref())
            && let Some((ext, bytes)) = find_best_icon_bytes(&manifest)
        {
            let subdir = if ext == "svg" {
                "scalable/apps"
            } else {
                "256x256/apps"
            };
            let dir = roots.icons_dir.join(subdir);
            fs::create_dir_all(&dir)?;
            let icon_dest = safe_join(&dir, Path::new(&format!("{}.{ext}", manifest.app_id)))?;
            fs::write(&icon_dest, bytes)?;
            let icon_abs = icon_dest.to_string_lossy().into_owned();
            if !manifest.created_files.contains(&icon_dest) {
                manifest.created_files.push(icon_dest);
            }
            if let Some(text) = desktop_text.take() {
                desktop_text = Some(upsert_desktop_key(&text, "Icon", &icon_abs));
            }
            fixes.push(format!(
                "Installed hicolor icon ({}.{ext})",
                manifest.app_id
            ));
        }
    }

    // 2. Ensure .desktop file exists for GUI apps (e.g. standalone AppImage in ~/Applications).
    if manifest.kind == AppKind::Gui && desktop_text.is_none() {
        fs::create_dir_all(&roots.desktop_dir)?;
        let dp = safe_join(
            &roots.desktop_dir,
            Path::new(&format!("{}.desktop", manifest.app_id)),
        )?;
        let exec_target = find_main_exec(&manifest);
        let exec_str = exec_target.to_string_lossy();
        let exec_quoted = if exec_str.contains(' ') {
            format!("\"{exec_str}\"")
        } else {
            exec_str.as_ref().to_string()
        };
        let wm =
            guess_startup_wm_class(&exec_str).unwrap_or_else(|| bin_slug_for_id(&manifest.app_id));
        let content = format!(
            "[Desktop Entry]\nType=Application\nName={}\nExec={exec_quoted}\nIcon={}\nTerminal=false\nStartupWMClass={wm}\n",
            manifest.name, manifest.app_id
        );
        desktop_text = Some(content);
        desktop_path = Some(dp.clone());
        if !manifest.created_files.contains(&dp) {
            manifest.created_files.push(dp);
        }
        fixes.push(format!("Created {}.desktop launcher", manifest.app_id));
    }

    // 3. Ensure StartupWMClass= is present on GUI .desktop entries.
    if manifest.kind == AppKind::Gui
        && let Some(text) = desktop_text.as_deref()
        && desktop_field(text, "StartupWMClass").is_none()
    {
        let exec_hint =
            desktop_field(text, "Exec").unwrap_or_else(|| manifest.source_filename.clone());
        let wm =
            guess_startup_wm_class(&exec_hint).unwrap_or_else(|| bin_slug_for_id(&manifest.app_id));
        desktop_text = Some(upsert_desktop_key(text, "StartupWMClass", &wm));
        fixes.push(format!("Added StartupWMClass={wm}"));
    }

    if let (Some(dp), Some(text)) = (&desktop_path, &desktop_text) {
        fs::write(dp, text)?;
    }

    // 4. Ensure ~/.local/bin/<slug> symlink exists (never overwrite unmanaged files).
    let bin_slug = bin_slug_for_id(&manifest.app_id);
    let link_path = roots.bin_dir.join(&bin_slug);
    if !link_path.exists() && link_path.symlink_metadata().is_err() {
        let exec_target = find_main_exec(&manifest);
        if exec_target.is_file() {
            let _ = fs::create_dir_all(&roots.bin_dir);
            if let Ok(meta) = fs::metadata(&exec_target) {
                let mode = (meta.permissions().mode() & !0o6000) | 0o755;
                let _ = fs::set_permissions(&exec_target, fs::Permissions::from_mode(mode));
            }
            if symlink(&exec_target, &link_path).is_ok() {
                manifest.created_files.push(link_path);
                fixes.push(format!("Linked ~/.local/bin/{bin_slug}"));
            }
        }
    }

    // 5. Auto-detect upstream update source from app config files or known-app table if not yet set.
    if manifest.update_source.is_none() {
        let detected = if manifest.install_dir.is_dir() {
            crate::upstream::detect_dir_update_source(&manifest.install_dir)
        } else {
            None
        }
        .or_else(|| crate::upstream::lookup_known_app_source(&manifest.app_id));
        if let Some(src) = detected {
            fixes.push(format!("Detected update source ({})", src.display_label()));
            manifest.update_source = Some(src);
        }
    }

    save_manifest(roots, &manifest)?;
    refresh_desktop_caches(roots);
    Ok(fixes)
}

/// Stop managing an app in `easi` without deleting any of its files on disk.
pub fn forget_app(roots: &InstallRoots, query: &str) -> Result<Manifest, EasiError> {
    let manifest = resolve_installed(roots, query)?;
    delete_manifest(roots, &manifest.app_id)?;
    Ok(manifest)
}

/// Move a managed app whose files live outside `~/Applications/` neatly into `~/Applications/<Name>/`,
/// rewriting its `.desktop` `Exec=` and `~/.local/bin/<slug>` symlink.
pub fn organize_app_into_applications(
    roots: &InstallRoots,
    query: &str,
) -> Result<Manifest, EasiError> {
    let mut manifest = resolve_installed(roots, query)?;
    let canon_apps = fs::canonicalize(&roots.applications_dir).unwrap_or_else(|_| {
        let _ = fs::create_dir_all(&roots.applications_dir);
        roots.applications_dir.clone()
    });
    let canon_old = fs::canonicalize(&manifest.install_dir)?;
    if canon_old.starts_with(&canon_apps) {
        return Ok(manifest);
    }

    fs::create_dir_all(&roots.applications_dir)?;
    let folder_name: String = manifest
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    let dest_dir = safe_join(
        &roots.applications_dir,
        Path::new(if folder_name.is_empty() {
            &manifest.app_id
        } else {
            &folder_name
        }),
    )?;
    if dest_dir.exists() {
        return Err(EasiError::UnmanagedOverwrite(dest_dir));
    }

    let old_str = canon_old.to_string_lossy().into_owned();
    let new_install_dir: PathBuf;
    let new_exec: PathBuf;

    if canon_old.is_file() {
        fs::create_dir_all(&dest_dir)?;
        let fname = canon_old
            .file_name()
            .ok_or_else(|| EasiError::Desktop("invalid source filename".into()))?;
        let dest_file = dest_dir.join(fname);
        fs::rename(&canon_old, &dest_file)?;
        new_install_dir = dest_dir.clone();
        new_exec = dest_file;
    } else if canon_old.is_dir() {
        fs::rename(&canon_old, &dest_dir)?;
        new_install_dir = dest_dir.clone();
        new_exec = find_best_exec_in_dir(&dest_dir, &bin_slug_for_id(&manifest.app_id))
            .unwrap_or_else(|| dest_dir.join(&manifest.source_filename));
    } else {
        return Err(EasiError::NotFound(canon_old));
    }

    let new_str = if canon_old.is_file() {
        new_exec.to_string_lossy().into_owned()
    } else {
        new_install_dir.to_string_lossy().into_owned()
    };

    // Rewrite tracked .desktop Exec= and Icon= paths that pointed to the old location.
    for path in &manifest.created_files {
        if path.starts_with(&roots.desktop_dir)
            && path.extension().and_then(|s| s.to_str()) == Some("desktop")
            && let Ok(text) = fs::read_to_string(path)
        {
            let updated = text.replace(&old_str, &new_str);
            if updated != text {
                let _ = fs::write(path, updated);
            }
        }
    }

    // Update ~/.local/bin/<slug> symlink to point to the organized executable.
    let bin_slug = bin_slug_for_id(&manifest.app_id);
    let link_path = roots.bin_dir.join(&bin_slug);
    let _ = fs::create_dir_all(&roots.bin_dir);
    if link_path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&link_path);
    }
    let _ = symlink(&new_exec, &link_path);

    // Refresh manifest paths.
    manifest.install_dir = new_install_dir.clone();
    manifest
        .created_files
        .retain(|p| !p.starts_with(&canon_old) && p != &canon_old);
    manifest
        .created_dirs
        .retain(|p| !p.starts_with(&canon_old) && p != &canon_old);
    manifest.created_dirs.push(new_install_dir.clone());
    collect_tree(
        &new_install_dir,
        &mut manifest.created_files,
        &mut manifest.created_dirs,
    );
    if !manifest.created_files.contains(&link_path) {
        manifest.created_files.push(link_path);
    }

    save_manifest(roots, &manifest)?;
    refresh_desktop_caches(roots);
    Ok(manifest)
}

/// Manually adopt an existing app directory or standalone AppImage/ELF chosen by the user.
/// Refuses system directories outside `$HOME`, `$HOME` itself, or package-manager directories.
pub fn adopt_path(roots: &InstallRoots, path: &Path, repair: bool) -> Result<Manifest, EasiError> {
    if !path.exists() {
        return Err(EasiError::NotFound(path.to_path_buf()));
    }
    let canon_home = fs::canonicalize(&roots.home_dir)?;
    let canon = fs::canonicalize(path)?;
    if !canon.starts_with(&canon_home)
        || canon == canon_home
        || is_excluded_home_path(&canon)
        || fs::canonicalize(&roots.applications_dir)
            .ok()
            .is_some_and(|a| a == canon)
    {
        return Err(EasiError::Desktop(
            "Choose a specific app folder or executable inside your home directory (not $HOME itself or system/Flatpak/Steam folders).".into(),
        ));
    }

    let discovered = if canon.is_dir() {
        discover_from_dir(roots, &canon).ok_or_else(|| {
            EasiError::Desktop(format!(
                "No executable program found inside {}.",
                canon.display()
            ))
        })?
    } else if canon.is_file() {
        discover_from_file(&canon).ok_or_else(|| {
            EasiError::Desktop(format!(
                "{} is not an AppImage or Linux executable.",
                canon.display()
            ))
        })?
    } else {
        return Err(EasiError::Desktop(
            "Selected path must be a folder or executable file.".into(),
        ));
    };

    adopt_app(roots, &discovered, repair)
}

fn is_excluded_home_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    EXCLUDED_HOME_SEGMENTS.iter().any(|seg| s.contains(seg))
}

fn discover_from_file(canon_path: &Path) -> Option<DiscoveredApp> {
    let fmt = detect_file_format(canon_path).ok()?;
    if !matches!(
        fmt,
        Format::AppImageType1 | Format::AppImageType2 | Format::BareElf
    ) {
        return None;
    }
    let fname = canon_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (slug, ver) = slug_and_version(&fname);
    let app_id = format!("easi.{}", if slug.is_empty() { "app" } else { &slug });
    let name = display_name_from_raw(&fname);
    let kind = if matches!(fmt, Format::AppImageType1 | Format::AppImageType2) {
        AppKind::Gui
    } else {
        AppKind::Terminal
    };
    let mut issues = vec![AppIssue::MissingDesktopEntry];
    if kind == AppKind::Gui {
        issues.push(AppIssue::MissingIcon);
    }
    Some(DiscoveredApp {
        app_id,
        name,
        version: ver,
        kind,
        install_dir: canon_path.to_path_buf(),
        exec_path: canon_path.to_path_buf(),
        desktop_path: None,
        icon_value: None,
        size_bytes: fs::metadata(canon_path).map(|m| m.len()).unwrap_or(0),
        issues,
    })
}

fn discover_from_dir(roots: &InstallRoots, canon_dir: &Path) -> Option<DiscoveredApp> {
    let dir_name = canon_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (slug, dir_ver) = slug_and_version(&dir_name);
    let slug_clean = if slug.is_empty() { "app" } else { &slug };
    let exec_path = find_best_exec_in_dir(canon_dir, slug_clean)?;
    let app_id = format!("easi.{slug_clean}");
    let name = display_name_from_raw(&dir_name);
    let version = detect_dir_version(canon_dir, &exec_path).or(dir_ver);

    let mut icon_entries = Vec::new();
    collect_icon_entries(canon_dir, canon_dir, 0, &mut icon_entries);
    let kind = if !icon_entries.is_empty() {
        AppKind::Gui
    } else {
        AppKind::Terminal
    };

    let mut issues = Vec::new();
    if kind == AppKind::Gui {
        issues.push(AppIssue::MissingDesktopEntry);
        issues.push(AppIssue::MissingIcon);
    }
    if !roots.bin_dir.join(slug_clean).exists() {
        issues.push(AppIssue::MissingBinSymlink);
    }

    Some(DiscoveredApp {
        app_id,
        name,
        version,
        kind,
        install_dir: canon_dir.to_path_buf(),
        exec_path,
        desktop_path: None,
        icon_value: None,
        size_bytes: dir_size_bytes(canon_dir),
        issues,
    })
}

/// Find the best main executable inside an existing app directory (checks root + `bin/`).
fn find_best_exec_in_dir(dir: &Path, slug: &str) -> Option<PathBuf> {
    let mut best: Option<(i32, PathBuf)> = None;
    let search_dirs = [dir.to_path_buf(), dir.join("bin")];
    for sub in &search_dirs {
        let Ok(entries) = fs::read_dir(sub) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(meta) = fs::metadata(&p) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.ends_with(".so")
                || name.contains(".so.")
                || name.ends_with(".png")
                || name.ends_with(".svg")
                || name.ends_with(".txt")
                || name.ends_with(".md")
                || name.ends_with(".json")
                || name.contains("uninstall")
                || name.contains("crashreporter")
                || name.contains("updater")
            {
                continue;
            }
            let is_exec_mode = (meta.permissions().mode() & 0o111) != 0;
            let is_elf = matches!(
                detect_file_format(&p),
                Ok(Format::BareElf | Format::AppImageType1 | Format::AppImageType2)
            );
            let is_launcher_script = is_exec_mode && (name.ends_with(".sh") || !name.contains('.'));
            if !is_elf && !is_launcher_script {
                continue;
            }
            let mut score = if is_elf { 30 } else { 15 };
            if sub == dir {
                score += 10;
            }
            let stem = name.trim_end_matches(".sh");
            if stem == slug {
                score += 50;
            } else if !slug.is_empty() && stem.contains(slug) {
                score += 25;
            }
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn find_best_icon_bytes(manifest: &Manifest) -> Option<(&'static str, Vec<u8>)> {
    // Case A: Single-file AppImage Type 2 -> read embedded SquashFS icons.
    if manifest.install_dir.is_file()
        && let Ok(fmt) = detect_file_format(&manifest.install_dir)
        && fmt == Format::AppImageType2
        && let Ok(contents) = inspect_contents(&manifest.install_dir, fmt)
    {
        let slug = bin_slug_for_id(&manifest.app_id);
        let scored = score_icons(&contents.entries, &slug);
        for cand in scored {
            if let Some((_, bytes)) = contents
                .embedded_icons
                .iter()
                .find(|(p, _)| p == &cand.path)
            {
                if is_svg_bytes(bytes) {
                    return Some(("svg", bytes.clone()));
                }
                if bytes.starts_with(b"\x89PNG") {
                    return Some(("png", bytes.clone()));
                }
            }
        }
    }

    // Case B: App directory (e.g. ~/Applications/zen/, ~/Applications/WebStorm-*/).
    if manifest.install_dir.is_dir() {
        let mut entries = Vec::new();
        collect_icon_entries(
            &manifest.install_dir,
            &manifest.install_dir,
            0,
            &mut entries,
        );
        let slug = bin_slug_for_id(&manifest.app_id);
        let scored = score_icons(&entries, &slug);
        for cand in scored {
            let full = manifest.install_dir.join(&cand.path);
            if let Ok(bytes) = fs::read(&full) {
                if is_svg_bytes(&bytes) {
                    return Some(("svg", bytes));
                }
                if bytes.starts_with(b"\x89PNG") {
                    return Some(("png", bytes));
                }
            }
        }
        // Fallback for Electron apps whose icon is packed inside `resources/app.asar`.
        if let Some((_, bytes)) = crate::detect::extract_icon_from_asar_path(
            &manifest.install_dir.join("resources/app.asar"),
        ) {
            if is_svg_bytes(&bytes) {
                return Some(("svg", bytes));
            }
            if bytes.starts_with(b"\x89PNG") {
                return Some(("png", bytes));
            }
        }
    }
    None
}

fn is_svg_bytes(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    std::str::from_utf8(head).is_ok_and(|s| s.contains("<svg"))
}

fn collect_icon_entries(root: &Path, dir: &Path, depth: usize, out: &mut Vec<ArchiveEntry>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            collect_icon_entries(root, &p, depth + 1, out);
        } else if meta.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            let lower = rel.to_ascii_lowercase();
            if lower.ends_with(".png") || lower.ends_with(".svg") {
                out.push(ArchiveEntry {
                    path: rel,
                    size: meta.len(),
                    mode: 0o644,
                    is_dir: false,
                    is_symlink: false,
                    symlink_target: None,
                    is_elf: false,
                    elf_arch: None,
                    is_script: false,
                });
            }
        }
    }
}

fn upsert_desktop_key(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(&prefix) {
                found = true;
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("{key}={value}"));
    }
    lines.join("\n") + "\n"
}

fn find_main_exec(manifest: &Manifest) -> PathBuf {
    if manifest.install_dir.is_file() {
        return manifest.install_dir.clone();
    }
    if let Some(dp) = manifest
        .created_files
        .iter()
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("desktop"))
        && let Ok(text) = fs::read_to_string(dp)
        && let Some(exec_raw) = desktop_field(&text, "Exec")
        && let Some(p) = parse_exec_binary_path(&exec_raw, Path::new("/"))
        && p.exists()
    {
        return p;
    }
    manifest.install_dir.join(&manifest.source_filename)
}

fn is_icon_valid(roots: &InstallRoots, icon: Option<&str>) -> bool {
    let Some(raw) = icon.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        roots.home_dir.join(rest)
    } else if let Some(rest) = raw.strip_prefix("$HOME/") {
        roots.home_dir.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        return expanded.exists();
    }
    // Check user hicolor theme, legacy ~/.icons, and ~/.local/share/pixmaps.
    let sizes = [
        "scalable", "512x512", "256x256", "128x128", "64x64", "48x48",
    ];
    for sz in sizes {
        for ext in ["svg", "png"] {
            if roots
                .icons_dir
                .join(format!("{sz}/apps/{raw}.{ext}"))
                .exists()
                || roots
                    .home_dir
                    .join(format!(".icons/hicolor/{sz}/apps/{raw}.{ext}"))
                    .exists()
            {
                return true;
            }
        }
    }
    for ext in ["svg", "png"] {
        if roots
            .home_dir
            .join(format!(".local/share/pixmaps/{raw}.{ext}"))
            .exists()
        {
            return true;
        }
    }
    // Check system hicolor / pixmaps.
    Path::new(&format!("/usr/share/pixmaps/{raw}.png")).exists()
        || Path::new(&format!("/usr/share/pixmaps/{raw}.svg")).exists()
        || Path::new(&format!("/usr/share/icons/hicolor/scalable/apps/{raw}.svg")).exists()
        || Path::new(&format!("/usr/share/icons/hicolor/256x256/apps/{raw}.png")).exists()
}

fn parse_exec_binary_path(exec_raw: &str, home_dir: &Path) -> Option<PathBuf> {
    let tokens = split_exec_tokens(exec_raw);
    let mut idx = 0;
    // Skip `env` wrapper and `VAR=val` assignments (common on Wayland/Hyprland/NixOS).
    if tokens
        .get(idx)
        .is_some_and(|t| t == "env" || t == "/usr/bin/env")
    {
        idx += 1;
    }
    while tokens
        .get(idx)
        .is_some_and(|t| t.contains('=') && !t.starts_with('/'))
    {
        idx += 1;
    }
    let token = tokens.get(idx)?;
    if token.is_empty() {
        return None;
    }
    if let Some(stripped) = token
        .strip_prefix("~/")
        .or_else(|| token.strip_prefix("$HOME/"))
    {
        return Some(home_dir.join(stripped));
    }
    let p = PathBuf::from(token);
    if p.is_absolute() { Some(p) } else { None }
}

fn split_exec_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn infer_install_dir(roots: &InstallRoots, canon_exec: &Path) -> PathBuf {
    // 1. If inside ~/Applications/<AppDir>/..., return ~/Applications/<AppDir>.
    if let Ok(canon_apps) = fs::canonicalize(&roots.applications_dir)
        && let Ok(rel) = canon_exec.strip_prefix(&canon_apps)
        && let Some(first) = rel.components().next()
    {
        return canon_apps.join(first);
    }
    // 2. If directly inside ~/.local/bin/ (e.g. helium.AppImage, agy, uv), the file itself is the install target!
    if let Ok(canon_bin) = fs::canonicalize(&roots.bin_dir)
        && canon_exec.parent() == Some(canon_bin.as_path())
    {
        return canon_exec.to_path_buf();
    }
    // 3. If inside <app>/bin/<exe> (e.g. ~/.local/zed.app/bin/zed, ~/.local/share/flutter/bin/flutter),
    // return the app root <app>, not <app>/bin.
    if let Some(parent) = canon_exec.parent() {
        if parent.file_name().and_then(|s| s.to_str()) == Some("bin")
            && let Some(grandparent) = parent.parent()
            && grandparent != roots.home_dir
            && grandparent != roots.home_dir.join(".local")
        {
            return grandparent.to_path_buf();
        }
        return parent.to_path_buf();
    }
    canon_exec.to_path_buf()
}

fn detect_dir_version(install_dir: &Path, exec_path: &Path) -> Option<String> {
    if install_dir.is_dir() {
        if let Ok(ini) = fs::read_to_string(install_dir.join("application.ini")) {
            for line in ini.lines() {
                if let Some(v) = line.trim().strip_prefix("Version=") {
                    return Some(v.trim().to_string());
                }
            }
        }
        if let Ok(text) = fs::read_to_string(install_dir.join("version"))
            && let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty())
        {
            return Some(line.trim_start_matches('v').to_string());
        }
        for rel in ["package.json", "resources/app/package.json"] {
            if let Ok(json) = fs::read_to_string(install_dir.join(rel))
                && let Some(ver) = extract_json_string_field(&json, "version")
            {
                return Some(ver);
            }
        }
    }
    let name = exec_path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    slug_and_version(&name).1
}

fn bin_slug_for_id(app_id: &str) -> String {
    app_id
        .strip_prefix("easi.")
        .unwrap_or(app_id)
        .split('.')
        .next_back()
        .unwrap_or(app_id)
        .to_ascii_lowercase()
}

fn collect_tree(dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if let Ok(meta) = fs::symlink_metadata(&p) {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                dirs.push(p.clone());
                collect_tree(&p, files, dirs);
            } else {
                files.push(p);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn scans_excludes_system_steam_wine_and_adopts_repairs_zen_browser() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        fs::create_dir_all(&roots.desktop_dir).expect("mkdir desktop");
        fs::create_dir_all(&roots.applications_dir).expect("mkdir apps");

        // 1. Existing Zen Browser in ~/Applications/zen with broken Icon= in zen-browser.desktop
        let zen_dir = roots.applications_dir.join("zen");
        let zen_icons = zen_dir.join("browser/chrome/icons/default");
        fs::create_dir_all(&zen_icons).expect("mkdir zen icons");
        let zen_bin = zen_dir.join("zen");
        fs::write(&zen_bin, b"\x7fELF\x02\x01\x01\x00").expect("write zen bin");
        fs::write(
            zen_dir.join("application.ini"),
            "[App]\nName=Zen\nVersion=1.12.5b\n",
        )
        .expect("write application.ini");
        fs::write(zen_icons.join("default128.png"), b"\x89PNG\r\n\x1a\npng").expect("write png");

        let zen_desktop = roots.desktop_dir.join("zen-browser.desktop");
        fs::write(
            &zen_desktop,
            format!(
                "[Desktop Entry]\nType=Application\nName=Zen Browser\nExec={}\nIcon=/nonexistent/zen.png\n",
                zen_bin.display()
            ),
        )
        .expect("write zen desktop");

        // 2. Excluded entries: Steam game, Wine handler, Flatpak, system /usr/bin app
        fs::write(
            roots.desktop_dir.join("Factorio.desktop"),
            "[Desktop Entry]\nName=Factorio\nExec=steam steam://rungameid/427520\n",
        )
        .expect("steam");
        fs::write(
            roots.desktop_dir.join("wine-extension-chm.desktop"),
            "[Desktop Entry]\nName=Wine\nExec=wine start /ProgIDOpen chmfile %f\n",
        )
        .expect("wine");
        fs::write(
            roots.desktop_dir.join("org.flatpak.App.desktop"),
            "[Desktop Entry]\nName=FlatApp\nExec=flatpak run org.flatpak.App\n",
        )
        .expect("flatpak");
        fs::write(
            roots.desktop_dir.join("sys.desktop"),
            "[Desktop Entry]\nName=SysApp\nExec=/usr/bin/gedit\n",
        )
        .expect("sys");

        // Scan -> should ONLY discover Zen Browser with MissingIcon + MissingStartupWmClass!
        let found = scan_unmanaged_apps(&roots);
        assert_eq!(found.len(), 1, "expected only Zen Browser, got {found:?}");
        assert_eq!(found[0].app_id, "zen-browser");
        assert_eq!(found[0].name, "Zen Browser");
        assert_eq!(found[0].version.as_deref(), Some("1.12.5b"));
        assert!(found[0].issues.contains(&AppIssue::MissingIcon));

        // Adopt & Fix -> installs hicolor icon, rewrites Icon=zen-browser + StartupWMClass!
        let manifest = adopt_app(&roots, &found[0], true).expect("adopt zen");
        assert!(manifest.adopted);
        assert!(
            roots
                .icons_dir
                .join("256x256/apps/zen-browser.png")
                .exists()
        );
        let fixed_desktop = fs::read_to_string(&zen_desktop).expect("read fixed desktop");
        assert!(fixed_desktop.contains("zen-browser.png"));
        assert!(fixed_desktop.contains("StartupWMClass="));

        // Second scan returns empty because Zen Browser is now managed!
        assert!(scan_unmanaged_apps(&roots).is_empty());

        // Stop managing (forget_app) removes manifest ONLY, keeps ~/Applications/zen intact!
        forget_app(&roots, "zen-browser").expect("forget");
        assert!(zen_bin.exists(), "forget_app must not delete app files");
        assert!(list_manifests(&roots).expect("list").is_empty());
    }

    #[test]
    fn manual_adopt_folder_and_wayland_env_exec_and_distro_exclusions() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        fs::create_dir_all(&roots.desktop_dir).expect("mkdir desktop");
        fs::create_dir_all(&roots.applications_dir).expect("mkdir apps");

        // User-installed Flatpak inside $HOME and Nix store entry must be excluded.
        let user_flatpak_bin = tmp.path().join(".local/share/flatpak/exports/bin/app");
        fs::create_dir_all(user_flatpak_bin.parent().unwrap()).unwrap();
        fs::write(&user_flatpak_bin, b"\x7fELF").unwrap();
        fs::write(
            roots.desktop_dir.join("user-flatpak.desktop"),
            format!(
                "[Desktop Entry]\nName=UserFlat\nExec={}\n",
                user_flatpak_bin.display()
            ),
        )
        .unwrap();
        fs::write(
            roots.desktop_dir.join("nix-app.desktop"),
            "[Desktop Entry]\nName=NixApp\nExec=env OZONE_PLATFORM=wayland /nix/store/abc-foo/bin/foo\n",
        )
        .unwrap();

        // Wayland/Hyprland `env VAR=val` desktop entry inside $HOME should be discovered.
        let custom_dir = tmp.path().join("Tools/my-editor");
        fs::create_dir_all(&custom_dir).unwrap();
        let custom_bin = custom_dir.join("my-editor");
        fs::write(&custom_bin, b"\x7fELF\x02\x01\x01\x00").unwrap();
        fs::write(custom_dir.join("icon.png"), b"\x89PNG\r\n\x1a\npng").unwrap();

        assert!(scan_unmanaged_apps(&roots).is_empty());

        // Refuses adopting $HOME itself.
        assert!(adopt_path(&roots, tmp.path(), true).is_err());

        // Manual adopt of ~/Tools/my-editor succeeds and wires launcher + icon + bin symlink!
        let m = adopt_path(&roots, &custom_dir, true).expect("manual adopt folder");
        assert_eq!(m.app_id, "easi.my-editor");
        assert!(m.adopted);
        assert!(roots.desktop_dir.join("easi.my-editor.desktop").exists());
        assert!(
            roots
                .icons_dir
                .join("256x256/apps/easi.my-editor.png")
                .exists()
        );
        assert!(roots.bin_dir.join("my-editor").exists());

        // Verify `env VAR=val` Exec parsing works for Wayland/Hyprland/Sway desktop entries.
        let parsed = parse_exec_binary_path(
            &format!("env OZONE_PLATFORM=wayland \"{}\"", custom_bin.display()),
            tmp.path(),
        );
        assert_eq!(parsed.as_deref(), Some(custom_bin.as_path()));
    }
}
