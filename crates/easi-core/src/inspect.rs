//! Candidate scoring, trust summary, and package classification.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::appid::{
    IdInputs, display_name_from_raw, display_name_from_slug, guess_startup_wm_class,
    resolve_app_id, slug_and_version,
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
        needle: "pingsender",
        delta: -80,
    },
    ScoreRule {
        needle: "crashreporter",
        delta: -80,
    },
    ScoreRule {
        needle: "crashhelper",
        delta: -80,
    },
    ScoreRule {
        needle: "minidump-analyzer",
        delta: -80,
    },
    ScoreRule {
        needle: "plugin-container",
        delta: -80,
    },
    ScoreRule {
        needle: "vaapitest",
        delta: -80,
    },
    ScoreRule {
        needle: "glxtest",
        delta: -80,
    },
    ScoreRule {
        needle: "updater",
        delta: -60,
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
        needle: "/platform/",
        delta: -55,
    },
    ScoreRule {
        needle: "/third_party/",
        delta: -70,
    },
    ScoreRule {
        needle: "/vendored/",
        delta: -70,
    },
    ScoreRule {
        needle: "/vendor/",
        delta: -55,
    },
    ScoreRule {
        needle: "/site-packages/",
        delta: -80,
    },
    ScoreRule {
        needle: "/node_modules/",
        delta: -80,
    },
    ScoreRule {
        needle: "/ext-runtime/",
        delta: -60,
    },
    ScoreRule {
        needle: "/bootstrapping/",
        delta: -50,
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
    ScoreRule {
        needle: "node_modules/",
        delta: -80,
    },
    ScoreRule {
        needle: "/extensions/",
        delta: -50,
    },
    ScoreRule {
        needle: "/resources/linux/",
        delta: 35,
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
        let depth = entry.path.trim_start_matches("./").matches('/').count() as i32;
        score -= depth * 5;
        // Boost primary top-level `<app>/bin/<cmd>` entrypoints over deeply-nested bundled runtimes.
        if depth <= 2 && lower.contains("/bin/") {
            score += 25;
        }

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

/// Directory segments that contain documentation/vendored images, NEVER application icons.
const NON_APP_ICON_SEGMENTS: &[&str] = &[
    "/docs/",
    "/doc/",
    "/_static/",
    "/third_party/",
    "/vendored/",
    "/vendor/",
    "/site-packages/",
    "/node_modules/",
    "/tests/",
    "/test/",
    "/examples/",
    "/ref/",
    "/man/",
];

pub fn score_icons(entries: &[ArchiveEntry], app_slug: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for entry in entries {
        if entry.is_dir || entry.is_symlink {
            continue;
        }
        let lower = entry.path.to_ascii_lowercase();
        let padded = format!("/{lower}");
        if NON_APP_ICON_SEGMENTS.iter().any(|seg| padded.contains(seg)) {
            continue;
        }
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
    format: Format,
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

    let signature = check_signature(
        path,
        contents.has_appimage_sig_section,
        contents.appimage_sig_section.as_ref(),
    );
    let mut warnings = Vec::new();
    let is_appimage_fmt = matches!(format, Format::AppImageType1 | Format::AppImageType2);
    let has_elf = contents.entries.iter().any(|e| e.is_elf);

    // Pre-install extracted-size & decompression-ratio estimation (overridable warning — never
    // interrupts legitimate large apps mid-install).
    if !is_appimage_fmt
        && format != Format::BareElf
        && format != Format::Jar
        && format != Format::ShellInstaller
    {
        let est_bytes: u64 = contents.entries.iter().map(|e| e.size).sum();
        let entry_count = contents.entries.len();
        let ratio = est_bytes.checked_div(size_bytes).unwrap_or(0);
        const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;
        const RATIO_MIN_BYTES: u64 = 256 * 1024 * 1024;
        if est_bytes > FOUR_GIB
            || entry_count > 50_000
            || (ratio >= 100 && est_bytes > RATIO_MIN_BYTES)
        {
            warnings.push(format!(
                "Large or high-ratio archive: extracts to ~{} across {} entries ({}x ratio) — verify free disk space and trust this source",
                crate::manifest::format_human_bytes(est_bytes),
                entry_count,
                ratio.max(1)
            ));
        }
    }

    // Fail loudly with an actionable terminal command if host lacks required runtime deps
    // (e.g. musl host without `gcompat`, NixOS without `appimage-run`/`nix-ld`, or `.jar` without Java).
    crate::install::check_host_runtime_deps(format, has_elf)?;

    if is_appimage_fmt && crate::install::is_nixos() && crate::install::has_appimage_run() {
        warnings.push("NixOS detected — easi will wrap this AppImage with `appimage-run`".into());
    } else if is_appimage_fmt && !crate::install::has_libfuse2() {
        warnings.push(
            "libfuse.so.2 not found — easi will create an APPIMAGE_EXTRACT_AND_RUN=1 wrapper so this AppImage launches cleanly"
                .into(),
        );
    }
    // Check ELF architecture mismatch (e.g. x86_64 binary on Asahi Linux / ARM64).
    let host_arch = std::env::consts::ARCH;
    let elf_arch_opt = contents
        .entries
        .iter()
        .find_map(|e| e.elf_arch)
        .or_else(|| {
            if is_appimage_fmt {
                let mut f = File::open(path).ok()?;
                let mut hdr = [0u8; 20];
                let n = f.read(&mut hdr).ok()?;
                crate::detect::elf_arch_from_header(&hdr[..n])
            } else {
                None
            }
        });
    if let Some(bin_arch) = elf_arch_opt
        && bin_arch != host_arch
    {
        warnings.push(format!(
            "Architecture mismatch: binary is built for {bin_arch}, but this machine is {host_arch} (may not run without emulation)"
        ));
    }

    if format == Format::BareElf {
        let missing = crate::install::missing_shared_libs(path);
        if !missing.is_empty() {
            warnings.push(format!(
                "Missing shared libraries: {} (app may fail to launch until installed)",
                missing.join(", ")
            ));
        }
    }

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

/// Detached signature suffixes checked next to a package, in preference order.
pub const DETACHED_SIGNATURE_EXTS: &[&str] = &[".sig", ".asc"];

fn check_signature(
    path: &Path,
    has_appimage_sig_section: bool,
    appimage_sig_section: Option<&(u64, u64, Vec<u8>)>,
) -> SignatureStatus {
    for ext in DETACHED_SIGNATURE_EXTS {
        let mut sig_os = path.as_os_str().to_os_string();
        sig_os.push(ext);
        let sig_path = std::path::PathBuf::from(sig_os);
        if sig_path.exists() {
            return gpg_verify(&sig_path, path, &format!("detached {ext}"));
        }
    }

    if let Some((sig_off, sig_size, sig_bytes)) = appimage_sig_section {
        return verify_appimage_embedded_sig(path, *sig_off, *sig_size, sig_bytes);
    }

    if has_appimage_sig_section {
        return SignatureStatus::PresentUnverified("AppImage embedded .sha256_sig present".into());
    }
    SignatureStatus::None
}

/// Verify an AppImage Type 2 embedded `.sha256_sig` section via `gpg` by hashing the file
/// with `.sha256_sig` zeroed out per the AppImage specification.
fn verify_appimage_embedded_sig(
    path: &Path,
    sig_off: u64,
    sig_size: u64,
    sig_bytes: &[u8],
) -> SignatureStatus {
    let trimmed_sig: Vec<u8> = sig_bytes.iter().copied().take_while(|&b| b != 0).collect();
    if trimmed_sig.is_empty() {
        return SignatureStatus::None;
    }

    let Ok(mut file) = File::open(path) else {
        return SignatureStatus::PresentUnverified("AppImage embedded .sha256_sig present".into());
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut pos = 0u64;
    let sig_end = sig_off.saturating_add(sig_size);
    loop {
        let Ok(n) = file.read(&mut buf) else {
            return SignatureStatus::PresentUnverified(
                "AppImage embedded .sha256_sig present".into(),
            );
        };
        if n == 0 {
            break;
        }
        let chunk_end = pos + n as u64;
        if chunk_end > sig_off && pos < sig_end {
            let z_start = sig_off.saturating_sub(pos) as usize;
            let z_end = (sig_end.min(chunk_end) - pos) as usize;
            buf[z_start..z_end].fill(0);
        }
        hasher.update(&buf[..n]);
        pos = chunk_end;
    }
    let hex_digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let Ok(tmp) = tempfile::TempDir::new() else {
        return SignatureStatus::PresentUnverified("AppImage embedded .sha256_sig present".into());
    };
    let digest_path = tmp.path().join("digest.txt");
    let sig_path = tmp.path().join("digest.sig");
    if std::fs::write(&digest_path, &hex_digest).is_err()
        || std::fs::write(&sig_path, &trimmed_sig).is_err()
    {
        return SignatureStatus::PresentUnverified("AppImage embedded .sha256_sig present".into());
    }

    gpg_verify(&sig_path, &digest_path, "AppImage embedded .sha256_sig")
}

/// Outcome of one `gpg --verify` run, derived only from its machine-readable status lines.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GpgVerdict {
    /// Cryptographically valid; carries the primary-key fingerprint (stable publisher identity).
    Valid(String),
    NoPublicKey,
    Rejected(&'static str),
}

/// gpg status keywords that override any `VALIDSIG`, in precedence order. gpg exits 0 for a
/// valid signature from *any* key in the keyring and still prints `VALIDSIG` next to
/// `EXPKEYSIG`/`REVKEYSIG`, so exit status alone must never mean "verified".
const GPG_STATUS_OVERRIDES: &[(&str, GpgVerdict)] = &[
    ("BADSIG", GpgVerdict::Rejected("bad signature")),
    ("REVKEYSIG", GpgVerdict::Rejected("signing key revoked")),
    ("EXPKEYSIG", GpgVerdict::Rejected("signing key expired")),
    ("EXPSIG", GpgVerdict::Rejected("signature expired")),
    ("NO_PUBKEY", GpgVerdict::NoPublicKey),
    (
        "ERRSIG",
        GpgVerdict::Rejected("signature could not be checked"),
    ),
];

/// Parse `gpg --status-fd 1` output. `VALIDSIG <fpr> <date> <ts> <exp> <ver> <res> <pk> <hash>
/// <class> [<primary-fpr>]`: prefer the primary fingerprint so subkey rotation keeps identity.
fn parse_gpg_status(status: &str) -> GpgVerdict {
    let lines: Vec<Vec<&str>> = status
        .lines()
        .filter_map(|l| l.strip_prefix("[GNUPG:] "))
        .map(|l| l.split_whitespace().collect())
        .collect();
    for (keyword, verdict) in GPG_STATUS_OVERRIDES {
        if lines.iter().any(|f| f.first() == Some(keyword)) {
            return verdict.clone();
        }
    }
    lines
        .iter()
        .find(|f| f.first() == Some(&"VALIDSIG"))
        .and_then(|f| f.get(10).or_else(|| f.get(1)))
        .filter(|fpr| fpr.len() >= 32 && fpr.chars().all(|c| c.is_ascii_hexdigit()))
        .map_or(GpgVerdict::Rejected("no valid signature"), |fpr| {
            GpgVerdict::Valid(fpr.to_ascii_uppercase())
        })
}

fn gpg_verify(sig_path: &Path, data_path: &Path, label: &str) -> SignatureStatus {
    match Command::new("gpg")
        .args(["--batch", "--status-fd", "1", "--verify"])
        .arg(sig_path)
        .arg(data_path)
        .output()
    {
        Ok(out) => match parse_gpg_status(&String::from_utf8_lossy(&out.stdout)) {
            GpgVerdict::Valid(fingerprint) => SignatureStatus::Verified {
                detail: format!("{label} verified via gpg"),
                fingerprint,
            },
            GpgVerdict::NoPublicKey => SignatureStatus::PresentUnverified(format!(
                "{label} present (signing key not in local gpg keyring)"
            )),
            GpgVerdict::Rejected(reason) => {
                SignatureStatus::Invalid(format!("{label} failed gpg verification ({reason})"))
            }
        },
        Err(_) => SignatureStatus::PresentUnverified(format!("{label} present (gpg unavailable)")),
    }
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
    let trust = build_trust_summary(path, format, &contents)?;

    // Guard against Arch Linux pacman packages (`.pkg.tar.zst` / `.PKGINFO`).
    let is_distro_pkg = contents.entries.iter().any(|e| {
        let trimmed = e.path.trim_start_matches("./");
        trimmed == ".PKGINFO" || trimmed == "debian-binary"
    });
    if is_distro_pkg {
        return Ok(Classified::NotAnApp {
            path: path.to_path_buf(),
            reason: "distro package archive (use your system package manager)".into(),
        });
    }

    if format == Format::Jar && contents.jar_main_class.is_none() {
        return Ok(Classified::NotAnApp {
            path: path.to_path_buf(),
            reason: "Java library JAR (no Main-Class entry in MANIFEST.MF)".into(),
        });
    }

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
        Format::AppImageType1
            | Format::AppImageType2
            | Format::BareElf
            | Format::Jar
            | Format::ShellInstaller
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
    let desktop_version = desktop.and_then(|(_, text)| desktop_field(text, "X-AppImage-Version"));
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
        || format == Format::Jar
        || (desktop.is_some() && !desktop_terminal)
        || !icon_candidates.is_empty()
        || has_gui_marker
    {
        AppKind::Gui
    } else {
        AppKind::Terminal
    };

    let upd_repo_name = contents
        .appimage_upd_info
        .as_deref()
        .and_then(crate::upstream::parse_appimage_upd_info)
        .and_then(|src| match src {
            crate::model::UpdateSource::GitHubRelease { repo, .. } => {
                Some(display_name_from_slug(&repo))
            }
            crate::model::UpdateSource::ForgeRelease { project, .. } => {
                project.rsplit('/').next().map(display_name_from_slug)
            }
            crate::model::UpdateSource::ZsyncUrl { .. }
            | crate::model::UpdateSource::ElectronYaml { .. }
            | crate::model::UpdateSource::JetBrains { .. }
            | crate::model::UpdateSource::HttpRedirect { .. } => None,
        });

    let name = desktop_name
        .or_else(|| contents.meta_name.clone())
        .or(upd_repo_name)
        .unwrap_or_else(|| {
            let raw_hint = top_dir
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(source_name.as_str());
            display_name_from_raw(raw_hint)
        });
    let resolved_version = desktop_version
        .or_else(|| contents.meta_version.clone())
        .or(file_version);

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
            version: resolved_version,
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

    let developer = metainfo
        .and_then(|(_, xml)| crate::appid::extract_xml_tag(xml, "developer_name"))
        .or_else(|| {
            update_source.as_ref().and_then(|src| match src {
                crate::model::UpdateSource::GitHubRelease { owner, .. } => Some(owner.clone()),
                crate::model::UpdateSource::ForgeRelease { project, .. } => {
                    project.split('/').next().map(str::to_string)
                }
                crate::model::UpdateSource::JetBrains { .. } => Some("JetBrains".into()),
                crate::model::UpdateSource::ZsyncUrl { .. }
                | crate::model::UpdateSource::ElectronYaml { .. }
                | crate::model::UpdateSource::HttpRedirect { .. } => None,
            })
        });
    let plan = InstallPlan {
        source_path: path.to_path_buf(),
        format,
        app_id: app_id.clone(),
        synthesized_id,
        name,
        version: resolved_version,
        kind,
        main_exec,
        icon_path,
        icon_bytes,
        desktop_entry: desktop.map(|(_, text)| text.clone()),
        startup_wm_class,
        developer,
        origin_url: trust.origin_url.clone(),
        signature_summary: Some(trust.signature.summary()),
        signer_fingerprint: trust.signature.fingerprint().map(str::to_string),
        update_source,
        planned_writes,
    };

    if let Some(existing) = find_existing(&app_id) {
        return Ok(Classified::Conflict {
            existing: Box::new(existing),
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
    fn gpg_status_lines_decide_verdict_not_exit_code() {
        const FPR: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
        const SUBKEY: &str = "89ABCDEF0123456789ABCDEF0123456789ABCDEF";
        let validsig_primary =
            format!("[GNUPG:] VALIDSIG {SUBKEY} 2026-01-01 1767225600 0 4 0 1 10 00 {FPR}\n");
        let validsig_bare = format!("[GNUPG:] VALIDSIG {}\n", FPR.to_ascii_lowercase());
        let rows: &[(&str, String, GpgVerdict)] = &[
            (
                "valid subkey sig -> primary fingerprint",
                format!("[GNUPG:] GOODSIG ABCD Dev\n{validsig_primary}[GNUPG:] TRUST_UNDEFINED\n"),
                GpgVerdict::Valid(FPR.into()),
            ),
            (
                "valid sig without primary field -> uppercased fingerprint",
                validsig_bare,
                GpgVerdict::Valid(FPR.into()),
            ),
            (
                "expired key still prints VALIDSIG",
                format!("[GNUPG:] EXPKEYSIG ABCD Dev\n{validsig_primary}"),
                GpgVerdict::Rejected("signing key expired"),
            ),
            (
                "revoked key",
                format!("[GNUPG:] REVKEYSIG ABCD Dev\n{validsig_primary}"),
                GpgVerdict::Rejected("signing key revoked"),
            ),
            (
                "bad signature",
                "[GNUPG:] BADSIG ABCD Dev\n".into(),
                GpgVerdict::Rejected("bad signature"),
            ),
            (
                "missing public key",
                "[GNUPG:] ERRSIG ABCD 1 10 00 1767225600 9 -\n[GNUPG:] NO_PUBKEY ABCD\n".into(),
                GpgVerdict::NoPublicKey,
            ),
            (
                "error without missing key",
                "[GNUPG:] ERRSIG ABCD 1 10 00 1767225600 4 -\n".into(),
                GpgVerdict::Rejected("signature could not be checked"),
            ),
            (
                "no status lines",
                "gpg: Good signature from \"Dev\"\n".into(),
                GpgVerdict::Rejected("no valid signature"),
            ),
            (
                "malformed fingerprint",
                "[GNUPG:] VALIDSIG nothex\n".into(),
                GpgVerdict::Rejected("no valid signature"),
            ),
        ];
        for (name, status, want) in rows {
            assert_eq!(&parse_gpg_status(status), want, "{name}");
        }
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

        // 6. CLI SDK with bundled runtime ELF + vendored doc PNGs -> Terminal, top-level bin/ wins
        let sdk_pkg = make_tar_gz(
            tmp.path(),
            "google-cloud-cli-linux-x86_64.tar.gz",
            &[
                (
                    "google-cloud-sdk/bin/gcloud",
                    b"#!/bin/sh\necho hi\n",
                    0o755,
                ),
                (
                    "google-cloud-sdk/platform/bundledpythonunix/bin/python3.14",
                    b"\x7fELF\x02\x01\x01\x00",
                    0o755,
                ),
                (
                    "google-cloud-sdk/platform/gsutil/third_party/requests/docs/_static/logo.png",
                    b"\x89PNG\r\n",
                    0o644,
                ),
            ],
        );
        match classify_package(&sdk_pkg, |_| None).expect("classify sdk") {
            Classified::Ready { plan, .. } => {
                assert_eq!(plan.kind, AppKind::Terminal);
                assert_eq!(plan.main_exec, "google-cloud-sdk/bin/gcloud");
                assert!(plan.icon_path.is_none());
            }
            other => panic!("expected Ready Terminal for CLI SDK, got {other:?}"),
        }
    }
}
