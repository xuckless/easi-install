//! Self-update engine (`easi self-update`): fetches latest GitHub Release, verifies SHA-256
//! (and optional minisign), extracts safely, and atomically swaps the running binary.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::error::EasiError;
use crate::install::extract_archive_safe;
use crate::model::Format;

pub const DEFAULT_REPO: &str = "xuckl/easi-install";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    AlreadyUpToDate {
        version: String,
    },
    Updated {
        from: String,
        to: String,
        bin_path: PathBuf,
    },
}

pub fn run_self_update(
    current_version: &str,
    target_bin: &Path,
) -> Result<UpdateOutcome, EasiError> {
    let repo = std::env::var("EASI_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let arch = current_arch()?;
    let latest_tag = fetch_latest_tag(&repo)?;
    let latest_ver = latest_tag.trim_start_matches('v');

    if !is_newer_version(current_version, latest_ver) {
        return Ok(UpdateOutcome::AlreadyUpToDate {
            version: current_version.to_string(),
        });
    }

    let tmp = TempDir::new()?;
    let tarball_name = format!("easi-{latest_tag}-linux-{arch}.tar.gz");
    let base_url = format!("https://github.com/{repo}/releases/download/{latest_tag}");

    let tarball_path = tmp.path().join(&tarball_name);
    let sums_path = tmp.path().join("SHA256SUMS");

    download_file(&format!("{base_url}/{tarball_name}"), &tarball_path)?;
    download_file(&format!("{base_url}/SHA256SUMS"), &sums_path)?;

    if let Ok(pubkey) = std::env::var("EASI_MINISIGN_PUBKEY")
        && !pubkey.is_empty()
    {
        let sig_path = tmp.path().join("SHA256SUMS.minisig");
        download_file(&format!("{base_url}/SHA256SUMS.minisig"), &sig_path)?;
        let out = Command::new("minisign")
            .args(["-V", "-P", &pubkey, "-m"])
            .arg(&sums_path)
            .output()
            .map_err(|e| EasiError::Desktop(format!("minisign failed to run: {e}")))?;
        if !out.status.success() {
            return Err(EasiError::Desktop(
                "minisign verification failed for release SHA256SUMS".into(),
            ));
        }
    }

    let sums_text = fs::read_to_string(&sums_path)?;
    let expected = parse_sha256sums(&sums_text, &tarball_name).ok_or_else(|| {
        EasiError::Desktop(format!("checksum for {tarball_name} missing in SHA256SUMS"))
    })?;
    let actual = sha256_file(&tarball_path)?;
    if actual != expected {
        return Err(EasiError::Desktop(format!(
            "SHA-256 mismatch for {tarball_name}: expected {expected}, got {actual}"
        )));
    }

    let extract_dir = tmp.path().join("extract");
    fs::create_dir_all(&extract_dir)?;
    extract_archive_safe(&tarball_path, Format::TarGz, &extract_dir)?;

    let new_bin = extract_dir.join("easi/easi");
    if !new_bin.exists() {
        return Err(EasiError::Archive(
            "release archive missing easi/easi binary".into(),
        ));
    }

    let parent = target_bin.parent().ok_or_else(|| {
        EasiError::Desktop(format!("invalid executable path: {}", target_bin.display()))
    })?;
    let staged = parent.join(".easi.update.tmp");
    fs::copy(&new_bin, &staged)?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    if let Err(err) = fs::rename(&staged, target_bin) {
        let _ = fs::remove_file(&staged);
        return Err(EasiError::Io(err));
    }

    Ok(UpdateOutcome::Updated {
        from: current_version.to_string(),
        to: latest_ver.to_string(),
        bin_path: target_bin.to_path_buf(),
    })
}

fn current_arch() -> Result<&'static str, EasiError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        other => Err(EasiError::Desktop(format!(
            "unsupported architecture for self-update: {other}"
        ))),
    }
}

fn fetch_latest_tag(repo: &str) -> Result<String, EasiError> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let out = Command::new("curl")
        .args(["-fsSL", &url])
        .output()
        .map_err(|e| EasiError::Desktop(format!("failed to invoke curl: {e}")))?;
    if !out.status.success() {
        return Err(EasiError::Desktop(format!(
            "failed to query GitHub releases for {repo}"
        )));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    extract_json_string_field(&body, "tag_name").ok_or_else(|| {
        EasiError::Desktop("could not parse tag_name from GitHub API response".into())
    })
}

fn download_file(url: &str, dest: &Path) -> Result<(), EasiError> {
    let out = Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(dest)
        .output()
        .map_err(|e| EasiError::Desktop(format!("failed to invoke curl: {e}")))?;
    if !out.status.success() {
        return Err(EasiError::Desktop(format!("download failed: {url}")));
    }
    Ok(())
}

pub fn extract_json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = json.find(&needle)? + needle.len();
    let rest = json[idx..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub fn parse_sha256sums(text: &str, filename: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == filename && hash.len() == 64 {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

pub fn sha256_file(path: &Path) -> Result<String, EasiError> {
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
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Compare dotted numeric semver strings (`0.1.0` < `0.2.0`).
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-'])
            .take_while(|p| p.chars().all(|c| c.is_ascii_digit()))
            .filter_map(|p| p.parse::<u64>().ok())
            .collect()
    };
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_and_sha256sums_parsing() {
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        assert!(!is_newer_version("0.2.0", "0.2.0"));
        assert!(!is_newer_version("0.2.0", "0.1.9"));

        let sums = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  easi-v0.2.0-linux-x86_64.tar.gz\n";
        assert_eq!(
            parse_sha256sums(sums, "easi-v0.2.0-linux-x86_64.tar.gz").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(parse_sha256sums(sums, "other.tar.gz"), None);

        let json = r#"{"url":"https://...","tag_name": "v0.2.0","draft":false}"#;
        assert_eq!(
            extract_json_string_field(json, "tag_name").as_deref(),
            Some("v0.2.0")
        );
    }
}
