//! Single-state Redux store + effect runner loop.

use std::collections::VecDeque;

use crate::reduce::reduce;
use crate::state::{Action, Effect, State};

#[derive(Debug, Default)]
pub struct Store {
    state: State,
}

impl Store {
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// Apply one action through the pure reducer and return resulting effects.
    pub fn dispatch(&mut self, action: Action) -> Vec<Effect> {
        let prev = std::mem::take(&mut self.state);
        let (next, effects) = reduce(prev, action);
        self.state = next;
        effects
    }

    /// Dispatch `initial`, execute every emitted `Effect` via `run_effect`, and feed
    /// resulting actions back into `reduce` until no effects remain.
    pub fn dispatch_and_run<F, O>(&mut self, initial: Action, mut run_effect: F, mut on_state: O)
    where
        F: FnMut(Effect) -> Vec<Action>,
        O: FnMut(&State),
    {
        let mut queue = VecDeque::from([initial]);
        while let Some(action) = queue.pop_front() {
            let effects = self.dispatch(action);
            on_state(&self.state);
            for effect in effects {
                for next_action in run_effect(effect) {
                    queue.push_back(next_action);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{Classified, DoneOutcome};

    #[test]
    fn store_loop_drives_effects_to_completion() {
        let mut store = Store::new();
        let mut visited = Vec::new();
        let pkg = PathBuf::from("/tmp/docs.zip");

        store.dispatch_and_run(
            Action::OpenPath(pkg.clone()),
            |effect| match effect {
                Effect::Inspect { path } => {
                    vec![Action::Inspected(Box::new(Classified::NotAnApp {
                        path,
                        reason: "documents only".into(),
                    }))]
                }
                Effect::HandOff { .. } => vec![Action::HandedOff],
                other => panic!("unexpected effect: {other:?}"),
            },
            |state| visited.push(std::mem::discriminant(state)),
        );

        assert_eq!(
            store.state(),
            &State::Done {
                outcome: DoneOutcome::HandedOff { path: pkg }
            }
        );
        assert_eq!(visited.len(), 3);
    }
}
