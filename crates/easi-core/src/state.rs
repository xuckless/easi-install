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
        existing: Box<InstalledApp>,
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
    },
    FailedRolledBack {
        error: String,
    },
}

/// Active page in the standalone App Manager (replaces string/boolean UI state in Slint).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ManagerPage {
    #[default]
    Apps,
    AppInfo {
        app_id: String,
    },
    Settings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FilterKind {
    #[default]
    All,
    Gui,
    Cli,
}

/// Active inline confirmation prompt (at most one at a time — never multiple booleans).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfirmPrompt {
    #[default]
    None,
    ClearData,
    UninstallApp,
    SelfUninstall,
}

/// Pure UI state owned by `Store` so views are strictly functions of state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerUi {
    pub page: ManagerPage,
    pub search_query: String,
    pub filter_kind: FilterKind,
    pub sort_by_size: bool,
    pub auto_check_updates: bool,
    pub bg_update_timer: bool,
    pub purge_default: bool,
    pub detail_purge: bool,
    pub confirm: ConfirmPrompt,
    pub trash_checked: bool,
    pub script_consent_checked: bool,
    pub command_copied: bool,
}

impl Default for ManagerUi {
    fn default() -> Self {
        Self {
            page: ManagerPage::Apps,
            search_query: String::new(),
            filter_kind: FilterKind::All,
            sort_by_size: false,
            auto_check_updates: true,
            bg_update_timer: false,
            purge_default: false,
            detail_purge: false,
            confirm: ConfirmPrompt::None,
            trash_checked: true,
            script_consent_checked: false,
            command_copied: false,
        }
    }
}

/// Pure UI intents dispatched from the Slint view into `Store::dispatch_ui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Navigate(ManagerPage),
    SetSearchQuery(String),
    SetFilterKind(FilterKind),
    ToggleSortBySize,
    SetAutoCheckUpdates(bool),
    SetBgUpdateTimer(bool),
    SetPurgeDefault(bool),
    SetDetailPurge(bool),
    SetConfirm(ConfirmPrompt),
    SetTrashChecked(bool),
    SetScriptConsent(bool),
    SetCommandCopied(bool),
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
    UninstallApp {
        app_id: String,
        purge_data: bool,
    },
    UninstallFinished {
        app_id: String,
        result: Result<(), String>,
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
        purge_data: bool,
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
