//! easi-gui: Slint view over the easi-core store. UI binds to state; callbacks only dispatch.

// Slint-generated code uses unwrap internally; our own code stays under the workspace lints.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod ui {
    slint::include_modules!();
}

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use easi_core::roots::InstallRoots;
use easi_core::runner::EffectRunner;
use easi_core::{
    Action, AppKind, DoneOutcome, Effect, SignatureStatus, State, Store, TrustSummary, WizardStep,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

pub fn run(initial_path: Option<PathBuf>) -> Result<(), slint::PlatformError> {
    let roots = InstallRoots::default_user().unwrap_or_else(|_| {
        InstallRoots::from_home(&std::env::temp_dir().join("easi-fallback-home"))
    });
    let runner = Arc::new(EffectRunner::new(roots));
    let store = Arc::new(Mutex::new(Store::new()));
    let window = ui::AppWindow::new()?;

    if let Ok(st) = store.lock() {
        project_state(&window, st.state());
    }

    // Wire UI callbacks -> pure dispatch into Redux store.
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_open_path(move |p| {
            dispatch_action(&w, &s, &r, Action::OpenPath(PathBuf::from(p.as_str())));
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_confirm_install(move || {
            dispatch_action(&w, &s, &r, Action::ConfirmInstall);
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_cancel(move || {
            dispatch_action(&w, &s, &r, Action::Cancel);
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_choose_candidate(move |idx| {
            let action = s.lock().ok().and_then(|st| match st.state() {
                State::NeedsChoice {
                    draft,
                    step: WizardStep::PickExecutable,
                    ..
                } => draft
                    .exec_candidates
                    .get(idx as usize)
                    .map(|c| Action::ChooseExecutable(c.path.clone())),
                State::NeedsChoice {
                    draft,
                    step: WizardStep::PickIcon,
                    ..
                } => Some(Action::ChooseIcon(
                    draft
                        .icon_candidates
                        .get(idx as usize)
                        .map(|c| c.path.clone()),
                )),
                _ => None,
            });
            if let Some(act) = action {
                dispatch_action(&w, &s, &r, act);
            }
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_confirm_name(move |name| {
            dispatch_action(&w, &s, &r, Action::ConfirmName(name.into()));
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_finish_trash(move |should_trash| {
            let act = if should_trash {
                Action::TrashSource
            } else {
                Action::KeepSource
            };
            dispatch_action(&w, &s, &r, act);
        });
    }
    {
        let w = window.as_weak();
        window.on_close_window(move || {
            if let Some(win) = w.upgrade() {
                let _ = win.hide();
            }
        });
    }

    if let Some(path) = initial_path {
        dispatch_action(&window.as_weak(), &store, &runner, Action::OpenPath(path));
    }

    window.run()
}

fn dispatch_action(
    weak: &Weak<ui::AppWindow>,
    store: &Arc<Mutex<Store>>,
    runner: &Arc<EffectRunner>,
    action: Action,
) {
    let effects = match store.lock() {
        Ok(mut st) => {
            let eff = st.dispatch(action);
            if let Some(win) = weak.upgrade() {
                project_state(&win, st.state());
            }
            eff
        }
        Err(_) => return,
    };

    for effect in effects {
        // CancelInstall is an atomic flag flip; run synchronously so it preempts in-flight work.
        if matches!(effect, Effect::CancelInstall) {
            let _ = runner.run(effect);
            continue;
        }
        let w = weak.clone();
        let s = Arc::clone(store);
        let r = Arc::clone(runner);
        std::thread::spawn(move || {
            let actions = r.run(effect);
            let _ = slint::invoke_from_event_loop(move || {
                for next in actions {
                    dispatch_action(&w, &s, &r, next);
                }
            });
        });
    }
}

fn project_trust(win: &ui::AppWindow, trust: &TrustSummary) {
    win.set_trust_size(format_bytes(trust.size_bytes).into());
    let short_sha: String = trust.sha256.chars().take(16).collect();
    win.set_trust_sha256(format!("{short_sha}…").into());
    win.set_trust_origin(trust.origin_url.clone().unwrap_or_default().into());
    let sig = match &trust.signature {
        SignatureStatus::Verified(m) => format!("Verified ({m})"),
        SignatureStatus::PresentUnverified(m) => format!("Unverified ({m})"),
        SignatureStatus::Invalid(m) => format!("Invalid ({m})"),
        SignatureStatus::None => "None".into(),
    };
    win.set_trust_signature(sig.into());
    win.set_trust_warnings(trust.warnings.join("; ").into());
}

fn project_state(win: &ui::AppWindow, state: &State) {
    match state {
        State::Idle => {
            win.set_screen("idle".into());
        }
        State::Inspecting { path } => {
            win.set_screen("inspecting".into());
            win.set_status_text(format!("Inspecting {}…", path.display()).into());
        }
        State::HandingOff { path } => {
            win.set_screen("inspecting".into());
            win.set_status_text(
                format!("Opening {} in previous archive handler…", path.display()).into(),
            );
        }
        State::Ready { plan, trust } => {
            win.set_screen("ready".into());
            win.set_app_name(plan.name.clone().into());
            win.set_app_version(plan.version.clone().unwrap_or_default().into());
            win.set_app_id(plan.app_id.clone().into());
            win.set_app_kind(
                match plan.kind {
                    AppKind::Gui => "GUI Application",
                    AppKind::Terminal => "Terminal Command",
                }
                .into(),
            );
            project_trust(win, trust);
        }
        State::Conflict {
            existing,
            plan,
            trust,
        } => {
            win.set_screen("conflict".into());
            let old_v = existing.version.as_deref().unwrap_or("unknown");
            let new_v = plan.version.as_deref().unwrap_or("unknown");
            win.set_conflict_text(
                format!(
                    "{} ({old_v}) is already installed — replace with {new_v}?",
                    existing.name
                )
                .into(),
            );
            project_trust(win, trust);
        }
        State::NeedsChoice { draft, step, .. } => match step {
            WizardStep::PickExecutable => {
                win.set_screen("wizard_exec".into());
                let items: Vec<SharedString> = draft
                    .exec_candidates
                    .iter()
                    .map(|c| SharedString::from(c.path.as_str()))
                    .collect();
                win.set_wizard_choices(ModelRc::new(VecModel::from(items)));
            }
            WizardStep::PickIcon => {
                win.set_screen("wizard_icon".into());
                let items: Vec<SharedString> = draft
                    .icon_candidates
                    .iter()
                    .map(|c| SharedString::from(c.path.as_str()))
                    .collect();
                win.set_wizard_choices(ModelRc::new(VecModel::from(items)));
            }
            WizardStep::ConfirmName => {
                win.set_screen("wizard_name".into());
                win.set_wizard_name_input(draft.name.clone().into());
            }
        },
        State::Installing { step, progress, .. } => {
            win.set_screen("installing".into());
            win.set_status_text(format!("{step:?}…").into());
            win.set_progress(f32::from(*progress) / 100.0);
        }
        State::Cancelling { .. } => {
            win.set_screen("installing".into());
            win.set_status_text("Cancelling…".into());
        }
        State::RollingBack { .. } => {
            win.set_screen("installing".into());
            win.set_status_text("Rolling back…".into());
        }
        State::OfferTrash { summary } => {
            win.set_screen("offer_trash".into());
            win.set_app_name(summary.name.clone().into());
            win.set_install_dir(summary.install_dir.display().to_string().into());
            win.set_trash_checked(true);
        }
        State::Trashing { .. } => {
            win.set_screen("installing".into());
            win.set_status_text("Moving download to Trash…".into());
        }
        State::Done { outcome } => {
            win.set_screen("done".into());
            win.set_error_text("".into());
            match outcome {
                DoneOutcome::Installed { summary, trashed } => {
                    let msg = if *trashed {
                        format!("{} installed and download moved to Trash.", summary.name)
                    } else {
                        format!("{} installed.", summary.name)
                    };
                    win.set_status_text(msg.into());
                }
                DoneOutcome::HandedOff { path } => {
                    win.set_status_text(
                        format!(
                            "Handed off {} to your previous archive manager.",
                            path.display()
                        )
                        .into(),
                    );
                }
            }
        }
        State::Cancelled => {
            win.set_screen("cancelled".into());
            win.set_status_text("Cancelled.".into());
            win.set_error_text("".into());
        }
        State::Failed { error, rolled_back } => {
            win.set_screen("failed".into());
            let suffix = if *rolled_back {
                " (rolled back cleanly)"
            } else {
                ""
            };
            win.set_error_text(format!("{error}{suffix}").into());
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
