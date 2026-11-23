//! easi-core: the single store. Pure `reduce` + effect descriptions; no I/O here.
//!
//! Every transition is an arm in `reduce`. Add behavior as a new `State` variant or a new
//! `(State, Action)` arm — never as a flag beside the state or a branch in a view.
//! The match must stay exhaustive: no `_ =>` catch-alls (see AGENTS.md).

/// The one lifecycle value. Variants carry the data they own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
}

/// Everything that can happen: user intent and I/O results. Filled in by M0/M1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {}

/// I/O the effect runner must perform, then report back as an `Action`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {}

pub fn reduce(state: State, action: Action) -> (State, Vec<Effect>) {
    match (state, action) {}
}
