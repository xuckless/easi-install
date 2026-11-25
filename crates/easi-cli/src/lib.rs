//! easi-cli: terminal view over the easi-core store. Parses argv into actions, renders state.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use easi_core::desktop::{restore_handlers, run_setup_with_options, self_uninstall};
use easi_core::install::{add_bin_to_shell_rc, find_app_data_dirs, uninstall_app_with_options};
use easi_core::manifest::resolve_installed;
use easi_core::reconcile::{probe_active_wm_class, reconcile_all, set_startup_wm_class};
use easi_core::roots::InstallRoots;
use easi_core::runner::EffectRunner;
use easi_core::{
    Action, AppKind, DoneOutcome, SignatureStatus, State, Store, TrustSummary, WizardStep,
    format_user_error,
};

#[derive(Debug, Parser)]
#[command(
    name = "easi",
    version,
    about = "Install downloaded Linux apps the easy way"
)]
pub struct Cli {
    /// Answer yes to every prompt (non-interactive).
    #[arg(short, long, global = true)]
    pub yes: bool,

    /// Register easi for AppImages (default double-click) and archives (Right-Click -> Open With).
    #[arg(long)]
    pub setup: bool,

    /// Opt-in with `--setup`: also claim .zip/.tar.*/.7z/.jar as default double-click handlers.
    #[arg(long)]
    pub claim_archives: bool,

    /// Undo `easi --setup` and restore all remembered previous default MIME handlers.
    #[arg(long)]
    pub restore_handlers: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install an AppImage, tarball or zip.
    Install {
        path: PathBuf,
        /// Print planned writes and trust summary without modifying disk.
        #[arg(long)]
        dry_run: bool,
        /// Move the downloaded source package to Trash after installing (opt-in under -y).
        #[arg(long)]
        trash: bool,
        /// Add ~/.local/bin to your shell profile if missing (opt-in under -y).
        #[arg(long)]
        add_to_path: bool,
        /// Explicitly consent to running an untrusted .run/.sh/.bin installer script under -y.
        #[arg(long)]
        allow_script: bool,
    },
    /// Inspect a package and print its trust summary and planned writes without installing.
    Inspect { path: PathBuf },
    /// Export managed apps and their update sources to a portable TOML file (or stdout).
    Export { file: Option<PathBuf> },
    /// Reinstall apps from an exported TOML file using their recorded update sources.
    Import { file: PathBuf },
    /// Remove an app installed by easi.
    Uninstall {
        app: String,
        /// Also remove per-user app settings and cache (~/.config, ~/.cache, ~/.local/share).
        #[arg(long)]
        purge_data: bool,
    },
    /// List apps installed by easi.
    List,
    /// Check upstream releases (AppImage zsync / GitHub Releases) for installed apps.
    CheckUpdates,
    /// Update an installed app (or --all) from its recorded upstream release source.
    Update {
        app: Option<String>,
        /// Update every installed app that has a newer upstream release.
        #[arg(long)]
        all: bool,
        /// Accept and re-pin a changed publisher signing key (or its removal) and download host for this one app.
        #[arg(long, conflicts_with = "all", requires = "app")]
        retrust: bool,
    },
    /// Record the real StartupWMClass for an installed app (prevents duplicate dock icons).
    LearnWmclass {
        app: String,
        /// Explicit WM_CLASS (if omitted, probes the active X11/XWayland window via xprop).
        wm_class: Option<String>,
    },
    /// Scan $HOME for existing non-package-manager apps that easi can adopt and repair.
    Scan,
    /// Adopt an existing unmanaged app (or --all) into easi and repair missing icons/launchers.
    Adopt {
        app: Option<String>,
        /// Adopt every discovered unmanaged app in $HOME.
        #[arg(long)]
        all: bool,
        /// Adopt without modifying icons, .desktop files, or CLI symlinks.
        #[arg(long)]
        no_fix: bool,
    },
    /// Repair missing icon, .desktop launcher, StartupWMClass, or CLI symlink for a managed app.
    Repair { app: String },
    /// Move an adopted app neatly into ~/Applications/<App>/ and update its launcher and symlink.
    Organize { app: String },
    /// Stop managing an app in easi without deleting any of its files on disk.
    Forget { app: String },
    /// Set or auto-detect the upstream update source (GitHub, GitLab, Codeberg, CDN YAML, JetBrains, or direct URL).
    ///
    /// Changing the source also forgets the pinned publisher signing key and download host;
    /// the next update pins them again from what it receives.
    SetSource {
        app: String,
        /// Repository, CDN, or latest-download URL (omit to auto-detect from app config files).
        url: Option<String>,
        /// Clear the recorded update source.
        #[arg(long)]
        clear: bool,
    },
    /// Update easi itself.
    SelfUpdate,
    /// Cleanly uninstall easi itself and restore previous MIME handlers (keeps installed apps).
    SelfUninstall,
    /// View or change easi settings (e.g. --auto-update true|false).
    Config {
        /// Enable or disable automatic easi update checks on use.
        #[arg(long)]
        auto_update: Option<bool>,
    },
}

pub fn run(cli: Cli) -> ExitCode {
    let roots = match InstallRoots::default_user() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            return ExitCode::FAILURE;
        }
    };

    if cli.setup {
        return run_setup_cmd(&roots, cli.claim_archives);
    }
    if cli.restore_handlers {
        return run_restore_handlers_cmd(&roots);
    }

    match cli.command {
        Some(Command::Install {
            path,
            dry_run,
            trash,
            add_to_path,
            allow_script,
        }) => {
            maybe_notify_update(&roots);
            if dry_run {
                run_inspect_cmd(&roots, path)
            } else {
                run_install_cmd(&roots, path, cli.yes, trash, add_to_path, allow_script)
            }
        }
        Some(Command::Inspect { path }) => run_inspect_cmd(&roots, path),
        Some(Command::Export { file }) => run_export_cmd(&roots, file.as_deref()),
        Some(Command::Import { file }) => run_import_cmd(&roots, &file),
        Some(Command::Uninstall { app, purge_data }) => {
            maybe_notify_update(&roots);
            run_uninstall_cmd(&roots, &app, purge_data, cli.yes)
        }
        Some(Command::List) => {
            maybe_notify_update(&roots);
            run_list_cmd(&roots)
        }
        Some(Command::CheckUpdates) => {
            maybe_notify_update(&roots);
            run_check_updates_cmd(&roots)
        }
        Some(Command::Update { app, all, retrust }) => {
            maybe_notify_update(&roots);
            let trust = if retrust {
                easi_core::upstream::PublisherTrust::Retrust
            } else {
                easi_core::upstream::PublisherTrust::Recorded
            };
            run_update_cmd(&roots, app.as_deref(), all, trust)
        }
        Some(Command::LearnWmclass { app, wm_class }) => {
            run_learn_wmclass_cmd(&roots, &app, wm_class.as_deref())
        }
        Some(Command::Scan) => run_scan_cmd(&roots),
        Some(Command::Adopt { app, all, no_fix }) => {
            run_adopt_cmd(&roots, app.as_deref(), all, !no_fix)
        }
        Some(Command::Repair { app }) => run_repair_cmd(&roots, &app),
        Some(Command::Organize { app }) => run_organize_cmd(&roots, &app),
        Some(Command::Forget { app }) => run_forget_cmd(&roots, &app),
        Some(Command::SetSource { app, url, clear }) => {
            run_set_source_cmd(&roots, &app, url.as_deref(), clear)
        }
        Some(Command::SelfUpdate) => run_self_update_cmd(),
        Some(Command::SelfUninstall) => run_self_uninstall_cmd(&roots, cli.yes),
        Some(Command::Config { auto_update }) => run_config_cmd(&roots, auto_update),
        None => {
            eprintln!(
                "easi: pass a subcommand (`install`, `uninstall`, `list`, `update`, `check-updates`, `config`, `self-uninstall`) or `--setup` / `--restore-handlers`"
            );
            ExitCode::FAILURE
        }
    }
}

fn maybe_notify_update(roots: &InstallRoots) {
    let current = env!("CARGO_PKG_VERSION");
    if let Some(latest) = easi_core::self_update::check_for_easi_update(roots, current) {
        easi_core::desktop::notify(
            "easi update available",
            &format!("easi {latest} is available (run `easi self-update`)."),
        );
        eprintln!(
            "Notice: easi {latest} is available (current: {current}). Run `easi self-update` to upgrade (disable with `easi config --auto-update false` or EASI_NO_AUTO_UPDATE=1)."
        );
    }
}

fn run_config_cmd(roots: &InstallRoots, auto_update: Option<bool>) -> ExitCode {
    match auto_update {
        Some(enabled) => match easi_core::self_update::set_auto_check_updates(roots, enabled) {
            Ok(cfg) => {
                println!("auto_check_updates = {}", cfg.auto_check_updates);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("easi: failed to save config: {}", format_user_error(&e));
                ExitCode::FAILURE
            }
        },
        None => {
            let cfg = easi_core::self_update::load_config(roots);
            println!("auto_check_updates = {}", cfg.auto_check_updates);
            ExitCode::SUCCESS
        }
    }
}

fn run_check_updates_cmd(roots: &InstallRoots) -> ExitCode {
    let manifests = match reconcile_all(roots) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            return ExitCode::FAILURE;
        }
    };
    if manifests.is_empty() {
        println!("No apps installed by easi.");
        return ExitCode::SUCCESS;
    }
    let mut available_count = 0usize;
    for m in &manifests {
        match easi_core::upstream::check_app_update(m) {
            easi_core::upstream::AppUpdateStatus::UpdateAvailable {
                name,
                current_version,
                latest_version,
                release_url,
                ..
            } => {
                available_count += 1;
                println!(
                    "{name:<24} UPDATE AVAILABLE: {current_version} -> {latest_version}  ({release_url})"
                );
            }
            easi_core::upstream::AppUpdateStatus::UpToDate {
                name,
                current_version,
                ..
            } => {
                println!("{name:<24} up to date ({current_version})");
            }
            easi_core::upstream::AppUpdateStatus::Unsupported { name, .. } => {
                println!("{name:<24} no upstream update source");
            }
            easi_core::upstream::AppUpdateStatus::CheckFailed { name, reason, .. } => {
                println!("{name:<24} check failed ({reason})");
            }
        }
    }
    if available_count > 0 {
        easi_core::desktop::notify(
            "App updates available",
            &format!("{available_count} installed app(s) have newer releases."),
        );
    }
    ExitCode::SUCCESS
}

fn run_self_update_cmd() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("easi: cannot locate current executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    let current = env!("CARGO_PKG_VERSION");
    match easi_core::self_update::run_self_update(current, &exe) {
        Ok(easi_core::self_update::UpdateOutcome::AlreadyUpToDate { version }) => {
            println!("easi {version} is already up to date.");
            ExitCode::SUCCESS
        }
        Ok(easi_core::self_update::UpdateOutcome::Updated { from, to, bin_path }) => {
            easi_core::desktop::notify(
                "easi updated",
                &format!("Updated easi from {from} to {to}."),
            );
            println!("Updated easi from {from} to {to} ({}).", bin_path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: self-update failed: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_setup_cmd(roots: &InstallRoots, claim_archives: bool) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("easi: cannot locate current executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run_setup_with_options(roots, &exe, claim_archives) {
        Ok(saved) => {
            if claim_archives {
                println!(
                    "easi registered as default handler for AppImages and archives (.tar.*, .zip, .7z, .jar)."
                );
            } else {
                println!(
                    "easi registered as default handler for AppImages, and added to Right-Click -> Open With for archives (.tar.*, .zip, .7z, .jar, .sh)."
                );
            }
            println!(
                "Remembered {} previous handler(s) in {}.",
                saved.defaults.len(),
                roots.previous_handlers_path().display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: setup failed: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_list_cmd(roots: &InstallRoots) -> ExitCode {
    match reconcile_all(roots) {
        Ok(list) if list.is_empty() => {
            println!("No apps installed by easi.");
            ExitCode::SUCCESS
        }
        Ok(list) => {
            let mut total_bytes = 0u64;
            for m in &list {
                let app = m.to_installed_app_with_sizes(roots);
                total_bytes = total_bytes.saturating_add(app.size_bytes);
                let ver = app.version.as_deref().unwrap_or("-");
                let kind = match app.kind {
                    AppKind::Gui => "GUI",
                    AppKind::Terminal => "CLI",
                };
                let size = easi_core::manifest::format_human_bytes(app.size_bytes);
                let data_note = if app.data_size_bytes > 0 {
                    format!(
                        " (+{} data)",
                        easi_core::manifest::format_human_bytes(app.data_size_bytes)
                    )
                } else {
                    String::new()
                };
                println!(
                    "{:<24} {:<10} [{}]  {:>10}{}  {}",
                    app.name, ver, kind, size, data_note, app.app_id
                );
            }
            println!(
                "\nTotal: {} across {} app(s).",
                easi_core::manifest::format_human_bytes(total_bytes),
                list.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_restore_handlers_cmd(roots: &InstallRoots) -> ExitCode {
    match restore_handlers(roots) {
        Ok(n) => {
            println!("Restored {n} previous MIME handler(s) and removed easi.desktop.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: restore failed: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_self_uninstall_cmd(roots: &InstallRoots, yes: bool) -> ExitCode {
    if !yes
        && !prompt_yes_no(
            "Uninstall easi and restore previous MIME handlers? (Your installed apps in ~/Applications will be kept.)",
            false,
        )
    {
        println!("Cancelled.");
        return ExitCode::SUCCESS;
    }
    match self_uninstall(roots) {
        Ok(restored) => {
            println!("Uninstalled easi and restored {restored} previous MIME handler(s).");
            println!("Your installed apps in ~/Applications remain intact.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: self-uninstall failed: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_update_cmd(
    roots: &InstallRoots,
    app: Option<&str>,
    all: bool,
    trust: easi_core::upstream::PublisherTrust,
) -> ExitCode {
    let targets = if all {
        match reconcile_all(roots) {
            Ok(list) => list
                .into_iter()
                .filter(|m| {
                    matches!(
                        easi_core::upstream::check_app_update(m),
                        easi_core::upstream::AppUpdateStatus::UpdateAvailable { .. }
                    )
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("easi: {}", format_user_error(&e));
                return ExitCode::FAILURE;
            }
        }
    } else if let Some(query) = app {
        match resolve_installed(roots, query) {
            Ok(m) => vec![m],
            Err(e) => {
                eprintln!("easi: {}", format_user_error(&e));
                return ExitCode::FAILURE;
            }
        }
    } else {
        eprintln!("easi: specify an app name or pass `--all`");
        return ExitCode::FAILURE;
    };

    if targets.is_empty() {
        println!("All installed apps are up to date.");
        return ExitCode::SUCCESS;
    }

    let mut failed = false;
    for m in &targets {
        println!("Updating {}…", m.name);
        match easi_core::upstream::download_and_update_app(roots, m, trust) {
            Ok(easi_core::upstream::UpdateOutcome { summary, integrity }) => {
                println!(
                    "Updated {} to {} ({}).",
                    summary.name,
                    summary.version.as_deref().unwrap_or("latest"),
                    integrity.label()
                );
            }
            Err(e) => {
                eprintln!(
                    "easi: failed to update {}: {}",
                    m.name,
                    format_user_error(&e)
                );
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_learn_wmclass_cmd(roots: &InstallRoots, app: &str, wm_class: Option<&str>) -> ExitCode {
    let class = match wm_class {
        Some(c) => c.to_string(),
        None => match probe_active_wm_class() {
            Some(c) => c,
            None => {
                eprintln!(
                    "easi: could not probe active window via xprop; pass the class explicitly: `easi learn-wmclass {app} <WMClass>`"
                );
                return ExitCode::FAILURE;
            }
        },
    };
    match set_startup_wm_class(roots, app, &class) {
        Ok(desktop) => {
            println!("Set StartupWMClass={class} in {}", desktop.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_scan_cmd(roots: &InstallRoots) -> ExitCode {
    let discovered = easi_core::adopt::scan_unmanaged_apps(roots);
    if discovered.is_empty() {
        println!("No unmanaged user apps detected in $HOME.");
        return ExitCode::SUCCESS;
    }
    println!(
        "Detected {} unmanaged user app(s) (run `easi adopt <name>` or `easi adopt --all`):\n",
        discovered.len()
    );
    for app in &discovered {
        let ver = app.version.as_deref().unwrap_or("-");
        let kind = match app.kind {
            AppKind::Gui => "GUI",
            AppKind::Terminal => "CLI",
        };
        let size = easi_core::manifest::format_human_bytes(app.size_bytes);
        let issues = if app.issues.is_empty() {
            String::new()
        } else {
            format!(
                "  [can fix: {}]",
                app.issues
                    .iter()
                    .map(|i| i.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!(
            "{:<24} {:<10} [{}]  {:>10}  {}{}",
            app.name, ver, kind, size, app.app_id, issues
        );
    }
    ExitCode::SUCCESS
}

fn run_adopt_cmd(roots: &InstallRoots, app: Option<&str>, all: bool, repair: bool) -> ExitCode {
    if all {
        return match easi_core::adopt::adopt_all(roots, repair) {
            Ok(list) if list.is_empty() => {
                println!("No unmanaged user apps to adopt.");
                ExitCode::SUCCESS
            }
            Ok(list) => {
                for m in &list {
                    println!("Adopted {} ({}).", m.name, m.app_id);
                }
                println!("\nAdopted {} app(s).", list.len());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("easi: {}", format_user_error(&e));
                ExitCode::FAILURE
            }
        };
    }

    let Some(query) = app else {
        eprintln!(
            "easi: specify an app name/id, folder path, or pass `--all` (run `easi scan` first)"
        );
        return ExitCode::FAILURE;
    };
    let candidate_path = PathBuf::from(query);
    if candidate_path.exists() {
        return match easi_core::adopt::adopt_path(roots, &candidate_path, repair) {
            Ok(m) => {
                println!("Adopted {} ({}).", m.name, m.app_id);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("easi: {}", format_user_error(&e));
                ExitCode::FAILURE
            }
        };
    }
    let q = query.to_ascii_lowercase();
    let discovered = easi_core::adopt::scan_unmanaged_apps(roots);
    let Some(target) = discovered
        .iter()
        .find(|a| a.app_id.eq_ignore_ascii_case(query) || a.name.eq_ignore_ascii_case(query))
        .or_else(|| {
            discovered.iter().find(|a| {
                a.app_id.to_ascii_lowercase().contains(&q)
                    || a.name.to_ascii_lowercase().contains(&q)
            })
        })
    else {
        eprintln!("easi: no unmanaged app matching '{query}' (run `easi scan` to see candidates)");
        return ExitCode::FAILURE;
    };

    match easi_core::adopt::adopt_app(roots, target, repair) {
        Ok(m) => {
            println!("Adopted {} ({}).", m.name, m.app_id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_repair_cmd(roots: &InstallRoots, app: &str) -> ExitCode {
    match easi_core::adopt::repair_app(roots, app) {
        Ok(fixes) if fixes.is_empty() => {
            println!("No issues found for '{app}' — icon and launcher look good.");
            ExitCode::SUCCESS
        }
        Ok(fixes) => {
            println!("Repaired '{app}':");
            for f in fixes {
                println!("  - {f}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_organize_cmd(roots: &InstallRoots, app: &str) -> ExitCode {
    match easi_core::adopt::organize_app_into_applications(roots, app) {
        Ok(m) => {
            println!("Organized {} into {}.", m.name, m.install_dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_forget_cmd(roots: &InstallRoots, app: &str) -> ExitCode {
    match easi_core::adopt::forget_app(roots, app) {
        Ok(m) => {
            println!(
                "Stopped managing {} ({}). App files on disk were not modified.",
                m.name, m.app_id
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_set_source_cmd(roots: &InstallRoots, app: &str, url: Option<&str>, clear: bool) -> ExitCode {
    if clear {
        return match easi_core::upstream::set_app_update_source(roots, app, None) {
            Ok(m) => {
                println!("Cleared update source for {} ({}).", m.name, m.app_id);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("easi: {}", format_user_error(&e));
                ExitCode::FAILURE
            }
        };
    }

    let source = if let Some(u) = url {
        match easi_core::upstream::parse_user_update_url(u) {
            Some(s) => s,
            None => {
                eprintln!(
                    "easi: unrecognized update source '{u}' (pass a GitHub/GitLab/Codeberg/CDN/latest URL or `jetbrains:<CODE>`)"
                );
                return ExitCode::FAILURE;
            }
        }
    } else {
        match resolve_installed(roots, app) {
            Ok(m) => {
                let detected = easi_core::upstream::detect_dir_update_source(&m.install_dir)
                    .or_else(|| easi_core::upstream::lookup_known_app_source(&m.app_id));
                match detected {
                    Some(s) => s,
                    None => {
                        eprintln!(
                            "easi: no embedded update config found in {}; pass a URL explicitly: `easi set-source {app} <url>`",
                            m.install_dir.display()
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            Err(e) => {
                eprintln!("easi: {}", format_user_error(&e));
                return ExitCode::FAILURE;
            }
        }
    };

    let label = source.display_label();
    match easi_core::upstream::set_app_update_source(roots, app, Some(source)) {
        Ok(m) => {
            println!("Set update source for {} ({}) -> {label}", m.name, m.app_id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_inspect_cmd(roots: &InstallRoots, path: PathBuf) -> ExitCode {
    let existing = easi_core::manifest::list_manifests(roots).unwrap_or_default();
    match easi_core::inspect::classify_package(&path, |id| {
        existing
            .iter()
            .find(|m| m.app_id == id)
            .map(|m| m.to_installed_app())
    }) {
        Ok(easi_core::Classified::Ready { plan, trust })
        | Ok(easi_core::Classified::Conflict { plan, trust, .. }) => {
            println!(
                "{} ({})",
                plan.name,
                plan.version.as_deref().unwrap_or("unknown version")
            );
            println!("  App ID: {}", plan.app_id);
            println!("  Format: {:?}", plan.format);
            println!("  Kind:   {:?}", plan.kind);
            if let Some(src) = &plan.update_source {
                println!("  Update: {}", src.display_label());
            }
            print_trust(&trust);
            if !plan.planned_writes.is_empty() {
                println!("  Planned writes (dry-run, nothing written):");
                for w in &plan.planned_writes {
                    println!("    - {w}");
                }
            }
            ExitCode::SUCCESS
        }
        Ok(easi_core::Classified::NeedsChoice { draft, trust, .. }) => {
            println!("{} (ambiguous — would open selection wizard)", draft.name);
            println!("  App ID: {}", draft.app_id);
            println!("  Format: {:?}", draft.format);
            print_trust(&trust);
            println!("  Executable candidates:");
            for c in &draft.exec_candidates {
                println!("    - {}", c.path);
            }
            ExitCode::SUCCESS
        }
        Ok(easi_core::Classified::NotAnApp { reason, .. }) => {
            println!("Not an installable app: {reason}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_export_cmd(roots: &InstallRoots, file: Option<&std::path::Path>) -> ExitCode {
    match easi_core::upstream::export_apps_toml(roots) {
        Ok(toml_text) => {
            if let Some(path) = file {
                match std::fs::write(path, toml_text) {
                    Ok(()) => {
                        println!("Exported managed apps to {}.", path.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("easi: failed to write {}: {e}", path.display());
                        ExitCode::FAILURE
                    }
                }
            } else {
                print!("{toml_text}");
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_import_cmd(roots: &InstallRoots, file: &std::path::Path) -> ExitCode {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("easi: failed to read {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    match easi_core::upstream::import_apps_from_toml(roots, &text) {
        Ok(outcomes) => {
            let mut failed = false;
            for o in outcomes {
                match o {
                    easi_core::upstream::ImportOutcome::Installed { name, version } => {
                        println!(
                            "Installed {name} ({}).",
                            version.as_deref().unwrap_or("latest")
                        );
                    }
                    easi_core::upstream::ImportOutcome::AlreadyInstalled { name } => {
                        println!("Skipped {name} (already installed).");
                    }
                    easi_core::upstream::ImportOutcome::SkippedNoSource { name } => {
                        println!("Skipped {name} (no upstream update source recorded).");
                    }
                    easi_core::upstream::ImportOutcome::Failed { name, error } => {
                        eprintln!("Failed to install {name}: {error}");
                        failed = true;
                    }
                }
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_uninstall_cmd(roots: &InstallRoots, app: &str, purge_data: bool, yes: bool) -> ExitCode {
    if !yes && !prompt_yes_no(&format!("Uninstall '{app}'?"), false) {
        println!("Cancelled.");
        return ExitCode::SUCCESS;
    }
    let mut should_purge = purge_data;
    if !should_purge
        && !yes
        && let Ok(m) = resolve_installed(roots, app)
    {
        let dirs = find_app_data_dirs(roots, &m);
        if !dirs.is_empty() {
            println!("Found app settings/cache directories:");
            for d in &dirs {
                println!("  - {}", d.display());
            }
            should_purge = prompt_yes_no("Also remove these app settings & cache?", false);
        }
    }
    match uninstall_app_with_options(roots, app, should_purge) {
        Ok(m) => {
            easi_core::desktop::notify("App uninstalled", &format!("Removed {}", m.name));
            if should_purge {
                println!(
                    "Uninstalled {} ({}) and removed app data.",
                    m.name, m.app_id
                );
            } else {
                println!("Uninstalled {} ({}).", m.name, m.app_id);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_install_cmd(
    roots: &InstallRoots,
    path: PathBuf,
    yes: bool,
    opt_in_trash: bool,
    opt_in_add_to_path: bool,
    allow_script: bool,
) -> ExitCode {
    let runner = EffectRunner::new(roots.clone());
    let mut store = Store::new();

    store.dispatch_and_run(
        Action::OpenPath(path),
        |effect| runner.run(effect),
        |state| {
            if let State::Installing { step, progress, .. } = state {
                println!("[{progress:3}%] {step:?}");
            }
        },
    );

    loop {
        let next_action = match store.state() {
            State::Ready { plan, trust } => {
                println!(
                    "Ready to install: {} ({})",
                    plan.name,
                    plan.version.as_deref().unwrap_or("unknown version")
                );
                println!("  App ID: {}", plan.app_id);
                println!("  Kind:   {:?}", plan.kind);
                print_trust(trust);
                if !plan.planned_writes.is_empty() {
                    println!("  Install changes:");
                    for w in &plan.planned_writes {
                        println!("    - {w}");
                    }
                }
                if let Some(script) = &trust.script_report {
                    println!("\n  WARNING! This package runs a shell script that:");
                    for act in &script.actions {
                        println!("    * {act}");
                    }
                    if script.obfuscated {
                        println!(
                            "    * Note: script is obfuscated or contains binary data — exact commands cannot be fully verified."
                        );
                    }
                    if allow_script {
                        Action::ConfirmInstall
                    } else if yes {
                        eprintln!(
                            "easi: refusing to run installer script under -y without explicit `--allow-script`."
                        );
                        Action::Cancel
                    } else if prompt_yes_no("Allow easi to execute this installer script?", false) {
                        Action::ConfirmInstall
                    } else {
                        Action::Cancel
                    }
                } else if yes || prompt_yes_no("Install?", true) {
                    Action::ConfirmInstall
                } else {
                    Action::Cancel
                }
            }
            State::Conflict {
                existing,
                plan,
                trust,
            } => {
                let old_v = existing.version.as_deref().unwrap_or("unknown");
                let new_v = plan.version.as_deref().unwrap_or("unknown");
                println!(
                    "{} ({old_v}) is already installed — replace with {new_v}?",
                    existing.name
                );
                print_trust(trust);
                if yes || prompt_yes_no("Replace existing installation?", true) {
                    Action::ConfirmInstall
                } else {
                    Action::Cancel
                }
            }
            State::NeedsChoice { draft, step, .. } => match step {
                WizardStep::PickExecutable => {
                    println!("Multiple executables found in {}:", draft.name);
                    for (i, c) in draft.exec_candidates.iter().enumerate() {
                        println!("  [{}] {}", i + 1, c.path);
                    }
                    let idx = if yes {
                        0
                    } else {
                        prompt_index("Select main executable", draft.exec_candidates.len())
                    };
                    let chosen = draft
                        .exec_candidates
                        .get(idx)
                        .map(|c| c.path.clone())
                        .unwrap_or_default();
                    Action::ChooseExecutable(chosen)
                }
                WizardStep::PickIcon => {
                    println!("Multiple icons found:");
                    for (i, c) in draft.icon_candidates.iter().enumerate() {
                        println!("  [{}] {}", i + 1, c.path);
                    }
                    let idx = if yes {
                        0
                    } else {
                        prompt_index("Select icon", draft.icon_candidates.len())
                    };
                    let chosen = draft.icon_candidates.get(idx).map(|c| c.path.clone());
                    Action::ChooseIcon(chosen)
                }
                WizardStep::ConfirmName => {
                    let name = if yes {
                        draft.name.clone()
                    } else {
                        prompt_line(&format!("App name [{}]: ", draft.name))
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| draft.name.clone())
                    };
                    Action::ConfirmName(name)
                }
            },
            State::OfferTrash { summary } => {
                println!(
                    "Installed {} to {}",
                    summary.name,
                    summary.install_dir.display()
                );
                if summary.kind == AppKind::Terminal && !summary.bin_on_path {
                    if opt_in_add_to_path
                        || (!yes
                            && prompt_yes_no(
                                "~/.local/bin is not in PATH. Add it to your shell profile?",
                                true,
                            ))
                    {
                        match add_bin_to_shell_rc(&roots.home_dir) {
                            Ok(rc) => {
                                println!("Updated {}. Restart your shell to apply.", rc.display())
                            }
                            Err(e) => eprintln!("Could not update shell profile: {e}"),
                        }
                    } else if yes {
                        println!(
                            "Note: ~/.local/bin is not in PATH (pass --add-to-path to update your shell rc)."
                        );
                    }
                }
                if opt_in_trash
                    || (!yes && prompt_yes_no("Move downloaded package to Trash?", true))
                {
                    Action::TrashSource
                } else {
                    if yes {
                        println!("Kept downloaded package (pass --trash to move it to Trash).");
                    }
                    Action::KeepSource
                }
            }
            State::Done { outcome } => {
                match outcome {
                    DoneOutcome::Installed { summary, trashed } => {
                        if *trashed {
                            println!(
                                "Done! {} installed and download moved to Trash.",
                                summary.name
                            );
                        } else {
                            println!("Done! {} installed.", summary.name);
                        }
                    }
                    DoneOutcome::HandedOff { path } => {
                        println!(
                            "{} does not look like an app; handed off to previous archive handler.",
                            path.display()
                        );
                    }
                }
                return ExitCode::SUCCESS;
            }
            State::Cancelled => {
                println!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            State::FailedRolledBack { error } => {
                eprintln!("easi: install failed (rolled back cleanly): {error}");
                return ExitCode::FAILURE;
            }
            State::Failed { error } => {
                eprintln!("easi: failed: {error}");
                return ExitCode::FAILURE;
            }
            State::Idle { .. }
            | State::Inspecting { .. }
            | State::HandingOff { .. }
            | State::Installing { .. }
            | State::Cancelling { .. }
            | State::RollingBack { .. }
            | State::Trashing { .. } => {
                return ExitCode::FAILURE;
            }
        };

        store.dispatch_and_run(
            next_action,
            |effect| runner.run(effect),
            |state| {
                if let State::Installing { step, progress, .. } = state {
                    println!("[{progress:3}%] {step:?}");
                }
            },
        );
    }
}

fn print_trust(trust: &TrustSummary) {
    println!("  Size:   {} bytes", trust.size_bytes);
    println!("  SHA256: {}", trust.sha256);
    if let Some(url) = &trust.origin_url {
        println!("  Origin: {url}");
    }
    match &trust.signature {
        SignatureStatus::Verified {
            detail,
            fingerprint,
        } => println!("  Sig:    OK ({detail})\n  Signer: {fingerprint}"),
        SignatureStatus::PresentUnverified(m) => println!("  Sig:    Unverified ({m})"),
        SignatureStatus::Invalid(m) => println!("  Sig:    INVALID ({m})"),
        SignatureStatus::None => println!("  Sig:    None"),
    }
    for w in &trust.warnings {
        println!("  Warn:   {w}");
    }
}

fn prompt_yes_no(question: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return default_yes;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    }
}

fn prompt_index(label: &str, max: usize) -> usize {
    print!("{label} [1-{max}] (default 1): ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return 0;
    }
    line.trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .filter(|&i| i < max)
        .unwrap_or(0)
}

fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}
