//! Candidate scoring, trust summary, and package classification.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::appid::{
    IdInputs, display_name_from_slug, guess_startup_wm_class, resolve_app_id, slug_and_version,
};
use crate::detect::{ArchiveEntry, PackageContents, detect_file_format, inspect_contents};
use crate::error::EasiError;
use crate::model::{
    AppKind, Candidate, ChoiceDraft, Classified, Format, InstallPlan, InstalledApp,
    SignatureStatus, TrustSummary, WizardStep,
};

struct ScoreRule {
    needle: &'static str,
    delta: i32,
}

/// Path-substring score adjustments for executable candidates.
const EXEC_PATH_RULES: &[ScoreRule] = &[
    ScoreRule {
        needle: "/bin/",
        delta: 30,
    },
    ScoreRule {
        needle: "uninstall",
        delta: -80,
    },
    ScoreRule {
        needle: "crashpad",
        delta: -60,
    },
    ScoreRule {
        needle: "chrome-sandbox",
        delta: -60,
    },
    ScoreRule {
        needle: "nacl_helper",
        delta: -60,
    },
    ScoreRule {
        needle: "/lib/",
        delta: -40,
    },
    ScoreRule {
        needle: "/libexec/",
        delta: -40,
    },
    ScoreRule {
        needle: ".so",
        delta: -100,
    },
    ScoreRule {
        needle: "test",
        delta: -25,
    },
];

/// Path-substring score adjustments for icon candidates.
const ICON_PATH_RULES: &[ScoreRule] = &[
    ScoreRule {
        needle: ".svg",
        delta: 40,
    },
    ScoreRule {
        needle: "256x256",
        delta: 35,
    },
    ScoreRule {
        needle: "512x512",
        delta: 35,
    },
    ScoreRule {
        needle: "128x128",
        delta: 25,
    },
    ScoreRule {
        needle: "64x64",
        delta: 15,
    },
    ScoreRule {
        needle: "icons/hicolor",
        delta: 20,
    },
    ScoreRule {
        needle: "16x16",
        delta: -10,
    },
    ScoreRule {
        needle: "tray",
        delta: -20,
    },
];

/// Source-tree markers: if present with no executables, report NotAnApp (never build from source).
const SOURCE_MARKERS: &[&str] = &[
    "Makefile",
    "configure",
    "CMakeLists.txt",
    "Cargo.toml",
    "meson.build",
    "setup.py",
    "pyproject.toml",
];

/// Bundled GUI framework/runtime markers for tarballs/zips that ship without `.desktop` or loose icons
/// (e.g. Electron/Chromium/Qt/GTK/SDL apps).
const GUI_MARKERS: &[&str] = &[
    "chrome-sandbox",
    "chrome_100_percent.pak",
    "app.asar",
    "libgtk",
    "libqt",
    "libegl",
    "libgles",
    "libvulkan",
    "libwayland",
    "libsdl",
];

pub fn score_executables(
    entries: &[ArchiveEntry],
    app_slug: &str,
    desktop_exec_hint: Option<&str>,
) -> Vec<Candidate> {
    let desktop_exec_base = desktop_exec_hint.and_then(|raw| {
        let first = raw.split_whitespace().next()?;
        Path::new(first)
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
    });
    let mut out = Vec::new();
    for entry in entries {
        if entry.is_dir || entry.is_symlink {
            continue;
        }
        let exec_bit = (entry.mode & 0o111) != 0;
        if !(entry.is_elf || (entry.is_script && exec_bit)) {
            continue;
        }
        let lower = entry.path.to_ascii_lowercase();
        let base = Path::new(&entry.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if base.ends_with(".so") || base.contains(".so.") {
            continue;
        }

        let mut score: i32 = if entry.is_elf { 50 } else { 20 };
        if exec_bit {
            score += 15;
        }
        if desktop_exec_base.as_deref() == Some(base.as_str()) {
            score += 60;
        }
        if !app_slug.is_empty() && (base == app_slug || base.starts_with(app_slug)) {
            score += 45;
        }
        let depth = entry.path.matches('/').count() as i32;
        score -= depth * 5;

        for rule in EXEC_PATH_RULES {
            if lower.contains(rule.needle) {
                score += rule.delta;
            }
        }
        out.push(Candidate {
            path: entry.path.clone(),
            score,
        });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    out
}

pub fn score_icons(entries: &[ArchiveEntry], app_slug: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.is_dir || entry.is_symlink {
            continue;
        }
        let lower = entry.path.to_ascii_lowercase();
        let is_diricon = lower == ".diricon" || lower.ends_with("/.diricon");
        if !(lower.ends_with(".png")
            || lower.ends_with(".svg")
            || lower.ends_with(".xpm")
            || is_diricon)
        {
            continue;
        }
        let base = Path::new(&entry.path)
            .file_stem()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        let mut score: i32 = if is_diricon { 30 } else { 10 };
        if !app_slug.is_empty() && base.contains(app_slug) {
            score += 40;
        }
        for rule in ICON_PATH_RULES {
            if lower.contains(rule.needle) {
                score += rule.delta;
            }
        }
        out.push(Candidate {
            path: entry.path.clone(),
            score,
        });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Extract `Key=Value` from `[Desktop Entry]` section of a `.desktop` file.
pub fn desktop_field(desktop_text: &str, key: &str) -> Option<String> {
    let mut in_main = false;
    for line in desktop_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_main = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_main || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Find the common single top-level directory of an archive, if all entries share one.
pub fn common_top_dir(entries: &[ArchiveEntry]) -> Option<String> {
    let mut tops = HashSet::new();
    for e in entries {
        let trimmed = e.path.trim_start_matches("./");
        if trimmed.is_empty() {
            continue;
        }
        let first = trimmed.split('/').next().unwrap_or("");
        if !first.is_empty() {
            tops.insert(first.to_string());
        }
    }
    if tops.len() == 1 {
        tops.into_iter().next()
    } else {
        None
    }
}

/// Compute SHA-256, origin xattr, signature status, and safety warnings.
pub fn build_trust_summary(
    path: &Path,
    contents: &PackageContents,
) -> Result<TrustSummary, EasiError> {
    let meta = std::fs::metadata(path)?;
    let size_bytes = meta.len();

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let origin_url = xattr::get(path, "user.xdg.origin.url")
        .ok()
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok());

    let signature = check_signature(path, contents.has_appimage_sig_section);
    let mut warnings = Vec::new();

    for entry in &contents.entries {
        if has_path_escape(&entry.path) {
            return Err(EasiError::PathEscape(entry.path.clone()));
        }
        if (entry.mode & 0o6000) != 0 {
            warnings.push(format!(
                "setuid/setgid bit will be stripped on {}",
                entry.path
            ));
        }
        if (entry.mode & 0o002) != 0 && !entry.is_symlink {
            warnings.push(format!("world-writable permissions on {}", entry.path));
        }
    }

    Ok(TrustSummary {
        size_bytes,
        sha256,
        origin_url,
        signature,
        warnings,
        script_report: contents.script_report.clone(),
    })
}

fn check_signature(path: &Path, has_appimage_sig_section: bool) -> SignatureStatus {
    for ext in [".sig", ".asc"] {
        let mut sig_os = path.as_os_str().to_os_string();
        sig_os.push(ext);
        let sig_path = std::path::PathBuf::from(sig_os);
        if sig_path.exists() {
            match Command::new("gpg")
                .args(["--batch", "--verify"])
                .arg(&sig_path)
                .arg(path)
                .output()
            {
                Ok(out) if out.status.success() => {
                    return SignatureStatus::Verified(format!("detached {ext} verified via gpg"));
                }
                Ok(_) => {
                    return SignatureStatus::Invalid(format!(
                        "detached {ext} failed gpg verification (key missing or bad sig)"
                    ));
                }
                Err(_) => {
                    return SignatureStatus::PresentUnverified(format!(
                        "detached {ext} present (gpg unavailable)"
                    ));
                }
            }
        }
    }

    if has_appimage_sig_section {
        return SignatureStatus::PresentUnverified("AppImage embedded .sha256_sig present".into());
    }
    SignatureStatus::None
}

pub fn has_path_escape(rel_path: &str) -> bool {
    if rel_path.starts_with('/') {
        return true;
    }
    rel_path.split('/').any(|part| part == "..")
}

/// Full read-only inspection pipeline: detect → list → trust → score → classify.
pub fn classify_package<F>(path: &Path, find_existing: F) -> Result<Classified, EasiError>
where
    F: Fn(&str) -> Option<InstalledApp>,
{
    let format = detect_file_format(path)?;
    let contents = inspect_contents(path, format)?;
    let trust = build_trust_summary(path, &contents)?;

    let source_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let top_dir = common_top_dir(&contents.entries);
    let (file_slug, file_version) = slug_and_version(&source_name);

    let metainfo = contents.metainfo_files.first();
    let desktop = contents.desktop_files.first();

    let desktop_exec_hint = desktop.and_then(|(_, text)| desktop_field(text, "Exec"));
    let is_single_file_app = matches!(
        format,
        Format::AppImageType1 | Format::AppImageType2 | Format::BareElf | Format::ShellInstaller
    );
    let exec_candidates = if is_single_file_app {
        vec![Candidate {
            path: source_name.clone(),
            score: 100,
        }]
    } else {
        score_executables(&contents.entries, &file_slug, desktop_exec_hint.as_deref())
    };

    if exec_candidates.is_empty() {
        let is_source = contents.entries.iter().any(|e| {
            let base = Path::new(&e.path)
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            SOURCE_MARKERS.contains(&base.as_ref())
        });
        let reason = if is_source {
            "source code archive (building from source is out of scope)".into()
        } else {
            "no executable binaries found".into()
        };
        return Ok(Classified::NotAnApp {
            path: path.to_path_buf(),
            reason,
        });
    }

    let icon_candidates = score_icons(&contents.entries, &file_slug);

    let (app_id, synthesized_id) = resolve_app_id(&IdInputs {
        metainfo_xml: metainfo.map(|(_, xml)| xml.as_str()),
        desktop_filename: desktop.map(|(p, _)| p.as_str()),
        top_dir: top_dir.as_deref(),
        source_filename: Some(source_name.as_str()),
        main_exec: exec_candidates.first().map(|c| c.path.as_str()),
    });

    let desktop_name = desktop.and_then(|(_, text)| desktop_field(text, "Name"));
    let desktop_terminal = desktop
        .and_then(|(_, text)| desktop_field(text, "Terminal"))
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let has_gui_marker = contents.entries.iter().any(|e| {
        let lower = e.path.to_ascii_lowercase();
        GUI_MARKERS.iter().any(|m| lower.contains(m))
    });

    let is_appimage = matches!(format, Format::AppImageType1 | Format::AppImageType2);
    let kind = if is_appimage
        || (desktop.is_some() && !desktop_terminal)
        || !icon_candidates.is_empty()
        || has_gui_marker
    {
        AppKind::Gui
    } else {
        AppKind::Terminal
    };

    let name = desktop_name.unwrap_or_else(|| {
        let slug = app_id.strip_prefix("easi.").unwrap_or(&app_id);
        display_name_from_slug(slug)
    });

    // Ambiguous if top two executable candidates are within 15 points of each other.
    let exec_ambiguous =
        exec_candidates.len() > 1 && (exec_candidates[0].score - exec_candidates[1].score) < 15;

    if exec_ambiguous {
        let draft = ChoiceDraft {
            source_path: path.to_path_buf(),
            format,
            app_id,
            synthesized_id,
            exec_candidates,
            icon_candidates,
            selected_exec: None,
            selected_icon: None,
            name,
            version: file_version,
            kind,
            desktop_entry: desktop.map(|(_, text)| text.clone()),
            startup_wm_class: None,
        };
        return Ok(Classified::NeedsChoice {
            draft,
            trust,
            step: WizardStep::PickExecutable,
        });
    }

    let main_exec = exec_candidates[0].path.clone();
    let icon_path = icon_candidates.first().map(|c| c.path.clone());
    let icon_bytes = icon_path.as_ref().and_then(|ip| {
        contents
            .embedded_icons
            .iter()
            .find(|(p, _)| p == ip)
            .map(|(_, bytes)| bytes.clone())
    });
    let startup_wm_class = desktop
        .and_then(|(_, text)| desktop_field(text, "StartupWMClass"))
        .or_else(|| guess_startup_wm_class(&main_exec));

    let mut planned_writes = vec![format!("Install files to ~/Applications/{name}/")];
    if format == Format::ShellInstaller {
        planned_writes.push(
            "Execute the installer script (with user consent) and record created files in manifest"
                .into(),
        );
    } else {
        if kind == AppKind::Gui {
            planned_writes.push(format!(
                "Create launcher ~/.local/share/applications/{app_id}.desktop"
            ));
            if icon_path.is_some() {
                planned_writes.push(format!(
                    "Install icon into ~/.local/share/icons/hicolor/ ({app_id})"
                ));
            }
        }
        let bin_name = app_id
            .strip_prefix("easi.")
            .unwrap_or(&app_id)
            .split('.')
            .next_back()
            .unwrap_or(&app_id)
            .to_ascii_lowercase();
        planned_writes.push(format!("Create command symlink ~/.local/bin/{bin_name}"));
    }
    planned_writes.push(format!(
        "Record manifest ~/.local/share/easi/installed/{app_id}.toml (for clean uninstall)"
    ));

    let update_source = crate::upstream::detect_update_source(
        contents.appimage_upd_info.as_deref(),
        trust.origin_url.as_deref(),
        metainfo.map(|(_, xml)| xml.as_str()),
    );

    let plan = InstallPlan {
        source_path: path.to_path_buf(),
        format,
        app_id: app_id.clone(),
        synthesized_id,
        name,
        version: file_version,
        kind,
        main_exec,
        icon_path,
        icon_bytes,
        desktop_entry: desktop.map(|(_, text)| text.clone()),
        startup_wm_class,
        update_source,
        planned_writes,
    };

    if let Some(existing) = find_existing(&app_id) {
        return Ok(Classified::Conflict {
            existing,
            plan,
            trust,
        });
    }

    Ok(Classified::Ready { plan, trust })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::TempDir;

    use super::*;

    fn make_tar_gz(dir: &Path, name: &str, files: &[(&str, &[u8], u32)]) -> std::path::PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).expect("create tar.gz");
        let enc = GzEncoder::new(file, Compression::fast());
        let mut builder = tar::Builder::new(enc);
        for (rel, data, mode) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder
                .append_data(&mut header, *rel, *data)
                .expect("append entry");
        }
        let enc = builder.into_inner().expect("finish tar");
        enc.finish().expect("finish gzip");
        path
    }

    #[test]
    fn classifies_gui_terminal_ambiguous_and_not_an_app() {
        let tmp = TempDir::new().expect("tempdir");

        // 1. GUI tarball with .desktop + icon + ELF
        let gui_pkg = make_tar_gz(
            tmp.path(),
            "CoolApp-2.1.0-linux-x86_64.tar.gz",
            &[
                ("CoolApp/bin/coolapp", b"\x7fELF\x02\x01\x01\x00", 0o755),
                (
                    "CoolApp/coolapp.desktop",
                    b"[Desktop Entry]\nName=Cool App\nExec=coolapp\n",
                    0o644,
                ),
                ("CoolApp/icons/256x256/coolapp.png", b"\x89PNG\r\n", 0o644),
            ],
        );
        let res = classify_package(&gui_pkg, |_| None).expect("classify gui");
        match res {
            Classified::Ready { plan, trust } => {
                assert_eq!(plan.kind, AppKind::Gui);
                assert_eq!(plan.app_id, "coolapp");
                assert_eq!(plan.name, "Cool App");
                assert_eq!(plan.version.as_deref(), Some("2.1.0"));
                assert_eq!(plan.main_exec, "CoolApp/bin/coolapp");
                assert_eq!(trust.sha256.len(), 64);
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        // 2. Terminal CLI tool (single ELF, no desktop/icon)
        let cli_pkg = make_tar_gz(
            tmp.path(),
            "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz",
            &[
                ("ripgrep-14.1.0/rg", b"\x7fELF\x02\x01\x01\x00", 0o755),
                ("ripgrep-14.1.0/README.md", b"hello", 0o644),
            ],
        );
        let res = classify_package(&cli_pkg, |_| None).expect("classify cli");
        match res {
            Classified::Ready { plan, .. } => {
                assert_eq!(plan.kind, AppKind::Terminal);
                assert_eq!(plan.app_id, "easi.ripgrep");
                assert!(plan.synthesized_id);
                assert_eq!(plan.main_exec, "ripgrep-14.1.0/rg");
            }
            other => panic!("expected Ready terminal, got {other:?}"),
        }

        // 3. Ambiguous tarball (two equally-scored ELFs)
        let amb_pkg = make_tar_gz(
            tmp.path(),
            "suite-1.0.tar.gz",
            &[
                ("suite/bin/alpha", b"\x7fELF\x02\x01\x01\x00", 0o755),
                ("suite/bin/beta", b"\x7fELF\x02\x01\x01\x00", 0o755),
            ],
        );
        let res = classify_package(&amb_pkg, |_| None).expect("classify ambiguous");
        assert!(matches!(
            res,
            Classified::NeedsChoice {
                step: WizardStep::PickExecutable,
                ..
            }
        ));

        // 4. Source archive (NotAnApp)
        let src_pkg = make_tar_gz(
            tmp.path(),
            "myproject-1.0.tar.gz",
            &[
                ("myproject/Makefile", b"all:\n\techo hi\n", 0o644),
                ("myproject/main.c", b"int main() { return 0; }", 0o644),
            ],
        );
        let res = classify_package(&src_pkg, |_| None).expect("classify src");
        assert!(matches!(res, Classified::NotAnApp { .. }));

        // 5. Zip with path escape rejected
        let bad_zip = tmp.path().join("evil.zip");
        {
            let f = File::create(&bad_zip).expect("create zip");
            let mut z = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("../escape.txt", opts).expect("start file");
            z.write_all(b"nope").expect("write");
            z.finish().expect("finish zip");
        }
        let err = classify_package(&bad_zip, |_| None).expect_err("must reject path escape");
        assert!(matches!(err, EasiError::PathEscape(_)));
    }
}
