//! Self-update engine (`easi self-update`): fetches latest GitHub Release, verifies SHA-256
//! and the pinned minisign signature, extracts safely, and atomically swaps the running binary.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::error::EasiError;
use crate::install::extract_archive_safe;
use crate::model::Format;
use crate::roots::InstallRoots;

pub const DEFAULT_REPO: &str = "xuckless/easi-install";
pub const DEFAULT_MINISIGN_PUBKEY: &str =
    "RWQnPmfJNaxDO2ecscxiezHEPZmmtizudyau8OENs5gw21DZWlmc2IRQ";
/// Bump this ONLY when onboarding permissions/capabilities change meaningfully so existing users
/// are not re-prompted on normal updates.
pub const CURRENT_ONBOARDING_VERSION: u32 = 1;
pub const PRIVACY_POLICY_TEXT: &str = include_str!("../../../PRIVACY.md");
const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;

fn default_true() -> bool {
    true
}

/// User configuration persisted in `~/.local/share/easi/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EasiConfig {
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
    #[serde(default)]
    pub last_update_check_epoch: u64,
    #[serde(default)]
    pub latest_known_version: Option<String>,
    #[serde(default)]
    pub onboarding_version: u32,
}

impl Default for EasiConfig {
    fn default() -> Self {
        Self {
            auto_check_updates: true,
            last_update_check_epoch: 0,
            latest_known_version: None,
            onboarding_version: 0,
        }
    }
}

pub fn needs_onboarding(roots: &InstallRoots) -> bool {
    load_config(roots).onboarding_version < CURRENT_ONBOARDING_VERSION
}

pub fn complete_onboarding(roots: &InstallRoots) -> Result<EasiConfig, EasiError> {
    let mut cfg = load_config(roots);
    cfg.onboarding_version = CURRENT_ONBOARDING_VERSION;
    save_config(roots, &cfg)?;
    Ok(cfg)
}

pub fn load_config(roots: &InstallRoots) -> EasiConfig {
    fs::read_to_string(roots.config_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(roots: &InstallRoots, cfg: &EasiConfig) -> Result<PathBuf, EasiError> {
    fs::create_dir_all(&roots.easi_dir)?;
    let path = roots.config_path();
    let text = toml::to_string_pretty(cfg).map_err(|e| EasiError::Desktop(e.to_string()))?;
    fs::write(&path, text)?;
    Ok(path)
}

pub fn set_auto_check_updates(
    roots: &InstallRoots,
    enabled: bool,
) -> Result<EasiConfig, EasiError> {
    let mut cfg = load_config(roots);
    cfg.auto_check_updates = enabled;
    save_config(roots, &cfg)?;
    Ok(cfg)
}

pub fn auto_update_disabled_by_env() -> bool {
    std::env::var("EASI_NO_AUTO_UPDATE")
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// Brew-style update check on use: respects `EASI_NO_AUTO_UPDATE` and `config.toml`,
/// caches results for 6 hours, and uses a short 2s timeout so offline use never stalls.
/// Returns `Some(latest_version)` if a newer release is available.
pub fn check_for_easi_update(roots: &InstallRoots, current_version: &str) -> Option<String> {
    if auto_update_disabled_by_env() {
        return None;
    }
    let mut cfg = load_config(roots);
    if !cfg.auto_check_updates {
        return None;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if now.saturating_sub(cfg.last_update_check_epoch) < CHECK_INTERVAL_SECS {
        return cfg
            .latest_known_version
            .filter(|v| is_newer_version(current_version, v));
    }

    let repo = std::env::var("EASI_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
    let latest_tag = fetch_latest_tag_with_timeout(&repo, 2).ok()?;
    let latest_ver = latest_tag.trim_start_matches('v').to_string();

    cfg.last_update_check_epoch = now;
    cfg.latest_known_version = Some(latest_ver.clone());
    let _ = save_config(roots, &cfg);

    if is_newer_version(current_version, &latest_ver) {
        Some(latest_ver)
    } else {
        None
    }
}

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

    let sig_path = tmp.path().join("SHA256SUMS.minisig");
    // Fail-closed: a release without a signature is never installed.
    download_file(&format!("{base_url}/SHA256SUMS.minisig"), &sig_path)?;
    // Read SHA256SUMS once so the bytes we verify are the bytes we trust.
    let sums = fs::read(&sums_path)?;
    verify_sums_signature(
        &sums,
        &fs::read_to_string(&sig_path)?,
        DEFAULT_MINISIGN_PUBKEY,
        &latest_tag,
    )?;

    let sums_text = String::from_utf8_lossy(&sums);
    let expected = parse_sha256sums(&sums_text, &tarball_name).ok_or_else(|| {
        EasiError::Desktop(format!("checksum for {tarball_name} missing in SHA256SUMS"))
    })?;
    let actual = file_digest_hex::<Sha256>(&tarball_path)?;
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
    if let Ok(roots) = InstallRoots::default_user() {
        crate::desktop::ensure_easi_desktop_and_icon(&roots, target_bin);
    }

    Ok(UpdateOutcome::Updated {
        from: current_version.to_string(),
        to: latest_ver.to_string(),
        bin_path: target_bin.to_path_buf(),
    })
}

/// Verify `sums` against a minisign signature from `pubkey` and require its signed trusted
/// comment to carry the exact token `tag:{tag}`, so an older validly signed release cannot be
/// replayed as the latest one.
pub fn verify_sums_signature(
    sums: &[u8],
    sig_text: &str,
    pubkey: &str,
    tag: &str,
) -> Result<(), EasiError> {
    let fail = |e: minisign_verify::Error| EasiError::Signature(e.to_string());
    let key = minisign_verify::PublicKey::from_base64(pubkey).map_err(fail)?;
    let sig = minisign_verify::Signature::decode(sig_text).map_err(fail)?;
    // Legacy (non-prehashed) signatures are refused; release.yml signs with current minisign.
    key.verify(sums, &sig, false).map_err(fail)?;
    let wanted = format!("tag:{tag}");
    if !sig
        .trusted_comment()
        .split_whitespace()
        .any(|t| t == wanted)
    {
        return Err(EasiError::Signature(format!(
            "signed trusted comment does not name release {tag}"
        )));
    }
    Ok(())
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
    fetch_latest_tag_with_timeout(repo, 15)
}

fn fetch_latest_tag_with_timeout(repo: &str, timeout_secs: u32) -> Result<String, EasiError> {
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        || repo.contains("..")
    {
        return Err(EasiError::Desktop(format!("invalid EASI_REPO: {repo}")));
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let timeout = timeout_secs.to_string();
    let out = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-filesize",
            "4194304",
            "--max-time",
            &timeout,
            "--",
            &url,
        ])
        .output()
        .map_err(|e| EasiError::Desktop(format!("failed to invoke curl: {e}")))?;
    if !out.status.success() {
        return Err(EasiError::Desktop(format!(
            "failed to query GitHub releases for {repo}"
        )));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let tag = extract_json_string_field(&body, "tag_name").ok_or_else(|| {
        EasiError::Desktop("could not parse tag_name from GitHub API response".into())
    })?;
    if tag.contains('/') || tag.contains('\\') || tag.contains("..") {
        return Err(EasiError::Desktop(format!("invalid release tag: {tag}")));
    }
    Ok(tag)
}

fn download_file(url: &str, dest: &Path) -> Result<(), EasiError> {
    if !url.starts_with("https://") {
        return Err(EasiError::Desktop(format!(
            "refusing non-HTTPS download URL: {url}"
        )));
    }
    let out = Command::new("curl")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "-fsSL",
            "--connect-timeout",
            "15",
            "--max-time",
            "180",
            "--max-filesize",
            "268435456",
            "-o",
        ])
        .arg(dest)
        .arg("--")
        .arg(url)
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

/// Stream a file through any RustCrypto hasher and return the lowercase hex digest.
pub fn file_digest_hex<D: Digest + std::io::Write>(path: &Path) -> Result<String, EasiError> {
    let mut hasher = D::new();
    std::io::copy(&mut File::open(path)?, &mut hasher)?;
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

    // Throwaway keypair made with `minisign -G -W`; SHA256SUMS signed with
    // `minisign -S -t "easi release tag:v9.9.9"` (minisign 0.12, prehashed).
    const TEST_PUBKEY: &str = "RWSnizU49shh6ejkXJgD9HKQaPbOen8nYG3iTouciJ9C8cb8b/G9kjKL";
    const TEST_SUMS: &[u8] = b"abc123  easi-v9.9.9-linux-x86_64.tar.gz\n";
    const TEST_SIG: &str = "untrusted comment: signature from minisign secret key
RUSnizU49shh6TJZbhRI8WiTqgVwZzT0F8nMaX7lgXjVgGwNojRIuqQgBkPPe2k0paPzlxDWzvUFYTzcdgFsAIVogzI8neb4fQc=
trusted comment: easi release tag:v9.9.9
ANbB7XuM6V9oebe+HcYx0ZvC5dcyaNu1A1mQEpWArDJOjSmRR9tDPpY0QhrZey35BPUnVzSvGq+3Cd2gEFccBQ==
";

    #[test]
    fn sums_signature_requires_valid_signature_and_exact_tag() {
        let retagged = TEST_SIG.replace("tag:v9.9.9", "tag:v9.9.8");
        // (case, sums, signature, pubkey, tag, must pass)
        type Row<'a> = (&'a str, &'a [u8], &'a str, &'a str, &'a str, bool);
        let rows: &[Row] = &[
            ("valid", TEST_SUMS, TEST_SIG, TEST_PUBKEY, "v9.9.9", true),
            (
                "tampered sums",
                b"def456  easi-v9.9.9-linux-x86_64.tar.gz\n",
                TEST_SIG,
                TEST_PUBKEY,
                "v9.9.9",
                false,
            ),
            (
                "wrong tag",
                TEST_SUMS,
                TEST_SIG,
                TEST_PUBKEY,
                "v9.9.8",
                false,
            ),
            (
                "tag prefix only",
                TEST_SUMS,
                TEST_SIG,
                TEST_PUBKEY,
                "v9.9",
                false,
            ),
            (
                "edited trusted comment",
                TEST_SUMS,
                &retagged,
                TEST_PUBKEY,
                "v9.9.8",
                false,
            ),
            (
                "wrong key",
                TEST_SUMS,
                TEST_SIG,
                DEFAULT_MINISIGN_PUBKEY,
                "v9.9.9",
                false,
            ),
            (
                "empty signature",
                TEST_SUMS,
                "",
                TEST_PUBKEY,
                "v9.9.9",
                false,
            ),
        ];
        for (case, sums, sig, pubkey, tag, ok) in rows {
            let result = verify_sums_signature(sums, sig, pubkey, tag);
            assert_eq!(result.is_ok(), *ok, "{case}: {result:?}");
        }
    }

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

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn config_roundtrip_and_cached_update_check() {
        let tmp = TempDir::new().expect("tempdir");
        let roots = InstallRoots::from_home(tmp.path());

        // Default config has auto_check_updates = true.
        assert!(load_config(&roots).auto_check_updates);

        // Turning it off persists and suppresses update check.
        set_auto_check_updates(&roots, false).expect("disable");
        assert!(!load_config(&roots).auto_check_updates);
        assert_eq!(check_for_easi_update(&roots, "0.1.0"), None);

        // Fresh cached version returns Some without touching network.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let cfg = EasiConfig {
            auto_check_updates: true,
            last_update_check_epoch: now,
            latest_known_version: Some("0.2.0".into()),
            onboarding_version: 0,
        };
        save_config(&roots, &cfg).expect("save");
        assert!(needs_onboarding(&roots));
        complete_onboarding(&roots).expect("complete onboarding");
        assert!(!needs_onboarding(&roots));
        assert_eq!(
            check_for_easi_update(&roots, "0.1.0").as_deref(),
            Some("0.2.0")
        );
        assert_eq!(check_for_easi_update(&roots, "0.2.0"), None);
    }
}
