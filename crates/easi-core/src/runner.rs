//! Thin effect runner: executes `Effect`s against `InstallRoots` and returns `Action`s.
//! Contains no business decisions — all transitions stay in `reduce`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::format_user_error;
use crate::inspect::classify_package;
use crate::install::{execute_install, move_to_trash, uninstall_app};
use crate::manifest::find_manifest_by_id;
use crate::reconcile::reconcile_all;
use crate::roots::InstallRoots;
use crate::state::{Action, Effect};

pub type HandoffFn = fn(&InstallRoots, &std::path::Path) -> Result<(), crate::EasiError>;

pub struct EffectRunner {
    pub roots: InstallRoots,
    pub cancel_flag: Arc<AtomicBool>,
    pub handoff_fn: HandoffFn,
}

impl EffectRunner {
    pub fn new(roots: InstallRoots) -> Self {
        Self {
            roots,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            handoff_fn: crate::desktop::handoff_non_app,
        }
    }

    pub fn run(&self, effect: Effect) -> Vec<Action> {
        match effect {
            Effect::ListInstalled => {
                let apps = reconcile_all(&self.roots)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.to_installed_app())
                    .collect();
                vec![Action::InstalledLoaded(apps)]
            }
            Effect::UninstallApp { app_id } => match uninstall_app(&self.roots, &app_id) {
                Ok(_) => vec![Action::UninstallFinished {
                    app_id,
                    ok: true,
                    error: None,
                }],
                Err(err) => vec![Action::UninstallFinished {
                    app_id,
                    ok: false,
                    error: Some(format_user_error(&err)),
                }],
            },
            Effect::Inspect { path } => {
                match classify_package(&path, |id| {
                    find_manifest_by_id(&self.roots, id).map(|m| m.to_installed_app())
                }) {
                    Ok(classified) => vec![Action::Inspected(Box::new(classified))],
                    Err(err) => vec![Action::InspectFailed(format_user_error(&err))],
                }
            }
            Effect::HandOff { path } => match (self.handoff_fn)(&self.roots, &path) {
                Ok(()) => vec![Action::HandedOff],
                Err(err) => vec![Action::HandOffFailed(format_user_error(&err))],
            },
            Effect::RunInstall {
                plan,
                replace_existing,
            } => {
                self.cancel_flag.store(false, Ordering::Relaxed);
                let mut actions = Vec::new();
                match execute_install(
                    &self.roots,
                    &plan,
                    replace_existing,
                    &self.cancel_flag,
                    |step, progress| {
                        actions.push(Action::InstallProgress { step, progress });
                    },
                ) {
                    Ok(summary) => actions.push(Action::InstallFinished(summary)),
                    Err(err) => actions.push(Action::InstallFailed(format_user_error(&err))),
                }
                actions
            }
            Effect::CancelInstall => {
                self.cancel_flag.store(true, Ordering::Relaxed);
                vec![]
            }
            Effect::Rollback { app_id } => {
                let ok = uninstall_app(&self.roots, &app_id).is_ok()
                    || find_manifest_by_id(&self.roots, &app_id).is_none();
                vec![Action::RollbackFinished { ok }]
            }
            Effect::MoveToTrash { path } => match move_to_trash(&self.roots, &path) {
                Ok(_) => vec![Action::Trashed],
                Err(err) => vec![Action::TrashFailed(format_user_error(&err))],
            },
        }
    }
}
