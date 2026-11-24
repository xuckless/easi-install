//! Pure Redux reducer. Every `(State, Action)` pair is handled explicitly — no `_ =>` catch-all.

use crate::model::{Classified, DoneOutcome, InstallStep, RollbackReason, WizardStep};
use crate::state::{Action, Effect, State};

pub fn reduce(state: State, action: Action) -> (State, Vec<Effect>) {
    match (state, action) {
        // --- Idle ---
        (State::Idle, Action::OpenPath(path)) => (
            State::Inspecting { path: path.clone() },
            vec![Effect::Inspect { path }],
        ),
        (
            state @ State::Idle,
            Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::Cancel
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- Inspecting ---
        (State::Inspecting { .. }, Action::Inspected(classified)) => match *classified {
            Classified::Ready { plan, trust } => (State::Ready { plan, trust }, vec![]),
            Classified::Conflict {
                existing,
                plan,
                trust,
            } => (
                State::Conflict {
                    existing,
                    plan,
                    trust,
                },
                vec![],
            ),
            Classified::NeedsChoice { draft, trust, step } => {
                (State::NeedsChoice { draft, trust, step }, vec![])
            }
            Classified::NotAnApp { path, .. } => (
                State::HandingOff { path: path.clone() },
                vec![Effect::HandOff { path }],
            ),
        },
        (State::Inspecting { .. }, Action::InspectFailed(error)) => (
            State::Failed {
                error,
                rolled_back: false,
            },
            vec![],
        ),
        (State::Inspecting { .. }, Action::Cancel) => (State::Cancelled, vec![]),
        (
            state @ State::Inspecting { .. },
            Action::OpenPath(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- HandingOff ---
        (State::HandingOff { path }, Action::HandedOff) => (
            State::Done {
                outcome: DoneOutcome::HandedOff { path },
            },
            vec![],
        ),
        (State::HandingOff { .. }, Action::HandOffFailed(error)) => (
            State::Failed {
                error,
                rolled_back: false,
            },
            vec![],
        ),
        (
            state @ State::HandingOff { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::Cancel
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- Ready ---
        (State::Ready { plan, .. }, Action::ConfirmInstall) => (
            State::Installing {
                plan: plan.clone(),
                step: InstallStep::Preparing,
                progress: 0,
            },
            vec![Effect::RunInstall {
                plan,
                replace_existing: false,
            }],
        ),
        (State::Ready { .. }, Action::Cancel) => (State::Cancelled, vec![]),
        (State::Ready { .. }, Action::Reset) => (State::Idle, vec![]),
        (
            state @ State::Ready { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_),
        ) => (state, vec![]),

        // --- NeedsChoice (wizard) ---
        (
            State::NeedsChoice {
                mut draft,
                trust,
                step: WizardStep::PickExecutable,
            },
            Action::ChooseExecutable(exec),
        ) => {
            draft.selected_exec = Some(exec);
            let next_step = if draft.icon_candidates.len() > 1 {
                WizardStep::PickIcon
            } else {
                if draft.selected_icon.is_none() {
                    draft.selected_icon = draft.icon_candidates.first().map(|c| c.path.clone());
                }
                WizardStep::ConfirmName
            };
            (
                State::NeedsChoice {
                    draft,
                    trust,
                    step: next_step,
                },
                vec![],
            )
        }
        (
            State::NeedsChoice {
                mut draft,
                trust,
                step: WizardStep::PickIcon,
            },
            Action::ChooseIcon(icon),
        ) => {
            draft.selected_icon = icon;
            (
                State::NeedsChoice {
                    draft,
                    trust,
                    step: WizardStep::ConfirmName,
                },
                vec![],
            )
        }
        (
            State::NeedsChoice {
                mut draft,
                trust,
                step: WizardStep::ConfirmName,
            },
            Action::ConfirmName(name),
        ) => {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                draft.name = trimmed.to_string();
            }
            let plan = draft.into_plan();
            (State::Ready { plan, trust }, vec![])
        }
        (State::NeedsChoice { .. }, Action::Cancel) => (State::Cancelled, vec![]),
        (State::NeedsChoice { .. }, Action::Reset) => (State::Idle, vec![]),
        // Wrong wizard action for current step or unrelated action → ignore explicitly.
        (
            state @ State::NeedsChoice {
                step: WizardStep::PickExecutable,
                ..
            },
            Action::ChooseIcon(_) | Action::ConfirmName(_),
        )
        | (
            state @ State::NeedsChoice {
                step: WizardStep::PickIcon,
                ..
            },
            Action::ChooseExecutable(_) | Action::ConfirmName(_),
        )
        | (
            state @ State::NeedsChoice {
                step: WizardStep::ConfirmName,
                ..
            },
            Action::ChooseExecutable(_) | Action::ChooseIcon(_),
        )
        | (
            state @ State::NeedsChoice { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_),
        ) => (state, vec![]),

        // --- Conflict ---
        (State::Conflict { plan, .. }, Action::ConfirmInstall) => (
            State::Installing {
                plan: plan.clone(),
                step: InstallStep::Preparing,
                progress: 0,
            },
            vec![Effect::RunInstall {
                plan,
                replace_existing: true,
            }],
        ),
        (State::Conflict { .. }, Action::Cancel) => (State::Cancelled, vec![]),
        (State::Conflict { .. }, Action::Reset) => (State::Idle, vec![]),
        (
            state @ State::Conflict { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_),
        ) => (state, vec![]),

        // --- Installing ---
        (State::Installing { plan, .. }, Action::InstallProgress { step, progress }) => (
            State::Installing {
                plan,
                step,
                progress: progress.min(100),
            },
            vec![],
        ),
        (State::Installing { .. }, Action::InstallFinished(summary)) => {
            (State::OfferTrash { summary }, vec![])
        }
        (State::Installing { plan, .. }, Action::Cancel) => {
            (State::Cancelling { plan }, vec![Effect::CancelInstall])
        }
        (State::Installing { plan, .. }, Action::InstallFailed(error)) => {
            let app_id = plan.app_id.clone();
            (
                State::RollingBack {
                    plan,
                    reason: RollbackReason::Failed(error),
                },
                vec![Effect::Rollback { app_id }],
            )
        }
        (
            state @ State::Installing { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- Cancelling ---
        (State::Cancelling { plan }, Action::InstallFailed(_) | Action::InstallProgress { .. }) => {
            let app_id = plan.app_id.clone();
            (
                State::RollingBack {
                    plan,
                    reason: RollbackReason::Cancelled,
                },
                vec![Effect::Rollback { app_id }],
            )
        }
        (State::Cancelling { .. }, Action::InstallFinished(summary)) => {
            // Install beat the cancel signal; roll back the completed install.
            let app_id = summary.app_id.clone();
            let plan = crate::model::InstallPlan {
                source_path: summary.source_path,
                format: crate::model::Format::Zip,
                app_id: summary.app_id,
                synthesized_id: false,
                name: summary.name,
                version: summary.version,
                kind: summary.kind,
                main_exec: String::new(),
                icon_path: None,
                desktop_entry: None,
                startup_wm_class: None,
            };
            (
                State::RollingBack {
                    plan,
                    reason: RollbackReason::Cancelled,
                },
                vec![Effect::Rollback { app_id }],
            )
        }
        (
            state @ State::Cancelling { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::Cancel
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- RollingBack ---
        (State::RollingBack { reason, .. }, Action::RollbackFinished { ok }) => match reason {
            RollbackReason::Cancelled => (State::Cancelled, vec![]),
            RollbackReason::Failed(error) => (
                State::Failed {
                    error,
                    rolled_back: ok,
                },
                vec![],
            ),
        },
        (
            state @ State::RollingBack { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::Cancel
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- OfferTrash ---
        (State::OfferTrash { summary }, Action::TrashSource) => {
            let path = summary.source_path.clone();
            (
                State::Trashing { summary },
                vec![Effect::MoveToTrash { path }],
            )
        }
        (State::OfferTrash { summary }, Action::KeepSource | Action::Cancel) => (
            State::Done {
                outcome: DoneOutcome::Installed {
                    summary,
                    trashed: false,
                },
            },
            vec![],
        ),
        (
            state @ State::OfferTrash { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::Trashed
            | Action::TrashFailed(_)
            | Action::Reset,
        ) => (state, vec![]),

        // --- Trashing ---
        (State::Trashing { summary }, Action::Trashed) => (
            State::Done {
                outcome: DoneOutcome::Installed {
                    summary,
                    trashed: true,
                },
            },
            vec![],
        ),
        (State::Trashing { summary }, Action::TrashFailed(_)) => (
            // Install already succeeded; trash failure keeps source and completes.
            State::Done {
                outcome: DoneOutcome::Installed {
                    summary,
                    trashed: false,
                },
            },
            vec![],
        ),
        (
            state @ State::Trashing { .. },
            Action::OpenPath(_)
            | Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::Cancel
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Reset,
        ) => (state, vec![]),

        // --- Terminal states: Done / Cancelled / Failed ---
        (State::Done { .. } | State::Cancelled | State::Failed { .. }, Action::Reset) => {
            (State::Idle, vec![])
        }
        (State::Done { .. } | State::Cancelled | State::Failed { .. }, Action::OpenPath(path)) => (
            State::Inspecting { path: path.clone() },
            vec![Effect::Inspect { path }],
        ),
        (
            state @ (State::Done { .. } | State::Cancelled | State::Failed { .. }),
            Action::Inspected(_)
            | Action::InspectFailed(_)
            | Action::HandedOff
            | Action::HandOffFailed(_)
            | Action::ConfirmInstall
            | Action::ChooseExecutable(_)
            | Action::ChooseIcon(_)
            | Action::ConfirmName(_)
            | Action::Cancel
            | Action::InstallProgress { .. }
            | Action::InstallFinished(_)
            | Action::InstallFailed(_)
            | Action::RollbackFinished { .. }
            | Action::TrashSource
            | Action::KeepSource
            | Action::Trashed
            | Action::TrashFailed(_),
        ) => (state, vec![]),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{
        AppKind, Candidate, ChoiceDraft, Format, InstallPlan, InstallSummary, InstalledApp,
        SignatureStatus, TrustSummary,
    };

    fn sample_trust() -> TrustSummary {
        TrustSummary {
            size_bytes: 1024,
            sha256: "abc".into(),
            origin_url: None,
            signature: SignatureStatus::None,
            warnings: vec![],
        }
    }

    fn sample_plan() -> InstallPlan {
        InstallPlan {
            source_path: PathBuf::from("/tmp/foo.AppImage"),
            format: Format::AppImageType2,
            app_id: "easi.foo".into(),
            synthesized_id: true,
            name: "Foo".into(),
            version: Some("1.0".into()),
            kind: AppKind::Gui,
            main_exec: "foo.AppImage".into(),
            icon_path: Some("foo.png".into()),
            desktop_entry: None,
            startup_wm_class: Some("foo".into()),
        }
    }

    fn sample_existing() -> InstalledApp {
        InstalledApp {
            app_id: "easi.foo".into(),
            name: "Foo".into(),
            version: Some("0.9".into()),
            source_filename: "foo-0.9.AppImage".into(),
            install_dir: PathBuf::from("/tmp/apps/Foo"),
            kind: AppKind::Gui,
        }
    }

    fn sample_summary() -> InstallSummary {
        InstallSummary {
            app_id: "easi.foo".into(),
            name: "Foo".into(),
            version: Some("1.0".into()),
            source_path: PathBuf::from("/tmp/foo.AppImage"),
            install_dir: PathBuf::from("/tmp/apps/Foo"),
            kind: AppKind::Gui,
            bin_on_path: true,
        }
    }

    fn sample_draft(two_icons: bool) -> ChoiceDraft {
        let icon_candidates = if two_icons {
            vec![
                Candidate {
                    path: "a.png".into(),
                    score: 10,
                },
                Candidate {
                    path: "b.png".into(),
                    score: 9,
                },
            ]
        } else {
            vec![Candidate {
                path: "a.png".into(),
                score: 10,
            }]
        };
        ChoiceDraft {
            source_path: PathBuf::from("/tmp/foo.tar.gz"),
            format: Format::TarGz,
            app_id: "easi.foo".into(),
            synthesized_id: true,
            exec_candidates: vec![
                Candidate {
                    path: "bin/a".into(),
                    score: 10,
                },
                Candidate {
                    path: "bin/b".into(),
                    score: 9,
                },
            ],
            icon_candidates,
            selected_exec: None,
            selected_icon: None,
            name: "Foo".into(),
            version: None,
            kind: AppKind::Gui,
            desktop_entry: None,
            startup_wm_class: None,
        }
    }

    #[test]
    fn reducer_transition_table() {
        let p = PathBuf::from("/tmp/pkg.tar.gz");
        let plan = sample_plan();
        let trust = sample_trust();
        let existing = sample_existing();
        let summary = sample_summary();

        struct Row {
            name: &'static str,
            from: State,
            action: Action,
            expected_state: State,
            expected_effects: Vec<Effect>,
        }

        let rows = vec![
            Row {
                name: "idle + open -> inspecting",
                from: State::Idle,
                action: Action::OpenPath(p.clone()),
                expected_state: State::Inspecting { path: p.clone() },
                expected_effects: vec![Effect::Inspect { path: p.clone() }],
            },
            Row {
                name: "idle + confirm -> ignored",
                from: State::Idle,
                action: Action::ConfirmInstall,
                expected_state: State::Idle,
                expected_effects: vec![],
            },
            Row {
                name: "inspecting + ready -> ready",
                from: State::Inspecting { path: p.clone() },
                action: Action::Inspected(Box::new(Classified::Ready {
                    plan: plan.clone(),
                    trust: trust.clone(),
                })),
                expected_state: State::Ready {
                    plan: plan.clone(),
                    trust: trust.clone(),
                },
                expected_effects: vec![],
            },
            Row {
                name: "inspecting + conflict -> conflict",
                from: State::Inspecting { path: p.clone() },
                action: Action::Inspected(Box::new(Classified::Conflict {
                    existing: existing.clone(),
                    plan: plan.clone(),
                    trust: trust.clone(),
                })),
                expected_state: State::Conflict {
                    existing: existing.clone(),
                    plan: plan.clone(),
                    trust: trust.clone(),
                },
                expected_effects: vec![],
            },
            Row {
                name: "inspecting + not_an_app -> handing_off",
                from: State::Inspecting { path: p.clone() },
                action: Action::Inspected(Box::new(Classified::NotAnApp {
                    path: p.clone(),
                    reason: "no executables".into(),
                })),
                expected_state: State::HandingOff { path: p.clone() },
                expected_effects: vec![Effect::HandOff { path: p.clone() }],
            },
            Row {
                name: "inspecting + fail -> failed",
                from: State::Inspecting { path: p.clone() },
                action: Action::InspectFailed("bad file".into()),
                expected_state: State::Failed {
                    error: "bad file".into(),
                    rolled_back: false,
                },
                expected_effects: vec![],
            },
            Row {
                name: "handing_off + handed_off -> done",
                from: State::HandingOff { path: p.clone() },
                action: Action::HandedOff,
                expected_state: State::Done {
                    outcome: DoneOutcome::HandedOff { path: p.clone() },
                },
                expected_effects: vec![],
            },
            Row {
                name: "ready + confirm -> installing",
                from: State::Ready {
                    plan: plan.clone(),
                    trust: trust.clone(),
                },
                action: Action::ConfirmInstall,
                expected_state: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::Preparing,
                    progress: 0,
                },
                expected_effects: vec![Effect::RunInstall {
                    plan: plan.clone(),
                    replace_existing: false,
                }],
            },
            Row {
                name: "ready + cancel -> cancelled",
                from: State::Ready {
                    plan: plan.clone(),
                    trust: trust.clone(),
                },
                action: Action::Cancel,
                expected_state: State::Cancelled,
                expected_effects: vec![],
            },
            Row {
                name: "conflict + confirm -> installing(replace=true)",
                from: State::Conflict {
                    existing: existing.clone(),
                    plan: plan.clone(),
                    trust: trust.clone(),
                },
                action: Action::ConfirmInstall,
                expected_state: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::Preparing,
                    progress: 0,
                },
                expected_effects: vec![Effect::RunInstall {
                    plan: plan.clone(),
                    replace_existing: true,
                }],
            },
            Row {
                name: "wizard pick exec -> pick icon when multiple icons",
                from: State::NeedsChoice {
                    draft: sample_draft(true),
                    trust: trust.clone(),
                    step: WizardStep::PickExecutable,
                },
                action: Action::ChooseExecutable("bin/a".into()),
                expected_state: State::NeedsChoice {
                    draft: ChoiceDraft {
                        selected_exec: Some("bin/a".into()),
                        ..sample_draft(true)
                    },
                    trust: trust.clone(),
                    step: WizardStep::PickIcon,
                },
                expected_effects: vec![],
            },
            Row {
                name: "wizard pick exec -> confirm name when single icon",
                from: State::NeedsChoice {
                    draft: sample_draft(false),
                    trust: trust.clone(),
                    step: WizardStep::PickExecutable,
                },
                action: Action::ChooseExecutable("bin/a".into()),
                expected_state: State::NeedsChoice {
                    draft: ChoiceDraft {
                        selected_exec: Some("bin/a".into()),
                        selected_icon: Some("a.png".into()),
                        ..sample_draft(false)
                    },
                    trust: trust.clone(),
                    step: WizardStep::ConfirmName,
                },
                expected_effects: vec![],
            },
            Row {
                name: "wizard wrong action ignored",
                from: State::NeedsChoice {
                    draft: sample_draft(true),
                    trust: trust.clone(),
                    step: WizardStep::PickExecutable,
                },
                action: Action::ChooseIcon(Some("a.png".into())),
                expected_state: State::NeedsChoice {
                    draft: sample_draft(true),
                    trust: trust.clone(),
                    step: WizardStep::PickExecutable,
                },
                expected_effects: vec![],
            },
            Row {
                name: "installing + progress -> clamped progress",
                from: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::Preparing,
                    progress: 0,
                },
                action: Action::InstallProgress {
                    step: InstallStep::Extracting,
                    progress: 140,
                },
                expected_state: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::Extracting,
                    progress: 100,
                },
                expected_effects: vec![],
            },
            Row {
                name: "installing + finished -> offer_trash",
                from: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::SavingManifest,
                    progress: 95,
                },
                action: Action::InstallFinished(summary.clone()),
                expected_state: State::OfferTrash {
                    summary: summary.clone(),
                },
                expected_effects: vec![],
            },
            Row {
                name: "installing + cancel -> cancelling",
                from: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::Extracting,
                    progress: 40,
                },
                action: Action::Cancel,
                expected_state: State::Cancelling { plan: plan.clone() },
                expected_effects: vec![Effect::CancelInstall],
            },
            Row {
                name: "installing + fail -> rolling_back",
                from: State::Installing {
                    plan: plan.clone(),
                    step: InstallStep::Extracting,
                    progress: 40,
                },
                action: Action::InstallFailed("disk full".into()),
                expected_state: State::RollingBack {
                    plan: plan.clone(),
                    reason: RollbackReason::Failed("disk full".into()),
                },
                expected_effects: vec![Effect::Rollback {
                    app_id: "easi.foo".into(),
                }],
            },
            Row {
                name: "rolling_back(failed) + rollback_finished -> failed(rolled_back=true)",
                from: State::RollingBack {
                    plan: plan.clone(),
                    reason: RollbackReason::Failed("disk full".into()),
                },
                action: Action::RollbackFinished { ok: true },
                expected_state: State::Failed {
                    error: "disk full".into(),
                    rolled_back: true,
                },
                expected_effects: vec![],
            },
            Row {
                name: "rolling_back(cancelled) + rollback_finished -> cancelled",
                from: State::RollingBack {
                    plan: plan.clone(),
                    reason: RollbackReason::Cancelled,
                },
                action: Action::RollbackFinished { ok: true },
                expected_state: State::Cancelled,
                expected_effects: vec![],
            },
            Row {
                name: "offer_trash + trash -> trashing",
                from: State::OfferTrash {
                    summary: summary.clone(),
                },
                action: Action::TrashSource,
                expected_state: State::Trashing {
                    summary: summary.clone(),
                },
                expected_effects: vec![Effect::MoveToTrash {
                    path: summary.source_path.clone(),
                }],
            },
            Row {
                name: "offer_trash + keep -> done(trashed=false)",
                from: State::OfferTrash {
                    summary: summary.clone(),
                },
                action: Action::KeepSource,
                expected_state: State::Done {
                    outcome: DoneOutcome::Installed {
                        summary: summary.clone(),
                        trashed: false,
                    },
                },
                expected_effects: vec![],
            },
            Row {
                name: "trashing + trashed -> done(trashed=true)",
                from: State::Trashing {
                    summary: summary.clone(),
                },
                action: Action::Trashed,
                expected_state: State::Done {
                    outcome: DoneOutcome::Installed {
                        summary: summary.clone(),
                        trashed: true,
                    },
                },
                expected_effects: vec![],
            },
            Row {
                name: "trashing + trash_failed -> done(trashed=false)",
                from: State::Trashing {
                    summary: summary.clone(),
                },
                action: Action::TrashFailed("no trash".into()),
                expected_state: State::Done {
                    outcome: DoneOutcome::Installed {
                        summary: summary.clone(),
                        trashed: false,
                    },
                },
                expected_effects: vec![],
            },
            Row {
                name: "done + reset -> idle",
                from: State::Done {
                    outcome: DoneOutcome::HandedOff { path: p.clone() },
                },
                action: Action::Reset,
                expected_state: State::Idle,
                expected_effects: vec![],
            },
            Row {
                name: "failed + open_path -> inspecting",
                from: State::Failed {
                    error: "x".into(),
                    rolled_back: false,
                },
                action: Action::OpenPath(p.clone()),
                expected_state: State::Inspecting { path: p.clone() },
                expected_effects: vec![Effect::Inspect { path: p.clone() }],
            },
        ];

        for row in rows {
            let (next, effects) = reduce(row.from, row.action);
            assert_eq!(next, row.expected_state, "state mismatch in '{}'", row.name);
            assert_eq!(
                effects, row.expected_effects,
                "effects mismatch in '{}'",
                row.name
            );
        }
    }
}
