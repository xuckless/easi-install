//! Explicit finite state, actions, and effects for the single easi store.

use std::path::PathBuf;

use crate::model::{
    ChoiceDraft, Classified, DoneOutcome, InstallPlan, InstallStep, InstallSummary, InstalledApp,
    RollbackReason, TrustSummary, WizardStep,
};

/// The one lifecycle value. Variants carry the data they own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle {
        installed: Vec<InstalledApp>,
    },
    Inspecting {
        path: PathBuf,
    },
    HandingOff {
        path: PathBuf,
    },
    Ready {
        plan: InstallPlan,
        trust: TrustSummary,
    },
    NeedsChoice {
        draft: ChoiceDraft,
        trust: TrustSummary,
        step: WizardStep,
    },
    Conflict {
        existing: InstalledApp,
        plan: InstallPlan,
        trust: TrustSummary,
    },
    Installing {
        plan: InstallPlan,
        step: InstallStep,
        progress: u8,
    },
    Cancelling {
        plan: InstallPlan,
    },
    RollingBack {
        plan: InstallPlan,
        reason: RollbackReason,
    },
    OfferTrash {
        summary: InstallSummary,
    },
    Trashing {
        summary: InstallSummary,
    },
    Done {
        outcome: DoneOutcome,
    },
    Cancelled,
    Failed {
        error: String,
        rolled_back: bool,
    },
}

impl Default for State {
    fn default() -> Self {
        Self::Idle { installed: vec![] }
    }
}

/// Everything that can happen: user intent and I/O results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    LoadInstalled,
    InstalledLoaded(Vec<InstalledApp>),
    UninstallApp(String),
    UninstallFinished {
        app_id: String,
        ok: bool,
        error: Option<String>,
    },
    OpenPath(PathBuf),
    Inspected(Box<Classified>),
    InspectFailed(String),
    HandedOff,
    HandOffFailed(String),
    ConfirmInstall,
    ChooseExecutable(String),
    ChooseIcon(Option<String>),
    ConfirmName(String),
    Cancel,
    InstallProgress {
        step: InstallStep,
        progress: u8,
    },
    InstallFinished(InstallSummary),
    InstallFailed(String),
    RollbackFinished {
        ok: bool,
    },
    TrashSource,
    KeepSource,
    Trashed,
    TrashFailed(String),
    Reset,
}

/// I/O the effect runner must perform, then report back as an `Action`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    ListInstalled,
    UninstallApp {
        app_id: String,
    },
    Inspect {
        path: PathBuf,
    },
    HandOff {
        path: PathBuf,
    },
    RunInstall {
        plan: Box<InstallPlan>,
        replace_existing: bool,
    },
    CancelInstall,
    Rollback {
        app_id: String,
    },
    MoveToTrash {
        path: PathBuf,
    },
}
