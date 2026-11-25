//! Thin effect runner: executes `Effect`s against `InstallRoots` and returns `Action`s.
//! Contains no business decisions — all transitions stay in `reduce`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::format_user_error;
use crate::inspect::classify_package;
use crate::install::{execute_install, move_to_trash, uninstall_app, uninstall_app_with_options};
use crate::manifest::find_manifest_by_id;
use crate::oplog::{LockWait, Op, StateLock, run_op, run_op_with_source};
use crate::reconcile::reconcile_all;
use crate::roots::InstallRoots;
use crate::sandbox::Isolation;
use crate::state::{Action, Effect};

pub type HandoffFn = fn(&InstallRoots, &std::path::Path) -> Result<(), crate::EasiError>;

pub struct EffectRunner {
    pub roots: InstallRoots,
    pub isolation: Isolation,
    pub cancel_flag: Arc<AtomicBool>,
    pub handoff_fn: HandoffFn,
    /// How mutating effects wait for another easi operation (the CLI prints a notice).
    pub lock_wait: LockWait,
}

impl EffectRunner {
    pub fn new(roots: InstallRoots, isolation: Isolation) -> Self {
        Self {
            roots,
            isolation,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            handoff_fn: crate::desktop::handoff_non_app,
            lock_wait: LockWait::Patiently(|| {}),
        }
    }

    pub fn run(&self, effect: Effect) -> Vec<Action> {
        let mut actions = Vec::new();
        self.run_streaming(effect, |act| actions.push(act));
        actions
    }

    /// Execute `effect`, invoking `emit` immediately for each produced `Action` (allows the
    /// GUI to stream `InstallProgress` updates in real time).
    pub fn run_streaming<F>(&self, effect: Effect, mut emit: F)
    where
        F: FnMut(Action),
    {
        match effect {
            Effect::ListInstalled => {
                let apps = reconcile_all(&self.roots)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.to_installed_app_with_sizes(&self.roots))
                    .collect();
                emit(Action::InstalledLoaded(apps));
            }
            Effect::UninstallApp { app_id, purge_data } => {
                match run_op(&self.roots, self.lock_wait, Op::Uninstall, &app_id, || {
                    uninstall_app_with_options(&self.roots, &app_id, purge_data)
                }) {
                    Ok(m) => {
                        crate::desktop::notify("App uninstalled", &format!("Removed {}", m.name));
                        emit(Action::UninstallFinished {
                            app_id,
                            result: Ok(()),
                        });
                    }
                    Err(err) => emit(Action::UninstallFinished {
                        app_id,
                        result: Err(format_user_error(&err)),
                    }),
                }
            }
            Effect::Inspect { path } => {
                match classify_package(&path, &self.isolation, |id| {
                    find_manifest_by_id(&self.roots, id).map(|m| m.to_installed_app())
                }) {
                    Ok(classified) => emit(Action::Inspected(Box::new(classified))),
                    Err(err) => emit(Action::InspectFailed(format_user_error(&err))),
                }
            }
            Effect::HandOff { path } => match (self.handoff_fn)(&self.roots, &path) {
                Ok(()) => emit(Action::HandedOff),
                Err(err) => emit(Action::HandOffFailed(format_user_error(&err))),
            },
            Effect::RunInstall {
                plan,
                replace_existing,
            } => {
                self.cancel_flag.store(false, Ordering::Relaxed);
                let installed = run_op_with_source(
                    &self.roots,
                    self.lock_wait,
                    Op::Install,
                    &plan.app_id,
                    || {
                        execute_install(
                            &self.roots,
                            &self.isolation,
                            &plan,
                            replace_existing,
                            &self.cancel_flag,
                            |step, progress| {
                                emit(Action::InstallProgress { step, progress });
                            },
                        )
                    },
                    |summary| Some(summary.source.clone()),
                );
                match installed {
                    Ok(summary) => {
                        let ver = summary
                            .version
                            .as_deref()
                            .map(|v| format!(" {v}"))
                            .unwrap_or_default();
                        crate::desktop::notify(
                            "App installed",
                            &format!("{}{ver} is ready to use.", summary.name),
                        );
                        emit(Action::InstallFinished(summary));
                    }
                    Err(err) => emit(Action::InstallFailed(format_user_error(&err))),
                }
            }
            Effect::CancelInstall => {
                self.cancel_flag.store(true, Ordering::Relaxed);
            }
            Effect::Rollback { app_id } => {
                // Locked but not journaled: the failed or cancelled install it undoes already is.
                let ok = StateLock::acquire(&self.roots, self.lock_wait)
                    .and_then(|_lock| uninstall_app(&self.roots, &app_id))
                    .is_ok()
                    || find_manifest_by_id(&self.roots, &app_id).is_none();
                emit(Action::RollbackFinished { ok });
            }
            Effect::MoveToTrash { path } => match move_to_trash(&self.roots, &path) {
                Ok(_) => emit(Action::Trashed),
                Err(err) => emit(Action::TrashFailed(format_user_error(&err))),
            },
        }
    }
}
