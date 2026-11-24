//! Transactional installer, safe extractor, wiring, rollback, uninstall, PATH check, and Trash.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::Archive;
use tempfile::TempDir;
use xz2::read::XzDecoder;
use zip::ZipArchive;

use crate::detect::inspect_contents;
use crate::error::EasiError;
use crate::manifest::{
    Manifest, delete_manifest, find_manifest_by_id, resolve_installed, save_manifest,
};
use crate::model::{Format, InstallPlan, InstallStep, InstallSummary};
use crate::plan::{PlanOp, detect_de_quirk, plan_ops};
use crate::roots::{InstallRoots, safe_join};

/// Records every created file/symlink/dir in order so failure or cancel rolls back cleanly.
#[derive(Debug, Default)]
pub struct Transaction {
    pub created_files: Vec<PathBuf>,
    pub created_dirs: Vec<PathBuf>,
}

impl Transaction {
    pub fn record_file(&mut self, path: PathBuf) {
        self.created_files.push(path);
    }

    pub fn record_dir(&mut self, path: PathBuf) {
        if !self.created_dirs.contains(&path) {
            self.created_dirs.push(path);
        }
    }

    pub fn rollback(&self) {
        rollback_paths(&self.created_files, &self.created_dirs);
    }
}

pub fn rollback_paths(files: &[PathBuf], dirs: &[PathBuf]) {
    for path in files.iter().rev() {
        let _ = fs::remove_file(path);
    }
    for dir in dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
}

fn check_cancelled(cancel_flag: &AtomicBool) -> Result<(), EasiError> {
    if cancel_flag.load(Ordering::Relaxed) {
        return Err(EasiError::Desktop("installation cancelled".into()));
    }
    Ok(())
}

/// Execute a full transactional install against `roots`.
/// On any error, rolls back all created paths before returning `Err`.
pub fn execute_install<P>(
    roots: &InstallRoots,
    plan: &InstallPlan,
    replace_existing: bool,
    cancel_flag: &AtomicBool,
    mut on_progress: P,
) -> Result<InstallSummary, EasiError>
where
    P: FnMut(InstallStep, u8),
{
    on_progress(InstallStep::Preparing, 5);
    check_cancelled(cancel_flag)?;

    let old_manifest = if replace_existing {
        find_manifest_by_id(roots, &plan.app_id)
    } else {
        None
    };

    let mut tx = Transaction::default();
    let result = run_install_ops(
        roots,
        plan,
        old_manifest.as_ref(),
        &mut tx,
        cancel_flag,
        &mut on_progress,
    );

    match result {
        Ok(summary) => Ok(summary),
        Err(err) => {
            tx.rollback();
            Err(err)
        }
    }
}

fn run_install_ops<P>(
    roots: &InstallRoots,
    plan: &InstallPlan,
    old_manifest: Option<&Manifest>,
    tx: &mut Transaction,
    cancel_flag: &AtomicBool,
    on_progress: &mut P,
) -> Result<InstallSummary, EasiError>
where
    P: FnMut(InstallStep, u8),
{
    // If replacing an existing install, remove its managed files first.
    if let Some(old) = old_manifest {
        rollback_paths(&old.created_files, &old.created_dirs);
    }

    let safe_dir_name = sanitize_dir_name(&plan.name);
    let app_dir = roots.applications_dir.join(&safe_dir_name);
    if app_dir.exists() && old_manifest.is_none() {
        return Err(EasiError::UnmanagedOverwrite(app_dir));
    }

    let ops = plan_ops(plan.format, plan.kind);
    let mut resolved_exec: Option<PathBuf> = None;
    let mut extracted_icon_bytes: Option<(String, Vec<u8>)> = None;
    let mut installed_icon_name: Option<String> = None;

    for (idx, op) in ops.iter().enumerate() {
        check_cancelled(cancel_flag)?;
        let pct = ((idx + 1) * 90 / ops.len().max(1)) as u8;
        match op {
            PlanOp::CopyAppImage => {
                on_progress(InstallStep::Extracting, pct);
                fs::create_dir_all(&app_dir)?;
                tx.record_dir(app_dir.clone());
                let fname = plan
                    .source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{}.AppImage", plan.app_id));
                let dest = safe_join(&app_dir, Path::new(&fname))?;
                fs::copy(&plan.source_path, &dest)?;
                // Strip setuid/setgid, ensure executable.
                let mut perms = fs::metadata(&dest)?.permissions();
                perms.set_mode((perms.mode() & !0o6000) | 0o755);
                fs::set_permissions(&dest, perms)?;
                tx.record_file(dest.clone());
                resolved_exec = Some(dest);
            }
            PlanOp::ExtractAppImageAssets => {
                on_progress(InstallStep::Extracting, pct);
                if let Ok(contents) = inspect_contents(&plan.source_path, plan.format)
                    && let Some(icon_rel) = &plan.icon_path
                    && let Some((name, bytes)) = contents
                        .embedded_icons
                        .into_iter()
                        .find(|(p, _)| p == icon_rel)
                {
                    extracted_icon_bytes = Some((name, bytes));
                }
            }
            PlanOp::ExtractArchive => {
                on_progress(InstallStep::Extracting, pct);
                let staging = TempDir::new()?;
                extract_archive_safe(&plan.source_path, plan.format, staging.path())?;
                fs::create_dir_all(&app_dir)?;
                tx.record_dir(app_dir.clone());
                copy_tree_recorded(staging.path(), &app_dir, tx)?;
                let exec_path = safe_join(&app_dir, Path::new(&plan.main_exec))?;
                if exec_path.exists() {
                    let mut perms = fs::metadata(&exec_path)?.permissions();
                    perms.set_mode((perms.mode() & !0o6000) | 0o755);
                    fs::set_permissions(&exec_path, perms)?;
                }
                resolved_exec = Some(exec_path);
            }
            PlanOp::WireIcons => {
                on_progress(InstallStep::Wiring, pct);
                let icon_name = plan.app_id.clone();
                if let Some((rel, bytes)) = extracted_icon_bytes.take() {
                    let icon_dest = write_hicolor_icon(roots, &icon_name, &rel, &bytes, tx)?;
                    if icon_dest.is_some() {
                        installed_icon_name = Some(icon_name);
                    }
                } else if let Some(icon_rel) = &plan.icon_path {
                    let src = safe_join(&app_dir, Path::new(icon_rel))?;
                    if src.exists() {
                        let bytes = fs::read(&src)?;
                        let icon_dest =
                            write_hicolor_icon(roots, &icon_name, icon_rel, &bytes, tx)?;
                        if icon_dest.is_some() {
                            installed_icon_name = Some(icon_name);
                        }
                    }
                }
            }
            PlanOp::WireDesktop => {
                on_progress(InstallStep::Wiring, pct);
                if let Some(exec_path) = &resolved_exec {
                    fs::create_dir_all(&roots.desktop_dir)?;
                    let desktop_name = format!("{}.desktop", plan.app_id);
                    let desktop_path = safe_join(&roots.desktop_dir, Path::new(&desktop_name))?;
                    if desktop_path.exists() && old_manifest.is_none() {
                        return Err(EasiError::UnmanagedOverwrite(desktop_path));
                    }
                    let content =
                        render_desktop_entry(plan, exec_path, installed_icon_name.as_deref());
                    fs::write(&desktop_path, content)?;
                    tx.record_file(desktop_path);
                }
            }
            PlanOp::WireBinSymlink => {
                on_progress(InstallStep::Wiring, pct);
                if let Some(exec_path) = &resolved_exec {
                    fs::create_dir_all(&roots.bin_dir)?;
                    let bin_name = plan
                        .app_id
                        .strip_prefix("easi.")
                        .unwrap_or(&plan.app_id)
                        .split('.')
                        .next_back()
                        .unwrap_or(&plan.app_id)
                        .to_ascii_lowercase();
                    let link_path = safe_join(&roots.bin_dir, Path::new(&bin_name))?;
                    if link_path.exists() || link_path.symlink_metadata().is_ok() {
                        if old_manifest.is_some() {
                            let _ = fs::remove_file(&link_path);
                        } else {
                            return Err(EasiError::UnmanagedOverwrite(link_path));
                        }
                    }
                    symlink(exec_path, &link_path)?;
                    tx.record_file(link_path);
                }
            }
            PlanOp::RefreshDesktop => {
                on_progress(InstallStep::RefreshingDesktop, pct);
                refresh_desktop_caches(roots);
            }
            PlanOp::WriteManifest => {
                on_progress(InstallStep::SavingManifest, 98);
                let source_filename = plan
                    .source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let manifest = Manifest {
                    app_id: plan.app_id.clone(),
                    name: plan.name.clone(),
                    version: plan.version.clone(),
                    source_filename,
                    kind: plan.kind,
                    install_dir: app_dir.clone(),
                    created_files: tx.created_files.clone(),
                    created_dirs: tx.created_dirs.clone(),
                };
                save_manifest(roots, &manifest)?;
            }
        }
    }

    Ok(InstallSummary {
        app_id: plan.app_id.clone(),
        name: plan.name.clone(),
        version: plan.version.clone(),
        source_path: plan.source_path.clone(),
        install_dir: app_dir,
        kind: plan.kind,
        bin_on_path: is_bin_dir_on_path(&roots.bin_dir),
    })
}

fn sanitize_dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "App".into()
    } else {
        trimmed.to_string()
    }
}

fn write_hicolor_icon(
    roots: &InstallRoots,
    icon_name: &str,
    rel_src: &str,
    bytes: &[u8],
    tx: &mut Transaction,
) -> Result<Option<PathBuf>, EasiError> {
    let lower = rel_src.to_ascii_lowercase();
    let (subdir, ext) = if lower.ends_with(".svg") {
        ("scalable/apps", "svg")
    } else if lower.ends_with(".png") {
        ("256x256/apps", "png")
    } else {
        return Ok(None);
    };
    let dir = roots.icons_dir.join(subdir);
    fs::create_dir_all(&dir)?;
    let dest = safe_join(&dir, Path::new(&format!("{icon_name}.{ext}")))?;
    fs::write(&dest, bytes)?;
    tx.record_file(dest.clone());
    Ok(Some(dest))
}

fn render_desktop_entry(plan: &InstallPlan, exec_path: &Path, icon_name: Option<&str>) -> String {
    let exec_quoted = quote_desktop_exec(exec_path);
    if let Some(existing) = &plan.desktop_entry {
        let mut out = Vec::new();
        let mut wrote_exec = false;
        let mut wrote_icon = false;
        let mut wrote_wm = false;
        for line in existing.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Exec=") {
                out.push(format!("Exec={exec_quoted}"));
                wrote_exec = true;
            } else if trimmed.starts_with("Icon=") {
                if let Some(ic) = icon_name {
                    out.push(format!("Icon={ic}"));
                    wrote_icon = true;
                } else {
                    out.push(line.to_string());
                    wrote_icon = true;
                }
            } else if trimmed.starts_with("StartupWMClass=") {
                wrote_wm = true;
                out.push(line.to_string());
            } else {
                out.push(line.to_string());
            }
        }
        if !wrote_exec {
            out.push(format!("Exec={exec_quoted}"));
        }
        if !wrote_icon && let Some(ic) = icon_name {
            out.push(format!("Icon={ic}"));
        }
        if !wrote_wm && let Some(wm) = &plan.startup_wm_class {
            out.push(format!("StartupWMClass={wm}"));
        }
        return out.join("\n") + "\n";
    }

    let mut lines = vec![
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
        format!("Name={}", plan.name),
        format!("Exec={exec_quoted}"),
        "Terminal=false".to_string(),
    ];
    if let Some(ic) = icon_name {
        lines.push(format!("Icon={ic}"));
    }
    if let Some(wm) = &plan.startup_wm_class {
        lines.push(format!("StartupWMClass={wm}"));
    }
    lines.join("\n") + "\n"
}

fn quote_desktop_exec(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.into_owned()
    }
}

/// Extract archive safely into `dest_root`: canonicalized paths, no escapes, setuid stripped.
pub fn extract_archive_safe(
    path: &Path,
    format: Format,
    dest_root: &Path,
) -> Result<(), EasiError> {
    match format {
        Format::TarGz => {
            extract_tar_safe(GzDecoder::new(BufReader::new(File::open(path)?)), dest_root)
        }
        Format::TarXz => {
            extract_tar_safe(XzDecoder::new(BufReader::new(File::open(path)?)), dest_root)
        }
        Format::TarZst => {
            let dec = zstd::stream::read::Decoder::new(BufReader::new(File::open(path)?))
                .map_err(|e| EasiError::Archive(e.to_string()))?;
            extract_tar_safe(dec, dest_root)
        }
        Format::TarBz2 => {
            extract_tar_safe(BzDecoder::new(BufReader::new(File::open(path)?)), dest_root)
        }
        Format::Zip => extract_zip_safe(path, dest_root),
        Format::AppImageType1 | Format::AppImageType2 => Ok(()),
    }
}

fn extract_tar_safe<R: Read>(reader: R, dest_root: &Path) -> Result<(), EasiError> {
    let mut archive = Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| EasiError::Archive(e.to_string()))?;

    for item in entries {
        let mut entry = item.map_err(|e| EasiError::Archive(e.to_string()))?;
        let rel = entry
            .path()
            .map_err(|e| EasiError::Archive(e.to_string()))?
            .into_owned();
        if rel.as_os_str().is_empty() || rel == Path::new(".") {
            continue;
        }
        let dest = safe_join(dest_root, &rel)?;
        let header = entry.header();
        let mode = header.mode().unwrap_or(0o644) & !0o6000;
        let etype = header.entry_type();

        if etype.is_dir() {
            fs::create_dir_all(&dest)?;
        } else if etype.is_symlink() {
            if let Some(target) = entry.link_name().ok().flatten() {
                if target.is_absolute()
                    || target
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(EasiError::PathEscape(target.to_string_lossy().into_owned()));
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                symlink(target.as_ref(), &dest)?;
            }
        } else if etype.is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn extract_zip_safe(path: &Path, dest_root: &Path) -> Result<(), EasiError> {
    let file = File::open(path)?;
    let mut zip =
        ZipArchive::new(BufReader::new(file)).map_err(|e| EasiError::Archive(e.to_string()))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| EasiError::Archive(e.to_string()))?;
        let rel = PathBuf::from(entry.name());
        if rel.as_os_str().is_empty() || rel == Path::new(".") || rel == Path::new("./") {
            continue;
        }
        let dest = safe_join(dest_root, &rel)?;
        let mode = entry.unix_mode().unwrap_or(0o644) & !0o6000;

        if entry.is_dir() {
            fs::create_dir_all(&dest)?;
        } else if (mode & 0o170000) == 0o120000 {
            let mut target = String::new();
            entry.read_to_string(&mut target)?;
            let target_path = Path::new(&target);
            if target_path.is_absolute()
                || target_path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(EasiError::PathEscape(target));
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(target_path, &dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

fn copy_tree_recorded(src: &Path, dst: &Path, tx: &mut Transaction) -> Result<(), EasiError> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&src_path)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&src_path)?;
            symlink(target, &dst_path)?;
            tx.record_file(dst_path);
        } else if meta.is_dir() {
            fs::create_dir_all(&dst_path)?;
            tx.record_dir(dst_path.clone());
            copy_tree_recorded(&src_path, &dst_path, tx)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() & !0o6000);
            fs::set_permissions(&dst_path, perms)?;
            tx.record_file(dst_path);
        }
    }
    Ok(())
}

pub fn refresh_desktop_caches(roots: &InstallRoots) {
    let de = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let quirk = detect_de_quirk(&de);

    if quirk.refresh_desktop_db && roots.desktop_dir.exists() {
        let _ = Command::new("update-desktop-database")
            .arg(&roots.desktop_dir)
            .output();
    }
    if quirk.refresh_gtk_icon_cache && roots.icons_dir.exists() {
        let _ = Command::new("gtk-update-icon-cache")
            .args(["-f", "-t"])
            .arg(&roots.icons_dir)
            .output();
    }
    if quirk.refresh_kde_sycoca {
        let _ = Command::new("kbuildsycoca6")
            .output()
            .or_else(|_| Command::new("kbuildsycoca5").output());
    }
}

/// Replay manifest in reverse and remove the manifest file.
pub fn uninstall_app(roots: &InstallRoots, query: &str) -> Result<Manifest, EasiError> {
    let manifest = resolve_installed(roots, query)?;
    rollback_paths(&manifest.created_files, &manifest.created_dirs);
    delete_manifest(roots, &manifest.app_id)?;
    refresh_desktop_caches(roots);
    Ok(manifest)
}

/// Move a file to the XDG Trash (FreeDesktop Trash spec 1.0).
pub fn move_to_trash(roots: &InstallRoots, source: &Path) -> Result<PathBuf, EasiError> {
    if !source.exists() {
        return Err(EasiError::NotFound(source.to_path_buf()));
    }
    let files_dir = roots.trash_dir.join("files");
    let info_dir = roots.trash_dir.join("info");
    fs::create_dir_all(&files_dir)?;
    fs::create_dir_all(&info_dir)?;

    let orig_name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());

    let mut candidate = orig_name.clone();
    let mut counter = 1u32;
    while files_dir.join(&candidate).exists()
        || info_dir.join(format!("{candidate}.trashinfo")).exists()
    {
        candidate = format!("{orig_name}.{counter}");
        counter += 1;
    }

    let dest = files_dir.join(&candidate);
    let info_path = info_dir.join(format!("{candidate}.trashinfo"));
    let abs_source = fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let info_content = format!(
        "[Trash Info]\nPath={}\nDeletionDate={secs}\n",
        abs_source.to_string_lossy()
    );
    fs::write(&info_path, info_content)?;

    if fs::rename(source, &dest).is_err() {
        fs::copy(source, &dest)?;
        fs::remove_file(source)?;
    }
    Ok(dest)
}

pub fn is_bin_dir_on_path(bin_dir: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    let canon_bin = fs::canonicalize(bin_dir).ok();
    std::env::split_paths(&path_var).any(|p| {
        p == bin_dir
            || canon_bin
                .as_ref()
                .zip(fs::canonicalize(&p).ok())
                .is_some_and(|(a, b)| a == &b)
    })
}

/// Append `~/.local/bin` to the user's shell rc file (idempotent).
pub fn add_bin_to_shell_rc(home: &Path) -> Result<PathBuf, EasiError> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let rc_name = if shell.ends_with("zsh") {
        ".zshrc"
    } else if shell.ends_with("bash") {
        ".bashrc"
    } else {
        ".profile"
    };
    let rc_path = home.join(rc_name);
    let line = "export PATH=\"$HOME/.local/bin:$PATH\"";
    let existing = fs::read_to_string(&rc_path).unwrap_or_default();
    if !existing.contains(".local/bin") {
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str("# Added by easi\n");
        next.push_str(line);
        next.push('\n');
        fs::write(&rc_path, next)?;
    }
    Ok(rc_path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;
    use crate::inspect::classify_package;
    use crate::manifest::list_manifests;
    use crate::model::Classified;

    fn make_gui_tarball(dir: &Path, filename: &str) -> PathBuf {
        let path = dir.join(filename);
        let file = File::create(&path).expect("create tar.gz");
        let enc = GzEncoder::new(file, Compression::fast());
        let mut builder = tar::Builder::new(enc);
        let files: &[(&str, &[u8], u32)] = &[
            ("CoolApp/bin/coolapp", b"\x7fELF\x02\x01\x01\x00", 0o4755), // includes setuid to verify strip
            (
                "CoolApp/coolapp.desktop",
                b"[Desktop Entry]\nName=Cool App\nExec=coolapp\nIcon=coolapp\n",
                0o644,
            ),
            ("CoolApp/icons/256x256/coolapp.png", b"\x89PNG\r\n", 0o644),
        ];
        for (rel, data, mode) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder
                .append_data(&mut header, *rel, *data)
                .expect("append");
        }
        builder
            .into_inner()
            .expect("inner")
            .finish()
            .expect("finish");
        path
    }

    #[test]
    fn install_replace_uninstall_and_trash_against_temp_root() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        let dl_dir = tmp.path().join("Downloads");
        fs::create_dir_all(&dl_dir).expect("mkdir downloads");

        let pkg_v1 = make_gui_tarball(&dl_dir, "CoolApp-1.0.0.tar.gz");
        let Classified::Ready { plan, .. } = classify_package(&pkg_v1, |_| None).expect("classify")
        else {
            panic!("expected Ready");
        };

        let cancel = AtomicBool::new(false);
        let summary = execute_install(&roots, &plan, false, &cancel, |_, _| {}).expect("install");
        assert_eq!(summary.app_id, "coolapp");

        // Verify installed binary has setuid stripped!
        let bin_file = summary.install_dir.join("CoolApp/bin/coolapp");
        assert!(bin_file.exists());
        let mode = fs::metadata(&bin_file).expect("meta").permissions().mode();
        assert_eq!(mode & 0o6000, 0, "setuid/setgid must be stripped");

        // Verify .desktop, icon, symlink, and manifest exist.
        let desktop = roots.desktop_dir.join("coolapp.desktop");
        assert!(desktop.exists());
        let desktop_text = fs::read_to_string(&desktop).expect("read desktop");
        assert!(desktop_text.contains("CoolApp/bin/coolapp"));
        assert!(roots.icons_dir.join("256x256/apps/coolapp.png").exists());
        assert!(roots.bin_dir.join("coolapp").symlink_metadata().is_ok());
        assert_eq!(list_manifests(&roots).expect("list").len(), 1);

        // Re-drop detects conflict and replace succeeds.
        let pkg_v2 = make_gui_tarball(&dl_dir, "CoolApp-2.0.0.tar.gz");
        let classified_v2 = classify_package(&pkg_v2, |id| {
            find_manifest_by_id(&roots, id).map(|m| m.to_installed_app())
        })
        .expect("classify v2");
        let Classified::Conflict {
            existing,
            plan: plan_v2,
            ..
        } = classified_v2
        else {
            panic!("expected Conflict");
        };
        assert_eq!(existing.version.as_deref(), Some("1.0.0"));
        let summary_v2 =
            execute_install(&roots, &plan_v2, true, &cancel, |_, _| {}).expect("replace");
        assert_eq!(summary_v2.version.as_deref(), Some("2.0.0"));

        // Move source to Trash.
        let trashed = move_to_trash(&roots, &pkg_v2).expect("trash");
        assert!(trashed.exists());
        assert!(!pkg_v2.exists());

        // Uninstall replays manifest cleanly.
        uninstall_app(&roots, "coolapp").expect("uninstall");
        assert!(!desktop.exists());
        assert!(!bin_file.exists());
        assert!(roots.bin_dir.join("coolapp").symlink_metadata().is_err());
        assert!(list_manifests(&roots).expect("list").is_empty());
    }
}
