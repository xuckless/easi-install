//! easi-core: the single store, pure reducer, declarative tables, and effect runner.

pub mod adopt;
pub mod appid;
pub mod desktop;
pub mod detect;
pub mod error;
pub mod inspect;
pub mod install;
pub mod manifest;
pub mod model;
pub mod plan;
pub mod reconcile;
pub mod reduce;
pub mod roots;
pub mod runner;
pub mod self_update;
pub mod state;
pub mod store;
pub mod upstream;

pub use error::{EasiError, format_user_error};
pub use model::{
    AppKind, Candidate, ChoiceDraft, Classified, DoneOutcome, Format, InstallPlan, InstallStep,
    InstallSummary, InstalledApp, RollbackReason, ScriptReport, SignatureStatus, TrustSummary,
    UpdateSource, WizardStep,
};
pub use reduce::{reduce, reduce_ui};
pub use state::{
    Action, ConfirmPrompt, Effect, FilterKind, ManagerPage, ManagerUi, State, UiAction,
};
pub use store::Store;
