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
    let cfg = easi_core::self_update::load_config(&runner.roots);
    window.set_auto_check_updates(cfg.auto_check_updates);

    if let Ok(st) = store.lock() {
        project_state(&window, st.state());
    }

    // Background brew-style update check for easi itself (never blocks startup).
    {
        let w = window.as_weak();
        let roots_bg = runner.roots.clone();
        std::thread::spawn(move || {
            if let Some(latest) =
                easi_core::self_update::check_for_easi_update(&roots_bg, env!("CARGO_PKG_VERSION"))
            {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w.upgrade() {
                        win.set_easi_update_version(latest.into());
                    }
                });
            }
        });
    }

    // Wire UI callbacks -> pure dispatch into Redux store.
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_browse_package(move || {
            let w = w.clone();
            let s = Arc::clone(&s);
            let r = Arc::clone(&r);
            std::thread::spawn(move || {
                let mut dialog = rfd::FileDialog::new()
                    .set_title("Choose a package to install")
                    .add_filter(
                        "Supported Packages",
                        &[
                            "AppImage", "appimage", "tar", "gz", "tgz", "xz", "txz", "zst", "bz2",
                            "zip", "7z", "run", "sh",
                        ],
                    )
                    .add_filter("All Files", &["*"]);
                if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
                    let dl = home.join("Downloads");
                    if dl.is_dir() {
                        dialog = dialog.set_directory(dl);
                    }
                }
                if let Some(path) = dialog.pick_file() {
                    let _ = slint::invoke_from_event_loop(move || {
                        dispatch_action(&w, &s, &r, Action::OpenPath(path));
                    });
                }
            });
        });
    }
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
        window.on_uninstall_app(move |app_id, purge_data| {
            dispatch_action(
                &w,
                &s,
                &r,
                Action::UninstallApp {
                    app_id: app_id.into(),
                    purge_data,
                },
            );
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_update_app(move |app_id| {
            let w = w.clone();
            let s = Arc::clone(&s);
            let r = Arc::clone(&r);
            let id: String = app_id.into();
            std::thread::spawn(move || {
                if let Ok(m) = easi_core::manifest::resolve_installed(&r.roots, &id) {
                    let _ = easi_core::upstream::download_and_update_app(&r.roots, &m);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    dispatch_action(&w, &s, &r, Action::LoadInstalled);
                });
            });
        });
    }
    {
        let r = Arc::clone(&runner);
        window.on_launch_app(move |app_id| {
            let _ = easi_core::desktop::launch_installed_app(&r.roots, app_id.as_str());
        });
    }
    {
        let r = Arc::clone(&runner);
        window.on_open_app_folder(move |app_id| {
            let _ = easi_core::desktop::open_app_folder(&r.roots, app_id.as_str());
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        window.on_filter_changed(move || {
            if let (Some(win), Ok(st)) = (w.upgrade(), s.lock()) {
                project_state(&win, st.state());
            }
        });
    }
    {
        window.on_copy_suggested_command(move |cmd| {
            let _ = easi_core::desktop::copy_to_clipboard(cmd.as_str());
        });
    }
    {
        let w = window.as_weak();
        let s = Arc::clone(&store);
        let r = Arc::clone(&runner);
        window.on_back_to_manager(move || {
            dispatch_action(&w, &s, &r, Action::Reset);
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
        let r = Arc::clone(&runner);
        window.on_check_app_updates(move || {
            let w = w.clone();
            let roots_bg = r.roots.clone();
            std::thread::spawn(move || {
                let manifests = easi_core::manifest::list_manifests(&roots_bg).unwrap_or_default();
                let mut available_count = 0usize;
                let rows: Vec<ui::InstalledRow> = manifests
                    .iter()
                    .map(|m| {
                        let (note, has_update) = match easi_core::upstream::check_app_update(m) {
                            easi_core::upstream::AppUpdateStatus::UpdateAvailable {
                                latest_version,
                                ..
                            } => {
                                available_count += 1;
                                (format!("Update available: {latest_version}"), true)
                            }
                            easi_core::upstream::AppUpdateStatus::UpToDate { .. } => {
                                ("Up to date".into(), false)
                            }
                            easi_core::upstream::AppUpdateStatus::Unsupported { .. } => {
                                ("No upstream update source".into(), false)
                            }
                            easi_core::upstream::AppUpdateStatus::CheckFailed {
                                reason, ..
                            } => (format!("Check failed: {reason}"), false),
                        };
                        let app = m.to_installed_app_with_sizes(&roots_bg);
                        ui::InstalledRow {
                            app_id: app.app_id.into(),
                            name: app.name.into(),
                            version: app.version.unwrap_or_default().into(),
                            kind: match app.kind {
                                AppKind::Gui => "GUI".into(),
                                AppKind::Terminal => "CLI".into(),
                            },
                            size_text: format_app_size(app.size_bytes, app.data_size_bytes).into(),
                            update_note: note.into(),
                            has_update,
                        }
                    })
                    .collect();
                if available_count > 0 {
                    easi_core::desktop::notify(
                        "App updates available",
                        &format!("{available_count} installed app(s) have newer releases."),
                    );
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w.upgrade() {
                        win.set_installed_apps(ModelRc::new(VecModel::from(rows)));
                    }
                });
            });
        });
    }
    {
        let r = Arc::clone(&runner);
        window.on_toggle_auto_update(move |enabled| {
            let _ = easi_core::self_update::set_auto_check_updates(&r.roots, enabled);
        });
    }
    {
        let w = window.as_weak();
        window.on_run_self_update(move || {
            let w = w.clone();
            std::thread::spawn(move || {
                let msg = match std::env::current_exe()
                    .map_err(easi_core::EasiError::Io)
                    .and_then(|exe| {
                        easi_core::self_update::run_self_update(env!("CARGO_PKG_VERSION"), &exe)
                    }) {
                    Ok(easi_core::self_update::UpdateOutcome::Updated { to, .. }) => {
                        easi_core::desktop::notify(
                            "easi updated",
                            &format!("Updated easi to {to}. Restart easi to use it."),
                        );
                        format!("Updated easi to {to}. Restart easi to use the new version.")
                    }
                    Ok(easi_core::self_update::UpdateOutcome::AlreadyUpToDate { version }) => {
                        format!("easi {version} is already up to date.")
                    }
                    Err(e) => format!("Update failed: {}", easi_core::format_user_error(&e)),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = w.upgrade() {
                        win.set_easi_update_version("".into());
                        win.set_status_text(msg.into());
                    }
                });
            });
        });
    }
    {
        let w = window.as_weak();
        let r = Arc::clone(&runner);
        window.on_run_self_uninstall(move || {
            let _ = easi_core::desktop::self_uninstall(&r.roots);
            if let Some(win) = w.upgrade() {
                let _ = win.hide();
            }
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
    } else {
        dispatch_action(&window.as_weak(), &store, &runner, Action::LoadInstalled);
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
        State::Idle { installed } => {
            win.set_screen("idle".into());
            let total_bytes: u64 = installed.iter().map(|a| a.size_bytes).sum();
            let total_text = if installed.is_empty() {
                String::new()
            } else {
                easi_core::manifest::format_human_bytes(total_bytes)
            };
            win.set_total_size_text(total_text.into());

            let query = win.get_search_query().to_string().to_ascii_lowercase();
            let kind_filter = win.get_filter_kind().to_string();
            let sort_by_size = win.get_sort_by_size();

            let mut filtered: Vec<&easi_core::InstalledApp> = installed
                .iter()
                .filter(|app| {
                    let kind_ok = match kind_filter.as_str() {
                        "gui" => app.kind == AppKind::Gui,
                        "cli" => app.kind == AppKind::Terminal,
                        _ => true,
                    };
                    let query_ok = query.is_empty()
                        || app.name.to_ascii_lowercase().contains(&query)
                        || app.app_id.to_ascii_lowercase().contains(&query);
                    kind_ok && query_ok
                })
                .collect();

            if sort_by_size {
                filtered.sort_by(|a, b| {
                    b.size_bytes
                        .cmp(&a.size_bytes)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
            }

            let rows: Vec<ui::InstalledRow> = filtered
                .into_iter()
                .map(|app| ui::InstalledRow {
                    app_id: app.app_id.clone().into(),
                    name: app.name.clone().into(),
                    version: app.version.clone().unwrap_or_default().into(),
                    kind: match app.kind {
                        AppKind::Gui => "GUI".into(),
                        AppKind::Terminal => "CLI".into(),
                    },
                    size_text: format_app_size(app.size_bytes, app.data_size_bytes).into(),
                    update_note: "".into(),
                    has_update: false,
                })
                .collect();
            win.set_installed_apps(ModelRc::new(VecModel::from(rows)));
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
            if let Some(img) = load_preview_icon(&plan.app_id, plan.icon_bytes.as_deref()) {
                win.set_app_icon(img);
                win.set_has_app_icon(true);
            } else {
                win.set_has_app_icon(false);
            }
            let perms = plan
                .planned_writes
                .iter()
                .map(|p| format!("• {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            win.set_permissions_text(perms.into());
            if let Some(script) = &trust.script_report {
                win.set_has_script_warning(true);
                win.set_script_consent_checked(false);
                let mut lines: Vec<String> =
                    script.actions.iter().map(|a| format!("• {a}")).collect();
                if script.obfuscated {
                    lines.push(
                        "• Obfuscated/binary payload detected — exact actions cannot be fully verified."
                            .into(),
                    );
                }
                win.set_script_warning_text(lines.join("\n").into());
            } else {
                win.set_has_script_warning(false);
                win.set_script_consent_checked(true);
                win.set_script_warning_text("".into());
            }
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
            win.set_command_copied(false);
            let cmd = extract_suggested_command(error).unwrap_or_default();
            win.set_suggested_command(cmd.into());
            let suffix = if *rolled_back {
                " (rolled back cleanly)"
            } else {
                ""
            };
            win.set_error_text(format!("{error}{suffix}").into());
        }
    }
}

fn load_preview_icon(app_id: &str, bytes: Option<&[u8]>) -> Option<slint::Image> {
    let data = bytes?;
    let ext = if data.starts_with(b"<svg") || data.starts_with(b"<?xml") {
        "svg"
    } else if data.starts_with(b"\x89PNG") {
        "png"
    } else {
        return None;
    };
    let safe_id: String = app_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = std::env::temp_dir().join(format!("easi-preview-{safe_id}.{ext}"));
    std::fs::write(&path, data).ok()?;
    slint::Image::load_from_path(&path).ok()
}

fn format_app_size(size_bytes: u64, data_size_bytes: u64) -> String {
    let app_str = easi_core::manifest::format_human_bytes(size_bytes);
    if data_size_bytes > 0 {
        format!(
            "{app_str} (+{} data)",
            easi_core::manifest::format_human_bytes(data_size_bytes)
        )
    } else {
        app_str
    }
}

fn extract_suggested_command(error: &str) -> Option<String> {
    error
        .lines()
        .find(|line| line.starts_with("  ") && !line.trim().is_empty())
        .map(|line| line.trim().to_string())
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
