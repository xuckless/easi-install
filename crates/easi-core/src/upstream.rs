//! Upstream update checks & one-click updates across GitHub, GitLab, Codeberg,
//! AppImage `.upd_info` zsync, Electron-builder CDN YAML, JetBrains CDN, and direct CDN redirects.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::appid::slug_and_version;
use crate::error::EasiError;
use crate::inspect::classify_package;
use crate::install::execute_install;
use crate::manifest::{Manifest, list_manifests, resolve_installed, save_manifest};
use crate::model::{AppKind, Classified, InstallSummary, UpdateSource};
use crate::roots::InstallRoots;
use crate::self_update::{extract_json_string_field, is_newer_version};

enum KnownSourceKind {
    GitHub(&'static str, &'static str),
    JetBrains(&'static str),
}

struct KnownAppSource {
    slug: &'static str,
    source: KnownSourceKind,
}

/// Curated lookup table of popular non-distro Linux apps -> official upstream update source.
/// Zero-dependency, instant lookup; adding an app is one row here.
const KNOWN_APP_SOURCES: &[KnownAppSource] = &[
    KnownAppSource {
        slug: "zen-browser",
        source: KnownSourceKind::GitHub("zen-browser", "desktop"),
    },
    KnownAppSource {
        slug: "zen",
        source: KnownSourceKind::GitHub("zen-browser", "desktop"),
    },
    KnownAppSource {
        slug: "zed",
        source: KnownSourceKind::GitHub("zed-industries", "zed"),
    },
    KnownAppSource {
        slug: "helium",
        source: KnownSourceKind::GitHub("imputnet", "helium-linux"),
    },
    KnownAppSource {
        slug: "obsidian",
        source: KnownSourceKind::GitHub("obsidianmd", "obsidian-releases"),
    },
    KnownAppSource {
        slug: "vesktop",
        source: KnownSourceKind::GitHub("Vencord", "Vesktop"),
    },
    KnownAppSource {
        slug: "bruno",
        source: KnownSourceKind::GitHub("usebruno", "bruno"),
    },
    KnownAppSource {
        slug: "localsend",
        source: KnownSourceKind::GitHub("localsend", "localsend"),
    },
    KnownAppSource {
        slug: "logseq",
        source: KnownSourceKind::GitHub("logseq", "logseq"),
    },
    KnownAppSource {
        slug: "joplin",
        source: KnownSourceKind::GitHub("laurent22", "joplin"),
    },
    KnownAppSource {
        slug: "siyuan",
        source: KnownSourceKind::GitHub("siyuan-note", "siyuan"),
    },
    KnownAppSource {
        slug: "anytype",
        source: KnownSourceKind::GitHub("anyproto", "anytype-ts"),
    },
    KnownAppSource {
        slug: "bitwarden",
        source: KnownSourceKind::GitHub("bitwarden", "clients"),
    },
    KnownAppSource {
        slug: "keepassxc",
        source: KnownSourceKind::GitHub("keepassxreboot", "keepassxc"),
    },
    KnownAppSource {
        slug: "neovim",
        source: KnownSourceKind::GitHub("neovim", "neovim"),
    },
    KnownAppSource {
        slug: "nvim",
        source: KnownSourceKind::GitHub("neovim", "neovim"),
    },
    KnownAppSource {
        slug: "alacritty",
        source: KnownSourceKind::GitHub("alacritty", "alacritty"),
    },
    KnownAppSource {
        slug: "wezterm",
        source: KnownSourceKind::GitHub("wezterm", "wezterm"),
    },
    KnownAppSource {
        slug: "ghostty",
        source: KnownSourceKind::GitHub("ghostty-org", "ghostty"),
    },
    KnownAppSource {
        slug: "uv",
        source: KnownSourceKind::GitHub("astral-sh", "uv"),
    },
    KnownAppSource {
        slug: "ruff",
        source: KnownSourceKind::GitHub("astral-sh", "ruff"),
    },
    KnownAppSource {
        slug: "ripgrep",
        source: KnownSourceKind::GitHub("BurntSushi", "ripgrep"),
    },
    KnownAppSource {
        slug: "bat",
        source: KnownSourceKind::GitHub("sharkdp", "bat"),
    },
    KnownAppSource {
        slug: "fd",
        source: KnownSourceKind::GitHub("sharkdp", "fd"),
    },
    KnownAppSource {
        slug: "fzf",
        source: KnownSourceKind::GitHub("junegunn", "fzf"),
    },
    KnownAppSource {
        slug: "lazygit",
        source: KnownSourceKind::GitHub("jesseduffield", "lazygit"),
    },
    KnownAppSource {
        slug: "helix",
        source: KnownSourceKind::GitHub("helix-editor", "helix"),
    },
    KnownAppSource {
        slug: "lapce",
        source: KnownSourceKind::GitHub("lapce", "lapce"),
    },
    KnownAppSource {
        slug: "godot",
        source: KnownSourceKind::GitHub("godotengine", "godot"),
    },
    KnownAppSource {
        slug: "prismlauncher",
        source: KnownSourceKind::GitHub("PrismLauncher", "PrismLauncher"),
    },
    KnownAppSource {
        slug: "heroic",
        source: KnownSourceKind::GitHub("Heroic-Games-Launcher", "HeroicGamesLauncher"),
    },
    KnownAppSource {
        slug: "freetube",
        source: KnownSourceKind::GitHub("FreeTubeApp", "FreeTube"),
    },
    KnownAppSource {
        slug: "feishin",
        source: KnownSourceKind::GitHub("jeffvli", "feishin"),
    },
    KnownAppSource {
        slug: "superproductivity",
        source: KnownSourceKind::GitHub("johannesjo", "super-productivity"),
    },
    KnownAppSource {
        slug: "webstorm",
        source: KnownSourceKind::JetBrains("WS"),
    },
    KnownAppSource {
        slug: "intellij",
        source: KnownSourceKind::JetBrains("IIU"),
    },
    KnownAppSource {
        slug: "pycharm",
        source: KnownSourceKind::JetBrains("PCP"),
    },
    KnownAppSource {
        slug: "rustrover",
        source: KnownSourceKind::JetBrains("RR"),
    },
    KnownAppSource {
        slug: "goland",
        source: KnownSourceKind::JetBrains("GO"),
    },
    KnownAppSource {
        slug: "clion",
        source: KnownSourceKind::JetBrains("CL"),
    },
    KnownAppSource {
        slug: "rider",
        source: KnownSourceKind::JetBrains("RD"),
    },
    KnownAppSource {
        slug: "datagrip",
        source: KnownSourceKind::JetBrains("DG"),
    },
    KnownAppSource {
        slug: "phpstorm",
        source: KnownSourceKind::JetBrains("PS"),
    },
];

/// Look up an app_id or name slug (e.g. `easi.zen-browser`, `dev.zed.Zed`, `helium`) in `KNOWN_APP_SOURCES`.
pub fn lookup_known_app_source(app_id: &str) -> Option<UpdateSource> {
    let lower = app_id.trim().to_ascii_lowercase();
    let stripped = lower.strip_prefix("easi.").unwrap_or(&lower);
    let last_seg = stripped.rsplit('.').next().unwrap_or(stripped);
    let hit = KNOWN_APP_SOURCES
        .iter()
        .find(|row| row.slug == stripped || row.slug == last_seg)?;
    Some(match hit.source {
        KnownSourceKind::GitHub(owner, repo) => UpdateSource::GitHubRelease {
            owner: owner.to_string(),
            repo: repo.to_string(),
            asset_pattern: None,
        },
        KnownSourceKind::JetBrains(code) => UpdateSource::JetBrains {
            product_code: code.to_string(),
        },
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedApp {
    pub app_id: String,
    pub name: String,
    pub version: Option<String>,
    pub kind: AppKind,
    pub update_source: Option<UpdateSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppExportFile {
    pub format_version: u32,
    pub apps: Vec<ExportedApp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    Installed {
        name: String,
        version: Option<String>,
    },
    AlreadyInstalled {
        name: String,
    },
    SkippedNoSource {
        name: String,
    },
    Failed {
        name: String,
        error: String,
    },
}

/// Export all managed apps and their recorded update sources as portable TOML.
pub fn export_apps_toml(roots: &InstallRoots) -> Result<String, EasiError> {
    let manifests = list_manifests(roots)?;
    let apps = manifests
        .into_iter()
        .map(|m| ExportedApp {
            app_id: m.app_id,
            name: m.name,
            version: m.version,
            kind: m.kind,
            update_source: m.update_source,
        })
        .collect();
    let payload = AppExportFile {
        format_version: 1,
        apps,
    };
    toml::to_string_pretty(&payload).map_err(|e| EasiError::Manifest(e.to_string()))
}

/// Reinstall apps from an exported TOML file using their recorded `UpdateSource`.
pub fn import_apps_from_toml(
    roots: &InstallRoots,
    toml_text: &str,
) -> Result<Vec<ImportOutcome>, EasiError> {
    let parsed: AppExportFile =
        toml::from_str(toml_text).map_err(|e| EasiError::Manifest(e.to_string()))?;
    let existing = list_manifests(roots).unwrap_or_default();
    let mut outcomes = Vec::new();
    for entry in parsed.apps {
        if existing.iter().any(|m| m.app_id == entry.app_id) {
            outcomes.push(ImportOutcome::AlreadyInstalled { name: entry.name });
            continue;
        }
        let Some(src) = entry
            .update_source
            .or_else(|| lookup_known_app_source(&entry.app_id))
        else {
            outcomes.push(ImportOutcome::SkippedNoSource { name: entry.name });
            continue;
        };
        if crate::roots::validate_app_id(&entry.app_id).is_err() {
            outcomes.push(ImportOutcome::Failed {
                name: entry.name,
                error: format!("refusing invalid app_id in import file: {}", entry.app_id),
            });
            continue;
        }
        let safe_dir: String = entry
            .name
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
            .collect::<String>()
            .trim()
            .trim_matches('.')
            .to_string();
        let install_dir = roots.applications_dir.join(if safe_dir.is_empty() {
            &entry.app_id
        } else {
            &safe_dir
        });
        let synthetic = Manifest {
            app_id: entry.app_id,
            name: entry.name.clone(),
            version: None,
            source_filename: String::new(),
            kind: entry.kind,
            install_dir,
            developer: None,
            origin_url: None,
            signature_summary: None,
            signer_fingerprint: None,
            update_source: Some(src),
            adopted: false,
            created_files: vec![],
            created_dirs: vec![],
        };
        match download_and_update_app(roots, &synthetic) {
            Ok(summary) => outcomes.push(ImportOutcome::Installed {
                name: summary.name,
                version: summary.version,
            }),
            Err(e) => outcomes.push(ImportOutcome::Failed {
                name: entry.name,
                error: e.to_string(),
            }),
        }
    }
    Ok(outcomes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppUpdateStatus {
    UpToDate {
        app_id: String,
        name: String,
        current_version: String,
    },
    UpdateAvailable {
        app_id: String,
        name: String,
        current_version: String,
        latest_version: String,
        release_url: String,
    },
    Unsupported {
        app_id: String,
        name: String,
    },
    CheckFailed {
        app_id: String,
        name: String,
        reason: String,
    },
}

/// Detect an upstream `UpdateSource` from priority-ordered package metadata:
/// 1. AppImage `.upd_info` section (`gh-releases-zsync|...` or `zsync|...`)
/// 2. Download origin xattr (`user.xdg.origin.url` pointing to GitHub/GitLab/Codeberg/zsync/yml)
/// 3. AppStream metainfo XML repository URL
pub fn detect_update_source(
    appimage_upd_info: Option<&str>,
    origin_url: Option<&str>,
    metainfo_xml: Option<&str>,
) -> Option<UpdateSource> {
    if let Some(upd) = appimage_upd_info
        && let Some(src) = parse_appimage_upd_info(upd)
    {
        return Some(src);
    }
    if let Some(url) = origin_url
        && let Some(src) = parse_forge_or_known_url(url)
    {
        return Some(src);
    }
    if let Some(xml) = metainfo_xml {
        for host in [
            "https://github.com/",
            "https://gitlab.com/",
            "https://codeberg.org/",
        ] {
            if let Some(idx) = xml.find(host) {
                let rest = &xml[idx..];
                let end = rest
                    .find(|c: char| c == '<' || c == '"' || c.is_whitespace())
                    .unwrap_or(rest.len());
                if let Some(src) = parse_forge_or_known_url(&rest[..end]) {
                    return Some(src);
                }
            }
        }
    }
    None
}

/// Auto-detect an upstream `UpdateSource` from real config files inside an extracted app directory:
/// 1. Electron-builder `resources/app-update.yml` or `app-update.yml` (GitHub or generic CDN YAML)
/// 2. JetBrains `product-info.json` (`productCode` -> official JetBrains CDN API)
/// 3. Firefox/Gecko `application.ini` (`SourceRepository=` -> GitHub/GitLab/Codeberg)
/// 4. `resources/app/package.json` or `package.json` (`repository` / `homepage`)
pub fn detect_dir_update_source(install_dir: &Path) -> Option<UpdateSource> {
    for rel in ["resources/app-update.yml", "app-update.yml"] {
        if let Ok(yaml) = fs::read_to_string(install_dir.join(rel))
            && let Some(src) = parse_electron_app_update_yml(&yaml)
        {
            return Some(src);
        }
    }

    if let Ok(json) = fs::read_to_string(install_dir.join("product-info.json"))
        && let Some(code) = extract_json_string_field(&json, "productCode")
        && !code.is_empty()
    {
        return Some(UpdateSource::JetBrains { product_code: code });
    }

    if let Ok(ini) = fs::read_to_string(install_dir.join("application.ini")) {
        for line in ini.lines() {
            if let Some(repo) = line.trim().strip_prefix("SourceRepository=")
                && let Some(src) = parse_forge_or_known_url(repo.trim())
            {
                return Some(src);
            }
        }
    }

    for rel in ["resources/app/package.json", "package.json"] {
        if let Ok(json) = fs::read_to_string(install_dir.join(rel)) {
            for field in ["url", "repository", "homepage"] {
                if let Some(val) = extract_json_string_field(&json, field)
                    && let Some(src) = parse_forge_or_known_url(&val)
                {
                    return Some(src);
                }
            }
        }
    }

    None
}

/// Parse Electron-builder's `app-update.yml` (supports `github`, `generic`, `s3`, `spaces`).
pub fn parse_electron_app_update_yml(yaml: &str) -> Option<UpdateSource> {
    let provider = yaml_field(yaml, "provider")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if provider == "github" {
        let owner = yaml_field(yaml, "owner")?;
        let repo = yaml_field(yaml, "repo")?;
        return Some(UpdateSource::GitHubRelease {
            owner,
            repo,
            asset_pattern: None,
        });
    }
    if matches!(
        provider.as_str(),
        "generic" | "s3" | "spaces" | "custom" | ""
    ) && let Some(url) = yaml_field(yaml, "url")
        && url.starts_with("https://")
    {
        return Some(UpdateSource::ElectronYaml { url });
    }
    None
}

/// Parse a user-supplied update source string (GitHub, GitLab, Codeberg, zsync, Electron YAML,
/// `jetbrains:<CODE>`, or any direct CDN/proxy HTTP(S) download URL).
pub fn parse_user_update_url(input: &str) -> Option<UpdateSource> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(code) = trimmed
        .strip_prefix("jetbrains:")
        .or_else(|| trimmed.strip_prefix("jb:"))
    {
        let c = code.trim().to_ascii_uppercase();
        if !c.is_empty() {
            return Some(UpdateSource::JetBrains { product_code: c });
        }
    }
    if let Some(src) = parse_forge_or_known_url(trimmed) {
        return Some(src);
    }
    // Shorthand `owner/repo` -> GitHub.
    if !trimmed.contains("://") && !trimmed.contains(' ') {
        let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() == 2 {
            return Some(UpdateSource::GitHubRelease {
                owner: parts[0].to_string(),
                repo: parts[1].trim_end_matches(".git").to_string(),
                asset_pattern: None,
            });
        }
    }
    // Any other HTTPS URL -> direct CDN / proxy redirect check (plain HTTP refused to prevent MITM).
    if trimmed.starts_with("https://") {
        return Some(UpdateSource::HttpRedirect {
            url: trimmed.to_string(),
        });
    }
    None
}

fn require_https_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "refusing non-HTTPS or malformed update URL (MITM protection): {url}"
        ));
    }
    Ok(())
}

fn is_safe_repo_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn is_safe_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
}

fn is_safe_product_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= 16 && code.chars().all(|c| c.is_ascii_alphanumeric())
}

fn parse_forge_or_known_url(raw: &str) -> Option<UpdateSource> {
    let url = raw
        .trim()
        .strip_prefix("git+")
        .unwrap_or(raw.trim())
        .trim_end_matches('/');
    if url.starts_with("https://") && url.ends_with(".zsync") {
        return Some(UpdateSource::ZsyncUrl {
            url: url.to_string(),
        });
    }
    if url.starts_with("https://") && (url.ends_with(".yml") || url.ends_with(".yaml")) {
        return Some(UpdateSource::ElectronYaml {
            url: url.to_string(),
        });
    }
    if let Some(src) = parse_github_url(url) {
        return Some(src);
    }
    for (prefix, host, forge) in [
        ("https://gitlab.com/", "gitlab.com", "gitlab"),
        ("http://gitlab.com/", "gitlab.com", "gitlab"),
        ("https://codeberg.org/", "codeberg.org", "gitea"),
        ("http://codeberg.org/", "codeberg.org", "gitea"),
    ] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
            if segs.len() >= 2 {
                let project = format!("{}/{}", segs[0], segs[1].trim_end_matches(".git"));
                return Some(UpdateSource::ForgeRelease {
                    host: host.to_string(),
                    project,
                    forge: forge.to_string(),
                });
            }
        }
    }
    None
}

/// Persist or clear a custom/auto-detected `UpdateSource` on an installed app's manifest.
pub fn set_app_update_source(
    roots: &InstallRoots,
    query: &str,
    source: Option<UpdateSource>,
) -> Result<Manifest, EasiError> {
    let mut manifest = resolve_installed(roots, query)?;
    manifest.update_source = source;
    save_manifest(roots, &manifest)?;
    Ok(manifest)
}

pub fn parse_appimage_upd_info(upd: &str) -> Option<UpdateSource> {
    let parts: Vec<&str> = upd.trim().split('|').collect();
    match parts.first().copied()? {
        "gh-releases-zsync" if parts.len() >= 3 => Some(UpdateSource::GitHubRelease {
            owner: parts[1].to_string(),
            repo: parts[2].to_string(),
            asset_pattern: parts.get(4).map(|s| (*s).to_string()),
        }),
        "zsync" if parts.len() >= 2 && parts[1].starts_with("https://") => {
            Some(UpdateSource::ZsyncUrl {
                url: parts[1].to_string(),
            })
        }
        _ => None,
    }
}

pub fn parse_github_url(url: &str) -> Option<UpdateSource> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut segs = rest.split('/').filter(|s| !s.is_empty());
    let owner = segs.next()?;
    let repo = segs.next()?.trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(UpdateSource::GitHubRelease {
        owner: owner.to_string(),
        repo: repo.to_string(),
        asset_pattern: None,
    })
}

/// Parse `Filename: ...` from a `.zsync` header and extract its version token.
pub fn parse_zsync_version(header: &str) -> Option<String> {
    for line in header.lines() {
        if let Some(fname) = line.strip_prefix("Filename:") {
            let (_, ver) = slug_and_version(fname.trim());
            if ver.is_some() {
                return ver;
            }
        }
    }
    None
}

/// Parse Electron-builder `latest-linux.yml` -> `(version, relative_or_absolute_path)`.
pub fn parse_electron_latest_yml(yaml: &str) -> Option<(String, Option<String>)> {
    let ver = yaml_field(yaml, "version")?;
    let path = yaml_field(yaml, "path").or_else(|| yaml_field(yaml, "url"));
    Some((ver.trim_start_matches('v').to_string(), path))
}

/// Parse JetBrains releases API JSON -> `(version, linux_download_link)`.
pub fn parse_jetbrains_release_json(json: &str, arch: &str) -> Option<(String, Option<String>)> {
    let version = extract_json_string_field(json, "version")?;
    let key = if arch == "aarch64" {
        "\"linuxARM64\""
    } else {
        "\"linux\""
    };
    let link = json
        .find(key)
        .and_then(|idx| extract_json_string_field(&json[idx..], "link"));
    Some((version, link))
}

fn yaml_field(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn electron_yaml_url(base_or_yml: &str, arch: &str) -> String {
    if base_or_yml.ends_with(".yml") || base_or_yml.ends_with(".yaml") {
        base_or_yml.to_string()
    } else {
        let file = if arch == "aarch64" {
            "latest-linux-arm64.yml"
        } else {
            "latest-linux.yml"
        };
        format!("{}/{file}", base_or_yml.trim_end_matches('/'))
    }
}

fn version_status(
    manifest: &Manifest,
    current: String,
    latest: String,
    release_url: String,
) -> AppUpdateStatus {
    if is_newer_version(&current, &latest) {
        AppUpdateStatus::UpdateAvailable {
            app_id: manifest.app_id.clone(),
            name: manifest.name.clone(),
            current_version: current,
            latest_version: latest,
            release_url,
        }
    } else {
        AppUpdateStatus::UpToDate {
            app_id: manifest.app_id.clone(),
            name: manifest.name.clone(),
            current_version: current,
        }
    }
}

/// Check one installed app against its recorded `UpdateSource`.
pub fn check_app_update(manifest: &Manifest) -> AppUpdateStatus {
    let Some(source) = &manifest.update_source else {
        return AppUpdateStatus::Unsupported {
            app_id: manifest.app_id.clone(),
            name: manifest.name.clone(),
        };
    };
    let current = manifest
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0".to_string());

    let fail = |reason: String| AppUpdateStatus::CheckFailed {
        app_id: manifest.app_id.clone(),
        name: manifest.name.clone(),
        reason,
    };

    match source {
        UpdateSource::GitHubRelease { owner, repo, .. } => {
            if !is_safe_repo_segment(owner) || !is_safe_repo_segment(repo) {
                return fail(format!("invalid GitHub repository: {owner}/{repo}"));
            }
            let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            let release_url = format!("https://github.com/{owner}/{repo}/releases/latest");
            match curl_text(&api, &["-fsSL", "--max-time", "5"]) {
                Ok(body) => match parse_release_json(&body) {
                    Some(release) => {
                        version_status(manifest, current, release.version, release_url)
                    }
                    None => fail("no tag_name in GitHub release response".into()),
                },
                Err(reason) => fail(reason),
            }
        }
        UpdateSource::ForgeRelease {
            host,
            project,
            forge,
        } => {
            if !is_safe_host(host) || !project.split('/').all(is_safe_repo_segment) {
                return fail(format!("invalid forge update source: {host}/{project}"));
            }
            let (api, release_url) = if forge == "gitlab" {
                let encoded = project.replace('/', "%2F");
                (
                    format!("https://{host}/api/v4/projects/{encoded}/releases?per_page=1"),
                    format!("https://{host}/{project}/-/releases"),
                )
            } else {
                (
                    format!("https://{host}/api/v1/repos/{project}/releases/latest"),
                    format!("https://{host}/{project}/releases"),
                )
            };
            match curl_text(&api, &["-fsSL", "--max-time", "5"]) {
                Ok(body) => match parse_release_json(&body) {
                    Some(release) => {
                        version_status(manifest, current, release.version, release_url)
                    }
                    None => fail(format!("no tag_name in {host} release response")),
                },
                Err(reason) => fail(reason),
            }
        }
        UpdateSource::ZsyncUrl { url } => {
            match curl_text(url, &["-fsSL", "--max-time", "5", "-r", "0-4095"]) {
                Ok(header) => match parse_zsync_version(&header) {
                    Some(latest) => version_status(manifest, current, latest, url.clone()),
                    None => fail("could not parse version from .zsync header".into()),
                },
                Err(reason) => fail(reason),
            }
        }
        UpdateSource::ElectronYaml { url } => {
            let yml_url = electron_yaml_url(url, std::env::consts::ARCH);
            match curl_text(&yml_url, &["-fsSL", "--max-time", "5"]) {
                Ok(body) => match parse_electron_latest_yml(&body) {
                    Some((latest, _)) => version_status(manifest, current, latest, yml_url),
                    None => fail("could not parse version from latest-linux.yml".into()),
                },
                Err(reason) => fail(reason),
            }
        }
        UpdateSource::JetBrains { product_code } => {
            if !is_safe_product_code(product_code) {
                return fail(format!("invalid JetBrains product code: {product_code}"));
            }
            let api = format!(
                "https://data.services.jetbrains.com/products/releases?code={product_code}&latest=true&type=release"
            );
            match curl_text(&api, &["-fsSL", "--max-time", "5"]) {
                Ok(body) => match parse_jetbrains_release_json(&body, std::env::consts::ARCH) {
                    Some((latest, link)) => version_status(
                        manifest,
                        current,
                        latest,
                        link.unwrap_or_else(|| api.clone()),
                    ),
                    None => fail("could not parse JetBrains release metadata".into()),
                },
                Err(reason) => fail(reason),
            }
        }
        UpdateSource::HttpRedirect { url } => match resolve_redirect_url(url) {
            Ok(final_url) => {
                let fname = final_url.rsplit('/').next().unwrap_or(&final_url);
                match slug_and_version(fname).1 {
                    Some(latest) => version_status(manifest, current, latest, final_url),
                    None => fail("redirect filename did not include a version number".into()),
                }
            }
            Err(reason) => fail(reason),
        },
    }
}

fn curl_text(url: &str, args: &[&str]) -> Result<String, String> {
    require_https_url(url)?;
    let out = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "10",
            "--max-filesize",
            "4194304",
        ])
        .args(args)
        .arg("--")
        .arg(url)
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("HTTP request failed for {url}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn resolve_redirect_url(url: &str) -> Result<String, String> {
    require_https_url(url)?;
    let out = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "-fsSLI",
            "--max-time",
            "5",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            "--",
        ])
        .arg(url)
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("HTTP HEAD failed for {url}"));
    }
    let final_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    require_https_url(&final_url)?;
    Ok(final_url)
}

/// A forge release reduced to what updates need, parsed with serde_json so escaped strings and
/// field order cannot confuse asset selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRelease {
    pub version: String,
    pub asset_urls: Vec<String>,
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    assets: ReleaseAssetsJson,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ReleaseAssetsJson {
    /// GitHub / Gitea / Forgejo: `assets: [{browser_download_url}]`.
    Listed(Vec<ListedAssetJson>),
    /// GitLab: `assets: {links: [{url, direct_asset_url}]}`.
    Linked { links: Vec<LinkedAssetJson> },
}

impl Default for ReleaseAssetsJson {
    fn default() -> Self {
        Self::Listed(Vec::new())
    }
}

#[derive(Deserialize)]
struct ListedAssetJson {
    browser_download_url: String,
}

#[derive(Deserialize)]
struct LinkedAssetJson {
    url: String,
    #[serde(default)]
    direct_asset_url: Option<String>,
}

/// Parse a GitHub/Gitea release object or a GitLab release list (newest first).
pub fn parse_release_json(body: &str) -> Option<ParsedRelease> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let object = match value {
        serde_json::Value::Array(list) => list.into_iter().next()?,
        other => other,
    };
    let release: ReleaseJson = serde_json::from_value(object).ok()?;
    let asset_urls = match release.assets {
        ReleaseAssetsJson::Listed(list) => {
            list.into_iter().map(|a| a.browser_download_url).collect()
        }
        ReleaseAssetsJson::Linked { links } => links
            .into_iter()
            .map(|l| l.direct_asset_url.unwrap_or(l.url))
            .collect(),
    };
    Some(ParsedRelease {
        version: release.tag_name.trim_start_matches('v').to_string(),
        asset_urls,
    })
}

/// Score forge release asset URLs and pick the best match for the current Linux arch and
/// previously installed package extension.
pub fn select_best_github_asset(
    urls: &[String],
    arch: &str,
    prev_filename: &str,
) -> Option<String> {
    let prev_lower = prev_filename.to_ascii_lowercase();
    let arch_tokens: &[&str] = match arch {
        "aarch64" => &["aarch64", "arm64"],
        _ => &["x86_64", "amd64", "x64"],
    };
    let wrong_arch: &[&str] = match arch {
        "aarch64" => &[
            "x86_64", "amd64", "x64", "win", "darwin", "macos", ".dmg", ".exe", ".msi",
        ],
        _ => &[
            "aarch64", "arm64", "win", "darwin", "macos", ".dmg", ".exe", ".msi",
        ],
    };

    let mut best: Option<(i32, &String)> = None;
    for url in urls {
        if !url.starts_with("https://") {
            continue;
        }
        let lower = url.to_ascii_lowercase();
        if lower.ends_with(".sha256")
            || lower.ends_with(".sig")
            || lower.ends_with(".asc")
            || lower.ends_with(".zsync")
            || lower.ends_with(".deb")
            || lower.ends_with(".rpm")
            || lower.ends_with(".blockmap")
        {
            continue;
        }
        if wrong_arch.iter().any(|w| lower.contains(w)) {
            continue;
        }
        let mut score = 0i32;
        if arch_tokens.iter().any(|a| lower.contains(a)) {
            score += 40;
        }
        if lower.contains("linux") {
            score += 20;
        }
        const PKG_EXTS: &[&str] = &[".appimage", ".tar.xz", ".tar.gz", ".tar.zst", ".zip"];
        if PKG_EXTS
            .iter()
            .any(|ext| prev_lower.ends_with(ext) && lower.ends_with(ext))
        {
            score += 50;
        } else if PKG_EXTS.iter().any(|ext| lower.ends_with(ext)) {
            score += 25;
        }
        if score > 0 && best.is_none_or(|(bs, _)| score > bs) {
            best = Some((score, url));
        }
    }
    best.map(|(_, u)| u.clone())
}

/// Download the latest release for `manifest` into a temp dir and run a transactional replace.
pub fn download_and_update_app(
    roots: &InstallRoots,
    manifest: &Manifest,
) -> Result<InstallSummary, EasiError> {
    let source = manifest.update_source.as_ref().ok_or_else(|| {
        EasiError::Desktop(format!(
            "{} has no recorded upstream update source",
            manifest.name
        ))
    })?;

    let (download_url, known_latest_version): (String, Option<String>) = match source {
        UpdateSource::GitHubRelease { owner, repo, .. } => {
            if !is_safe_repo_segment(owner) || !is_safe_repo_segment(repo) {
                return Err(EasiError::Desktop(format!(
                    "invalid GitHub repository: {owner}/{repo}"
                )));
            }
            let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            let body =
                curl_text(&api, &["-fsSL", "--max-time", "15"]).map_err(EasiError::Desktop)?;
            let release = parse_release_json(&body).ok_or_else(|| {
                EasiError::Desktop("could not parse GitHub release response".into())
            })?;
            let url = select_best_github_asset(
                &release.asset_urls,
                std::env::consts::ARCH,
                &manifest.source_filename,
            )
            .ok_or_else(|| {
                EasiError::Desktop(format!(
                    "no matching Linux release asset found for {} ({})",
                    manifest.name,
                    std::env::consts::ARCH
                ))
            })?;
            (url, Some(release.version))
        }
        UpdateSource::ForgeRelease {
            host,
            project,
            forge,
        } => {
            if !is_safe_host(host) || !project.split('/').all(is_safe_repo_segment) {
                return Err(EasiError::Desktop(format!(
                    "invalid forge update source: {host}/{project}"
                )));
            }
            let api = if forge == "gitlab" {
                let encoded = project.replace('/', "%2F");
                format!("https://{host}/api/v4/projects/{encoded}/releases?per_page=1")
            } else {
                format!("https://{host}/api/v1/repos/{project}/releases/latest")
            };
            let body =
                curl_text(&api, &["-fsSL", "--max-time", "15"]).map_err(EasiError::Desktop)?;
            let release = parse_release_json(&body).ok_or_else(|| {
                EasiError::Desktop(format!("could not parse {host} release response"))
            })?;
            let url = select_best_github_asset(
                &release.asset_urls,
                std::env::consts::ARCH,
                &manifest.source_filename,
            )
            .ok_or_else(|| {
                EasiError::Desktop(format!(
                    "no matching Linux release asset found on {host} for {}",
                    manifest.name
                ))
            })?;
            (url, Some(release.version))
        }
        UpdateSource::ZsyncUrl { url } => {
            let header = curl_text(url, &["-fsSL", "--max-time", "10", "-r", "0-4095"])
                .map_err(EasiError::Desktop)?;
            let ver = parse_zsync_version(&header);
            let dl = resolve_zsync_download_url(url, &header).ok_or_else(|| {
                EasiError::Desktop(format!("could not resolve download URL from {url}"))
            })?;
            (dl, ver)
        }
        UpdateSource::ElectronYaml { url } => {
            let yml_url = electron_yaml_url(url, std::env::consts::ARCH);
            let body =
                curl_text(&yml_url, &["-fsSL", "--max-time", "10"]).map_err(EasiError::Desktop)?;
            let (ver, path_opt) = parse_electron_latest_yml(&body).ok_or_else(|| {
                EasiError::Desktop(format!("could not parse download path from {yml_url}"))
            })?;
            let rel = path_opt
                .ok_or_else(|| EasiError::Desktop(format!("no file path listed in {yml_url}")))?;
            let dl = if rel.starts_with("https://") {
                rel
            } else if rel.starts_with("http://") {
                return Err(EasiError::Desktop(format!(
                    "refusing non-HTTPS update URL in {yml_url}: {rel}"
                )));
            } else if let Some((base, _)) = yml_url.rsplit_once('/') {
                format!("{base}/{rel}")
            } else {
                rel
            };
            (dl, Some(ver))
        }
        UpdateSource::JetBrains { product_code } => {
            if !is_safe_product_code(product_code) {
                return Err(EasiError::Desktop(format!(
                    "invalid JetBrains product code: {product_code}"
                )));
            }
            let api = format!(
                "https://data.services.jetbrains.com/products/releases?code={product_code}&latest=true&type=release"
            );
            let body =
                curl_text(&api, &["-fsSL", "--max-time", "15"]).map_err(EasiError::Desktop)?;
            let (ver, link) = parse_jetbrains_release_json(&body, std::env::consts::ARCH)
                .ok_or_else(|| {
                    EasiError::Desktop(format!(
                        "no Linux download found on JetBrains CDN for {product_code}"
                    ))
                })?;
            let dl = link.ok_or_else(|| {
                EasiError::Desktop(format!(
                    "no Linux download found on JetBrains CDN for {product_code}"
                ))
            })?;
            (dl, Some(ver))
        }
        UpdateSource::HttpRedirect { url } => {
            let final_url = resolve_redirect_url(url).unwrap_or_else(|_| url.clone());
            let fname = final_url.rsplit('/').next().unwrap_or(&final_url);
            let ver = slug_and_version(fname).1;
            (final_url, ver)
        }
    };

    let tmp = TempDir::new()?;
    let raw_fname = download_url
        .split('?')
        .next()
        .unwrap_or(&download_url)
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .unwrap_or("update.pkg");
    let dest = crate::roots::safe_join(tmp.path(), Path::new(raw_fname))?;
    download_to_file(&download_url, &dest)?;

    let classified = classify_package(&dest, |_| Some(manifest.to_installed_app()))?;
    let mut plan = match classified {
        Classified::Ready { plan, .. } | Classified::Conflict { plan, .. } => plan,
        Classified::NeedsChoice { draft, .. } => draft.into_plan(),
        Classified::NotAnApp { reason, .. } => {
            return Err(EasiError::Desktop(format!(
                "downloaded asset is not an installable app: {reason}"
            )));
        }
    };
    // Security guard: NEVER auto-execute an untrusted shell script during an automated update!
    if plan.format == crate::model::Format::ShellInstaller {
        return Err(EasiError::Desktop(
            "refusing to execute a shell installer script during automated update — download and review it manually"
                .into(),
        ));
    }
    // Preserve original app_id and update_source across updates, and stamp the authoritative
    // upstream release version when the downloaded asset filename omits it (e.g. zed-linux-x86_64.tar.gz).
    plan.app_id = manifest.app_id.clone();
    if known_latest_version.is_some() {
        plan.version = known_latest_version;
    }
    if plan.update_source.is_none() {
        plan.update_source = manifest.update_source.clone();
    }

    let summary = execute_install(roots, &plan, true, &AtomicBool::new(false), |_, _| {})?;
    crate::desktop::notify(
        "App updated",
        &format!(
            "Updated {} to {}.",
            summary.name,
            summary.version.as_deref().unwrap_or("latest")
        ),
    );
    Ok(summary)
}

fn resolve_zsync_download_url(zsync_url: &str, header: &str) -> Option<String> {
    for prefix in ["URL:", "Filename:"] {
        for line in header.lines() {
            if let Some(val) = line.strip_prefix(prefix) {
                let v = val.trim();
                if v.starts_with("https://") {
                    return Some(v.to_string());
                }
                if v.starts_with("http://") {
                    return None;
                }
                if !v.is_empty()
                    && let Some((base, _)) = zsync_url.rsplit_once('/')
                {
                    return Some(format!("{base}/{v}"));
                }
            }
        }
    }
    None
}

fn download_to_file(url: &str, dest: &Path) -> Result<(), EasiError> {
    require_https_url(url).map_err(EasiError::Desktop)?;
    let out = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "600",
            "--max-filesize",
            "2147483648",
            "-fsSL",
            "-o",
        ])
        .arg(dest)
        .arg("--")
        .arg(url)
        .output()
        .map_err(|e| EasiError::Desktop(format!("curl failed: {e}")))?;
    if !out.status.success() {
        return Err(EasiError::Desktop(format!("failed to download {url}")));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_json_across_forges() {
        let rows: &[(&str, &str, Option<ParsedRelease>)] = &[
            (
                "github with escaped quote in body",
                r#"{"tag_name":"v1.2.0","body":"say \"browser_download_url\": \"https://evil\"","assets":[{"name":"a","browser_download_url":"https://github.com/o/r/releases/download/v1.2.0/app-x86_64.AppImage"}]}"#,
                Some(ParsedRelease {
                    version: "1.2.0".into(),
                    asset_urls: vec![
                        "https://github.com/o/r/releases/download/v1.2.0/app-x86_64.AppImage"
                            .into(),
                    ],
                }),
            ),
            (
                "gitlab list, newest first, prefers direct_asset_url",
                r#"[{"tag_name":"2.0","assets":{"links":[{"url":"https://cdn.example/x.tar.gz","direct_asset_url":"https://gitlab.com/g/p/-/releases/2.0/downloads/x.tar.gz"}]}},{"tag_name":"1.0"}]"#,
                Some(ParsedRelease {
                    version: "2.0".into(),
                    asset_urls: vec![
                        "https://gitlab.com/g/p/-/releases/2.0/downloads/x.tar.gz".into(),
                    ],
                }),
            ),
            (
                "release without assets",
                r#"{"tag_name":"v3"}"#,
                Some(ParsedRelease {
                    version: "3".into(),
                    asset_urls: vec![],
                }),
            ),
            ("missing tag", r#"{"assets":[]}"#, None),
            ("empty gitlab list", "[]", None),
            ("not json", "<html>rate limited</html>", None),
        ];
        for (name, body, want) in rows {
            assert_eq!(&parse_release_json(body), want, "{name}");
        }
    }

    #[test]
    fn parses_appimage_upd_info_github_and_zsync_headers() {
        assert_eq!(
            parse_appimage_upd_info(
                "gh-releases-zsync|obsidianmd|obsidian-releases|latest|Obsidian-*x86_64.AppImage.zsync"
            ),
            Some(UpdateSource::GitHubRelease {
                owner: "obsidianmd".into(),
                repo: "obsidian-releases".into(),
                asset_pattern: Some("Obsidian-*x86_64.AppImage.zsync".into()),
            })
        );

        assert_eq!(
            parse_appimage_upd_info("zsync|https://example.com/releases/CoolApp.AppImage.zsync"),
            Some(UpdateSource::ZsyncUrl {
                url: "https://example.com/releases/CoolApp.AppImage.zsync".into(),
            })
        );

        assert_eq!(
            parse_github_url(
                "https://github.com/BurntSushi/ripgrep/releases/download/14.1.0/ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz"
            ),
            Some(UpdateSource::GitHubRelease {
                owner: "BurntSushi".into(),
                repo: "ripgrep".into(),
                asset_pattern: None,
            })
        );

        let zsync =
            "zsync: 0.6.2\nFilename: CoolApp-2.4.1-x86_64.AppImage\nMTime: Mon, 01 Jan 2026\n";
        assert_eq!(parse_zsync_version(zsync).as_deref(), Some("2.4.1"));
    }

    #[test]
    fn detects_cdn_electron_jetbrains_gecko_and_user_urls() {
        let tmp = TempDir::new().unwrap();

        // 1. Electron generic CDN (non-GitHub!)
        let elec_dir = tmp.path().join("electron-app");
        fs::create_dir_all(elec_dir.join("resources")).unwrap();
        fs::write(
            elec_dir.join("resources/app-update.yml"),
            "provider: generic\nurl: https://cdn.example.com/releases/linux\n",
        )
        .unwrap();
        assert_eq!(
            detect_dir_update_source(&elec_dir),
            Some(UpdateSource::ElectronYaml {
                url: "https://cdn.example.com/releases/linux".into()
            })
        );

        // 2. JetBrains product-info.json
        let jb_dir = tmp.path().join("webstorm");
        fs::create_dir_all(&jb_dir).unwrap();
        fs::write(
            jb_dir.join("product-info.json"),
            r#"{"name":"WebStorm","version":"2026.1","productCode":"WS"}"#,
        )
        .unwrap();
        assert_eq!(
            detect_dir_update_source(&jb_dir),
            Some(UpdateSource::JetBrains {
                product_code: "WS".into()
            })
        );

        // 3. Firefox/Zen application.ini
        let zen_dir = tmp.path().join("zen");
        fs::create_dir_all(&zen_dir).unwrap();
        fs::write(
            zen_dir.join("application.ini"),
            "[App]\nName=Zen\nVersion=1.12.5b\nSourceRepository=https://github.com/zen-browser/desktop\n",
        )
        .unwrap();
        assert_eq!(
            detect_dir_update_source(&zen_dir),
            Some(UpdateSource::GitHubRelease {
                owner: "zen-browser".into(),
                repo: "desktop".into(),
                asset_pattern: None
            })
        );

        // 4. Parse latest-linux.yml + JetBrains JSON + user CDN/forge URLs
        let yml = "version: 3.2.1\npath: MyApp-3.2.1-x86_64.AppImage\n";
        assert_eq!(
            parse_electron_latest_yml(yml),
            Some(("3.2.1".into(), Some("MyApp-3.2.1-x86_64.AppImage".into())))
        );

        let jb_json = r#"{"WS":[{"version":"2026.1.2","downloads":{"linux":{"link":"https://download.jetbrains.com/webstorm/WebStorm-2026.1.2.tar.gz"}}}]}"#;
        assert_eq!(
            parse_jetbrains_release_json(jb_json, "x86_64"),
            Some((
                "2026.1.2".into(),
                Some("https://download.jetbrains.com/webstorm/WebStorm-2026.1.2.tar.gz".into())
            ))
        );

        assert_eq!(
            parse_user_update_url("https://gitlab.com/foo/bar"),
            Some(UpdateSource::ForgeRelease {
                host: "gitlab.com".into(),
                project: "foo/bar".into(),
                forge: "gitlab".into()
            })
        );
        assert_eq!(
            parse_user_update_url("https://downloads.vendor.com/latest/app-linux.tar.gz"),
            Some(UpdateSource::HttpRedirect {
                url: "https://downloads.vendor.com/latest/app-linux.tar.gz".into()
            })
        );

        // 5. Known app sources lookup table
        assert_eq!(
            lookup_known_app_source("dev.zed.Zed"),
            Some(UpdateSource::GitHubRelease {
                owner: "zed-industries".into(),
                repo: "zed".into(),
                asset_pattern: None
            })
        );
        assert_eq!(
            lookup_known_app_source("easi.webstorm"),
            Some(UpdateSource::JetBrains {
                product_code: "WS".into()
            })
        );
        assert_eq!(
            lookup_known_app_source("zen-browser"),
            Some(UpdateSource::GitHubRelease {
                owner: "zen-browser".into(),
                repo: "desktop".into(),
                asset_pattern: None
            })
        );

        // 6. Export TOML round-trip
        let roots = InstallRoots::from_home(tmp.path());
        let sample = Manifest {
            app_id: "zen-browser".into(),
            name: "Zen Browser".into(),
            version: Some("1.24.3b".into()),
            source_filename: "zen.tar.xz".into(),
            kind: AppKind::Gui,
            install_dir: roots.applications_dir.join("Zen"),
            developer: None,
            origin_url: None,
            signature_summary: None,
            signer_fingerprint: None,
            update_source: lookup_known_app_source("zen-browser"),
            adopted: true,
            created_files: vec![],
            created_dirs: vec![],
        };
        save_manifest(&roots, &sample).unwrap();
        let exported = export_apps_toml(&roots).unwrap();
        assert!(exported.contains("zen-browser"));
        assert!(
            exported.contains("zen-browser/desktop") || exported.contains("repo = \"desktop\"")
        );

        // 7. Security: plain HTTP and flag-injection strings are refused
        assert_eq!(
            parse_user_update_url("http://evil.example/app.tar.gz"),
            None
        );
        assert_eq!(parse_user_update_url("--output=/tmp/pwn"), None);
        assert_eq!(
            parse_electron_app_update_yml("provider: generic\nurl: http://evil.example/updates\n"),
            None
        );
        assert_eq!(
            parse_appimage_upd_info("zsync|http://evil.example/app.zsync"),
            None
        );
        assert_eq!(
            resolve_zsync_download_url(
                "https://good.example/app.zsync",
                "URL: http://evil.example/app.AppImage\n"
            ),
            None
        );
    }
}
