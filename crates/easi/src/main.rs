//! `easi` binary: no args → GUI, otherwise CLI.
//! File-manager double-click (`easi <path>`) routing to the GUI lands in M4.

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        return easi_cli::run(easi_cli::Cli::parse());
    }
    match easi_gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("easi: failed to start GUI: {e}");
            ExitCode::FAILURE
        }
    }
}
