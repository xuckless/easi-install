//! Self-update engine (`easi self-update`): fetches latest GitHub Release, verifies SHA-256
//! and the pinned minisign signature, extracts safely, and atomically swaps the running binary.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::error::EasiError;
use crate::install::extract_archive_safe;
use crate::model::Format;
use crate::roots::InstallRoots;
use crate::sandbox::{WaitOutcome, wait_or_kill};

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

impl EasiConfig {
    /// Used when `config.toml` is unreadable: background checks stay off and onboarding runs
    /// again, so a crash never silently re-enables network checks the user turned off.
    pub fn safe_fallback() -> Self {
        Self {
            auto_check_updates: false,
            ..Self::default()
        }
    }

    pub fn needs_onboarding(&self) -> bool {
        self.onboarding_version < CURRENT_ONBOARDING_VERSION
    }
}

/// Warning shown (CLI stderr, GUI notification) after an unreadable config was set aside.
pub fn corrupt_config_notice(kept_at: &Path) -> String {
    format!(
        "easi settings could not be read and were reset with automatic update checks OFF; \
         the old file was kept at {}. Re-enable checks with `easi config --auto-update true`.",
        kept_at.display()
    )
}

pub fn complete_onboarding(roots: &InstallRoots) -> Result<EasiConfig, EasiError> {
    let mut cfg = load_config(roots);
    cfg.onboarding_version = CURRENT_ONBOARDING_VERSION;
    save_config(roots, &cfg)?;
    Ok(cfg)
}

pub fn load_config(roots: &InstallRoots) -> EasiConfig {
    load_config_checked(roots).0
}

/// Load `config.toml`; a missing file gives first-run defaults. A file that does not parse is
/// renamed to `config.toml.corrupt-<n>` (never deleted) and replaced by
/// [`EasiConfig::safe_fallback`]; the second value is where the bad file now is.
pub fn load_config_checked(roots: &InstallRoots) -> (EasiConfig, Option<PathBuf>) {
    let path = roots.config_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (EasiConfig::default(), None),
        Err(_) => return (EasiConfig::safe_fallback(), Some(path)),
    };
    if let Some(cfg) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| toml::from_str(s).ok())
    {
        return (cfg, None);
    }
    let fallback = EasiConfig::safe_fallback();
    let aside = (1u32..)
        .map(|n| roots.easi_dir.join(format!("config.toml.corrupt-{n}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| path.clone());
    if fs::rename(&path, &aside).is_err() {
        return (fallback, Some(path));
    }
    let _ = save_config(roots, &fallback);
    (fallback, Some(aside))
}

pub fn save_config(roots: &InstallRoots, cfg: &EasiConfig) -> Result<PathBuf, EasiError> {
    fs::create_dir_all(&roots.easi_dir)?;
    let path = roots.config_path();
    let text = toml::to_string_pretty(cfg).map_err(|e| EasiError::Desktop(e.to_string()))?;
    crate::atomic::write_atomic(&path, text.as_bytes(), 0o644)?;
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

    swap_in_binary(&new_bin, target_bin, latest_ver)?;
    if let Ok(roots) = InstallRoots::default_user() {
        crate::desktop::ensure_easi_desktop_and_icon(&roots, target_bin);
    }

    Ok(UpdateOutcome::Updated {
        from: current_version.to_string(),
        to: latest_ver.to_string(),
        bin_path: target_bin.to_path_buf(),
    })
}

/// Where the binary replaced by the last self-update is kept for `easi self-update --rollback`.
pub fn previous_binary_path(bin: &Path) -> PathBuf {
    let mut name = bin.file_name().unwrap_or_default().to_os_string();
    name.push(".prev");
    bin.with_file_name(name)
}

/// Stage `new_bin` under a random name beside `target_bin`, require it to report
/// `expected_version`, keep the current binary as [`previous_binary_path`], then rename the
/// staged copy over `target_bin`. A random name cannot be pre-planted as a symlink.
pub fn swap_in_binary(
    new_bin: &Path,
    target_bin: &Path,
    expected_version: &str,
) -> Result<(), EasiError> {
    let parent = target_bin.parent().ok_or_else(|| {
        EasiError::Desktop(format!("invalid executable path: {}", target_bin.display()))
    })?;
    let mut staged = NamedTempFile::new_in(parent)?;
    std::io::copy(&mut File::open(new_bin)?, staged.as_file_mut())?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))?;
    staged.as_file().sync_all()?;
    // Close our write handle first: executing a file that is open for writing fails (ETXTBSY).
    let staged = staged.into_temp_path();
    let found = binary_version(&staged)?;
    if found != expected_version {
        return Err(EasiError::SelfUpdateVersionMismatch {
            expected: expected_version.to_string(),
            found,
        });
    }
    let prev = previous_binary_path(target_bin);
    let _ = fs::remove_file(&prev);
    if fs::hard_link(target_bin, &prev).is_err() {
        fs::copy(target_bin, &prev)?;
    }
    staged
        .persist(target_bin)
        .map_err(|e| EasiError::Io(e.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Put the binary kept by the last self-update back in place of `target_bin`; returns the
/// version it reports.
pub fn rollback_self_update(target_bin: &Path) -> Result<String, EasiError> {
    let prev = previous_binary_path(target_bin);
    if !prev.is_file() {
        return Err(EasiError::NotFound(prev));
    }
    let version = binary_version(&prev)?;
    fs::rename(&prev, target_bin)?;
    Ok(version)
}

/// The version an easi binary prints for `--version` (`easi X.Y.Z`), or what it printed
/// instead; a binary that fails, hangs past 10 s or prints nothing reports an empty version.
fn binary_version(bin: &Path) -> Result<String, EasiError> {
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let outcome = wait_or_kill(&mut child, Duration::from_secs(10), &AtomicBool::new(false))?;
    let mut out = String::new();
    match outcome {
        WaitOutcome::Exited(status) if status.success() => {
            if let Some(mut stdout) = child.stdout.take() {
                stdout.read_to_string(&mut out)?;
            }
        }
        WaitOutcome::Exited(_) | WaitOutcome::Cancelled | WaitOutcome::TimedOut => {}
    }
    Ok(out
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string())
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
        assert!(load_config(&roots).needs_onboarding());
        complete_onboarding(&roots).expect("complete onboarding");
        assert!(!load_config(&roots).needs_onboarding());
        assert_eq!(
            check_for_easi_update(&roots, "0.1.0").as_deref(),
            Some("0.2.0")
        );
        assert_eq!(check_for_easi_update(&roots, "0.2.0"), None);
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn unreadable_config_is_kept_aside_and_falls_back_to_checks_off() {
        // (case, config.toml bytes, expected auto_check_updates, kept aside as corrupt-1)
        let rows: &[(&str, Option<&[u8]>, bool, bool)] = &[
            ("missing", None, true, false),
            (
                "valid, checks off",
                Some(b"auto_check_updates = false\n"),
                false,
                false,
            ),
            (
                "truncated toml",
                Some(b"auto_check_updates = tr"),
                false,
                true,
            ),
            (
                "wrong type",
                Some(b"auto_check_updates = \"yes\"\n"),
                false,
                true,
            ),
            ("not utf-8", Some(b"\xff\xfe\x00"), false, true),
        ];
        for (case, bytes, auto, corrupt) in rows {
            let tmp = TempDir::new().expect("tempdir");
            let roots = InstallRoots::from_home(tmp.path());
            if let Some(bytes) = bytes {
                fs::create_dir_all(&roots.easi_dir).unwrap();
                fs::write(roots.config_path(), bytes).unwrap();
            }
            let (cfg, kept) = load_config_checked(&roots);
            assert_eq!(cfg.auto_check_updates, *auto, "{case}");
            let aside = roots.easi_dir.join("config.toml.corrupt-1");
            assert_eq!(kept.as_deref() == Some(aside.as_path()), *corrupt, "{case}");
            if *corrupt {
                assert_eq!(fs::read(&aside).unwrap(), *bytes.unwrap(), "{case}");
                // The fallback is persisted, so later loads do not flip checks back on.
                assert_eq!(load_config_checked(&roots), (cfg.clone(), None), "{case}");
                assert!(cfg.needs_onboarding(), "{case}");
            }
        }
    }

    /// Write an executable fake `easi` whose `--version` runs `body`.
    #[allow(clippy::unwrap_used)]
    fn fake_easi(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A sibling test forking while a fake binary's write fd is open makes exec fail with
    /// ETXTBSY until that child execs; retry briefly instead of racing.
    fn retry_busy<T>(mut f: impl FnMut() -> Result<T, EasiError>) -> Result<T, EasiError> {
        for _ in 0..50 {
            match f() {
                Err(EasiError::Io(e)) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        f()
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn swap_requires_expected_version_and_keeps_previous_for_rollback() {
        // (case, new binary --version body, swapped in)
        let rows = [
            ("matching version", "echo 'easi 9.9.9'", true),
            ("older version", "echo 'easi 9.9.8'", false),
            ("no output", "true", false),
            ("fails", "echo 'easi 9.9.9'; exit 1", false),
        ];
        for (case, body, swapped) in rows {
            let tmp = TempDir::new().expect("tempdir");
            let target = tmp.path().join("easi");
            let new_bin = tmp.path().join("new-easi");
            fake_easi(&target, "echo 'easi 1.0.0'");
            fake_easi(&new_bin, body);
            let result = retry_busy(|| swap_in_binary(&new_bin, &target, "9.9.9"));
            assert_eq!(result.is_ok(), swapped, "{case}: {result:?}");
            let installed = fs::read_to_string(&target).unwrap();
            assert_eq!(installed.contains(body), swapped, "{case}");
            // Only the target (and .prev after a swap) remain: no staging leftovers.
            let entries = fs::read_dir(tmp.path()).unwrap().count();
            assert_eq!(entries, if swapped { 3 } else { 2 }, "{case}");
            if swapped {
                assert_eq!(
                    retry_busy(|| rollback_self_update(&target)).unwrap(),
                    "1.0.0"
                );
                assert!(fs::read_to_string(&target).unwrap().contains("easi 1.0.0"));
                assert!(!previous_binary_path(&target).exists(), "{case}");
            }
        }
        let tmp = TempDir::new().expect("tempdir");
        assert!(matches!(
            rollback_self_update(&tmp.path().join("easi")),
            Err(EasiError::NotFound(_))
        ));
    }
}
