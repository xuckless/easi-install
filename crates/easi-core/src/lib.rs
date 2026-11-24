//! easi-core: the single store, pure reducer, declarative tables, and effect runner.

pub mod appid;
pub mod detect;
pub mod error;
pub mod inspect;
pub mod model;
pub mod reduce;
pub mod state;
pub mod store;

pub use error::{EasiError, format_user_error};
pub use model::{
    AppKind, Candidate, ChoiceDraft, Classified, DoneOutcome, Format, InstallPlan, InstallStep,
    InstallSummary, InstalledApp, RollbackReason, SignatureStatus, TrustSummary, WizardStep,
};
pub use reduce::reduce;
pub use state::{Action, Effect, State};
pub use store::Store;
