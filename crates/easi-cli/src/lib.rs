//! easi-cli: terminal view over the easi-core store. Parses argv into actions, renders state.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use easi_core::desktop::run_setup;
use easi_core::install::{add_bin_to_shell_rc, uninstall_app};
use easi_core::manifest::list_manifests;
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

    /// Register easi as the default handler for supported package MIME types.
    #[arg(long)]
    pub setup: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install an AppImage, tarball or zip.
    Install { path: PathBuf },
    /// Remove an app installed by easi.
    Uninstall { app: String },
    /// List apps installed by easi.
    List,
    /// Update easi itself.
    SelfUpdate,
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
        return run_setup_cmd(&roots);
    }

    match cli.command {
        Some(Command::Install { path }) => {
            maybe_notify_update(&roots);
            run_install_cmd(&roots, path, cli.yes)
        }
        Some(Command::Uninstall { app }) => {
            maybe_notify_update(&roots);
            run_uninstall_cmd(&roots, &app, cli.yes)
        }
        Some(Command::List) => {
            maybe_notify_update(&roots);
            run_list_cmd(&roots)
        }
        Some(Command::SelfUpdate) => run_self_update_cmd(),
        Some(Command::Config { auto_update }) => run_config_cmd(&roots, auto_update),
        None => {
            eprintln!(
                "easi: pass a subcommand (`install`, `uninstall`, `list`, `config`) or `--setup`"
            );
            ExitCode::FAILURE
        }
    }
}

fn maybe_notify_update(roots: &InstallRoots) {
    let current = env!("CARGO_PKG_VERSION");
    if let Some(latest) = easi_core::self_update::check_for_easi_update(roots, current) {
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
            println!("Updated easi from {from} to {to} ({}).", bin_path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: self-update failed: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_setup_cmd(roots: &InstallRoots) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("easi: cannot locate current executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run_setup(roots, &exe) {
        Ok(saved) => {
            println!("easi registered as default handler for supported package types.");
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
    match list_manifests(roots) {
        Ok(list) if list.is_empty() => {
            println!("No apps installed by easi.");
            ExitCode::SUCCESS
        }
        Ok(list) => {
            for m in list {
                let ver = m.version.as_deref().unwrap_or("-");
                let kind = match m.kind {
                    AppKind::Gui => "GUI",
                    AppKind::Terminal => "CLI",
                };
                println!("{:<24} {:<12} [{}]  {}", m.name, ver, kind, m.app_id);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_uninstall_cmd(roots: &InstallRoots, app: &str, yes: bool) -> ExitCode {
    if !yes && !prompt_yes_no(&format!("Uninstall '{app}'?"), false) {
        println!("Cancelled.");
        return ExitCode::SUCCESS;
    }
    match uninstall_app(roots, app) {
        Ok(m) => {
            println!("Uninstalled {} ({}).", m.name, m.app_id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("easi: {}", format_user_error(&e));
            ExitCode::FAILURE
        }
    }
}

fn run_install_cmd(roots: &InstallRoots, path: PathBuf, yes: bool) -> ExitCode {
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
                    if !yes && !prompt_yes_no("Allow easi to execute this installer script?", false)
                    {
                        Action::Cancel
                    } else {
                        Action::ConfirmInstall
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
                if summary.kind == AppKind::Terminal
                    && !summary.bin_on_path
                    && (yes
                        || prompt_yes_no(
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
                }
                if yes || prompt_yes_no("Move downloaded package to Trash?", true) {
                    Action::TrashSource
                } else {
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
            State::Failed { error, rolled_back } => {
                if *rolled_back {
                    eprintln!("easi: install failed (rolled back cleanly): {error}");
                } else {
                    eprintln!("easi: failed: {error}");
                }
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
        SignatureStatus::Verified(m) => println!("  Sig:    OK ({m})"),
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
