//! `easi` binary: chooses GUI vs CLI by argv and display environment.
//! - `easi` (no args) → GUI (Idle)
//! - `easi <path>` (file-manager double-click) → GUI opened with `<path>`, or CLI if headless
//! - `easi install|uninstall|list|self-update|--setup|...` → CLI

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

const CLI_SUBCOMMANDS: &[&str] = &[
    "install",
    "inspect",
    "export",
    "import",
    "uninstall",
    "list",
    "update",
    "check-updates",
    "self-update",
    "self-uninstall",
    "learn-wmclass",
    "scan",
    "adopt",
    "repair",
    "organize",
    "forget",
    "set-source",
    "config",
    "journal",
    "help",
];

fn has_gui_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

    // Hidden sandbox helpers re-exec this binary; they must start before any GUI/CLI setup.
    if let Some(code) = easi_core::sandbox::run_helper(&args) {
        return code;
    }

    if args.len() == 1 {
        return launch_gui(None);
    }

    // Single positional path (e.g. double-click from file manager via `Exec=easi %f`).
    if args.len() == 2 {
        let arg = args[1].to_string_lossy();
        let is_cli_word = arg.starts_with('-') || CLI_SUBCOMMANDS.contains(&arg.as_ref());
        if !is_cli_word {
            let path = PathBuf::from(&args[1]);
            if has_gui_session() {
                return launch_gui(Some(path));
            }
            return easi_cli::run(easi_cli::Cli {
                yes: false,
                setup: false,
                claim_archives: false,
                restore_handlers: false,
                command: Some(easi_cli::Command::Install {
                    path,
                    dry_run: false,
                    trash: false,
                    add_to_path: false,
                    allow_script: false,
                }),
            });
        }
    }

    easi_cli::run(easi_cli::Cli::parse())
}

fn launch_gui(initial_path: Option<PathBuf>) -> ExitCode {
    match easi_gui::run(initial_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("easi: failed to start GUI: {e}");
            ExitCode::FAILURE
        }
    }
}
