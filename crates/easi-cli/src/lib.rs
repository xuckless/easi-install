//! easi-cli: terminal view over the easi-core store. Parses argv into actions, renders state.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

    #[command(subcommand)]
    pub command: Command,
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
}

pub fn run(cli: Cli) -> std::process::ExitCode {
    eprintln!(
        "easi: {:?} not implemented yet (see TODO.md M2)",
        cli.command
    );
    std::process::ExitCode::FAILURE
}
