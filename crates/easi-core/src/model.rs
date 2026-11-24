//! Pure domain types shared across state, detection, planning, and persistence.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Format {
    AppImageType1,
    AppImageType2,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    Zip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppKind {
    Gui,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    Verified(String),
    PresentUnverified(String),
    Invalid(String),
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustSummary {
    pub size_bytes: u64,
    pub sha256: String,
    pub origin_url: Option<String>,
    pub signature: SignatureStatus,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub score: i32,
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
        existing: InstalledApp,
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
