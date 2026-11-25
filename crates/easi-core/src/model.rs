//! Pure domain types shared across state, detection, planning, and persistence.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Format {
    AppImageType1,
    AppImageType2,
    Tar,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    TarLz4,
    Zip,
    SevenZ,
    Jar,
    BareElf,
    ShellInstaller,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppKind {
    Gui,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    /// Cryptographically valid signature; `fingerprint` is the signer's primary key, shown to the
    /// user because gpg accepts any key in the local keyring regardless of who owns it.
    Verified {
        detail: String,
        fingerprint: String,
    },
    PresentUnverified(String),
    Invalid(String),
    None,
}

impl SignatureStatus {
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Verified { fingerprint, .. } => Some(fingerprint),
            Self::PresentUnverified(_) | Self::Invalid(_) | Self::None => None,
        }
    }

    /// One-line summary shared by the GUI trust panel and the recorded manifest.
    pub fn summary(&self) -> String {
        match self {
            Self::Verified {
                detail,
                fingerprint,
            } => format!("Verified ({detail}; signer {fingerprint})"),
            Self::PresentUnverified(m) => format!("Unverified ({m})"),
            Self::Invalid(m) => format!("Invalid ({m})"),
            Self::None => "None".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptReport {
    pub actions: Vec<String>,
    pub obfuscated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustSummary {
    pub size_bytes: u64,
    pub sha256: String,
    pub origin_url: Option<String>,
    pub signature: SignatureStatus,
    pub warnings: Vec<String>,
    pub script_report: Option<ScriptReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateSource {
    GitHubRelease {
        owner: String,
        repo: String,
        asset_pattern: Option<String>,
    },
    /// GitLab or Codeberg/Gitea/Forgejo releases API.
    ForgeRelease {
        host: String,
        project: String,
        forge: String,
    },
    ZsyncUrl {
        url: String,
    },
    /// Electron-builder generic/CDN provider serving `latest-linux.yml`.
    ElectronYaml {
        url: String,
    },
    /// JetBrains official CDN releases API (`product-info.json` productCode).
    JetBrains {
        product_code: String,
    },
    /// Direct CDN / proxy "latest download" redirect URL.
    HttpRedirect {
        url: String,
    },
}

impl UpdateSource {
    pub fn display_label(&self) -> String {
        match self {
            Self::GitHubRelease { owner, repo, .. } => format!("GitHub: {owner}/{repo}"),
            Self::ForgeRelease { host, project, .. } => format!("{host}: {project}"),
            Self::ZsyncUrl { url } => format!("zsync: {url}"),
            Self::ElectronYaml { url } => format!("Electron CDN: {url}"),
            Self::JetBrains { product_code } => format!("JetBrains CDN ({product_code})"),
            Self::HttpRedirect { url } => format!("Direct URL: {url}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub source_path: PathBuf,
    pub format: Format,
    pub app_id: String,
    pub synthesized_id: bool,
    pub name: String,
    pub version: Option<String>,
    pub kind: AppKind,
    pub main_exec: String,
    pub icon_path: Option<String>,
    pub icon_bytes: Option<Vec<u8>>,
    pub desktop_entry: Option<String>,
    pub startup_wm_class: Option<String>,
    pub developer: Option<String>,
    pub origin_url: Option<String>,
    pub signature_summary: Option<String>,
    /// Verified signer fingerprint, recorded so updates can detect a publisher key change.
    pub signer_fingerprint: Option<String>,
    pub update_source: Option<UpdateSource>,
    pub planned_writes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceDraft {
    pub source_path: PathBuf,
    pub format: Format,
    pub app_id: String,
    pub synthesized_id: bool,
    pub exec_candidates: Vec<Candidate>,
    pub icon_candidates: Vec<Candidate>,
    pub selected_exec: Option<String>,
    pub selected_icon: Option<String>,
    pub name: String,
    pub version: Option<String>,
    pub kind: AppKind,
    pub desktop_entry: Option<String>,
    pub startup_wm_class: Option<String>,
}

impl ChoiceDraft {
    pub fn into_plan(self) -> InstallPlan {
        let main_exec = self
            .selected_exec
            .or_else(|| self.exec_candidates.first().map(|c| c.path.clone()))
            .unwrap_or_default();
        InstallPlan {
            source_path: self.source_path,
            format: self.format,
            app_id: self.app_id,
            synthesized_id: self.synthesized_id,
            name: self.name,
            version: self.version,
            kind: self.kind,
            main_exec,
            icon_path: self.selected_icon,
            icon_bytes: None,
            desktop_entry: self.desktop_entry,
            startup_wm_class: self.startup_wm_class,
            developer: None,
            origin_url: None,
            signature_summary: None,
            signer_fingerprint: None,
            update_source: None,
            planned_writes: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    PickExecutable,
    PickIcon,
    ConfirmName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub app_id: String,
    pub name: String,
    pub version: Option<String>,
    pub source_filename: String,
    pub install_dir: PathBuf,
    pub kind: AppKind,
    pub developer: Option<String>,
    pub origin_url: Option<String>,
    pub signature_summary: Option<String>,
    pub adopted: bool,
    pub size_bytes: u64,
    pub data_size_bytes: u64,
    pub cache_size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStep {
    Preparing,
    Extracting,
    Wiring,
    RefreshingDesktop,
    SavingManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSummary {
    pub app_id: String,
    pub name: String,
    pub version: Option<String>,
    pub source_path: PathBuf,
    pub install_dir: PathBuf,
    pub kind: AppKind,
    pub bin_on_path: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    Ready {
        plan: InstallPlan,
        trust: TrustSummary,
    },
    Conflict {
        existing: Box<InstalledApp>,
        plan: InstallPlan,
        trust: TrustSummary,
    },
    NeedsChoice {
        draft: ChoiceDraft,
        trust: TrustSummary,
        step: WizardStep,
    },
    NotAnApp {
        path: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackReason {
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneOutcome {
    Installed {
        summary: InstallSummary,
        trashed: bool,
    },
    HandedOff {
        path: PathBuf,
    },
}
