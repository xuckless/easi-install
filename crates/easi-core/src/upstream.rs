//! Upstream update checks for installed apps (AppImage `.upd_info` zsync & GitHub Releases).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use tempfile::TempDir;

use crate::appid::slug_and_version;
use crate::error::EasiError;
use crate::inspect::classify_package;
use crate::install::execute_install;
use crate::manifest::Manifest;
use crate::model::{Classified, InstallSummary, UpdateSource};
use crate::roots::InstallRoots;
use crate::self_update::{extract_json_string_field, is_newer_version};

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

/// Detect an upstream `UpdateSource` from priority-ordered metadata:
/// 1. AppImage `.upd_info` section (`gh-releases-zsync|...` or `zsync|...`)
/// 2. Download origin xattr (`user.xdg.origin.url` pointing to GitHub Releases)
/// 3. AppStream metainfo XML GitHub URL
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
        && let Some(src) = parse_github_url(url)
    {
        return Some(src);
    }
    if let Some(xml) = metainfo_xml
        && let Some(idx) = xml.find("https://github.com/")
    {
        let rest = &xml[idx..];
        let end = rest
            .find(|c: char| c == '<' || c == '"' || c.is_whitespace())
            .unwrap_or(rest.len());
        if let Some(src) = parse_github_url(&rest[..end]) {
            return Some(src);
        }
    }
    None
}

pub fn parse_appimage_upd_info(upd: &str) -> Option<UpdateSource> {
    let parts: Vec<&str> = upd.trim().split('|').collect();
    match parts.first().copied()? {
        "gh-releases-zsync" if parts.len() >= 3 => Some(UpdateSource::GitHubRelease {
            owner: parts[1].to_string(),
            repo: parts[2].to_string(),
            asset_pattern: parts.get(4).map(|s| (*s).to_string()),
        }),
        "zsync" if parts.len() >= 2 => Some(UpdateSource::ZsyncUrl {
            url: parts[1].to_string(),
        }),
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

    match source {
        UpdateSource::GitHubRelease { owner, repo, .. } => {
            let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            let release_url = format!("https://github.com/{owner}/{repo}/releases/latest");
            match curl_text(&api, &["-fsSL", "--max-time", "5"]) {
                Ok(body) => match extract_json_string_field(&body, "tag_name") {
                    Some(tag) => {
                        let latest = tag.trim_start_matches('v').to_string();
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
                    None => AppUpdateStatus::CheckFailed {
                        app_id: manifest.app_id.clone(),
                        name: manifest.name.clone(),
                        reason: "no tag_name in GitHub release response".into(),
                    },
                },
                Err(reason) => AppUpdateStatus::CheckFailed {
                    app_id: manifest.app_id.clone(),
                    name: manifest.name.clone(),
                    reason,
                },
            }
        }
        UpdateSource::ZsyncUrl { url } => {
            match curl_text(url, &["-fsSL", "--max-time", "5", "-r", "0-4095"]) {
                Ok(header) => match parse_zsync_version(&header) {
                    Some(latest) => {
                        if is_newer_version(&current, &latest) {
                            AppUpdateStatus::UpdateAvailable {
                                app_id: manifest.app_id.clone(),
                                name: manifest.name.clone(),
                                current_version: current,
                                latest_version: latest,
                                release_url: url.clone(),
                            }
                        } else {
                            AppUpdateStatus::UpToDate {
                                app_id: manifest.app_id.clone(),
                                name: manifest.name.clone(),
                                current_version: current,
                            }
                        }
                    }
                    None => AppUpdateStatus::CheckFailed {
                        app_id: manifest.app_id.clone(),
                        name: manifest.name.clone(),
                        reason: "could not parse version from .zsync header".into(),
                    },
                },
                Err(reason) => AppUpdateStatus::CheckFailed {
                    app_id: manifest.app_id.clone(),
                    name: manifest.name.clone(),
                    reason,
                },
            }
        }
    }
}

fn curl_text(url: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("curl")
        .args(args)
        .arg(url)
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("HTTP request failed for {url}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Extract all `browser_download_url` values from a GitHub Releases JSON payload.
pub fn extract_github_asset_urls(json: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let needle = "\"browser_download_url\"";
    let mut rest = json;
    while let Some(idx) = rest.find(needle) {
        rest = &rest[idx + needle.len()..];
        if let Some(after_colon) = rest.trim_start().strip_prefix(':')
            && let Some(after_quote) = after_colon.trim_start().strip_prefix('"')
            && let Some(end) = after_quote.find('"')
        {
            urls.push(after_quote[..end].to_string());
            rest = &after_quote[end + 1..];
        }
    }
    urls
}

/// Score GitHub release asset URLs and pick the best match for the current Linux arch and
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

    let download_url = match source {
        UpdateSource::GitHubRelease { owner, repo, .. } => {
            let api = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            let body =
                curl_text(&api, &["-fsSL", "--max-time", "15"]).map_err(EasiError::Desktop)?;
            let urls = extract_github_asset_urls(&body);
            select_best_github_asset(&urls, std::env::consts::ARCH, &manifest.source_filename)
                .ok_or_else(|| {
                    EasiError::Desktop(format!(
                        "no matching Linux release asset found for {} ({})",
                        manifest.name,
                        std::env::consts::ARCH
                    ))
                })?
        }
        UpdateSource::ZsyncUrl { url } => {
            let header = curl_text(url, &["-fsSL", "--max-time", "10", "-r", "0-4095"])
                .map_err(EasiError::Desktop)?;
            resolve_zsync_download_url(url, &header).ok_or_else(|| {
                EasiError::Desktop(format!("could not resolve download URL from {url}"))
            })?
        }
    };

    let tmp = TempDir::new()?;
    let fname = download_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("update.pkg");
    let dest = tmp.path().join(fname);
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
    // Preserve original app_id and update_source across updates.
    plan.app_id = manifest.app_id.clone();
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
                if v.starts_with("http://") || v.starts_with("https://") {
                    return Some(v.to_string());
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
    let out = Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(dest)
        .output()
        .map_err(|e| EasiError::Desktop(format!("curl failed: {e}")))?;
    if !out.status.success() {
        return Err(EasiError::Desktop(format!("failed to download {url}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
