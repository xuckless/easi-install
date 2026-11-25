//! Typed errors for easi-core and user-facing message table.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EasiError {
    #[error("path not found: {0}")]
    NotFound(PathBuf),

    #[error("unsupported or unrecognized file format: {0}")]
    UnsupportedFormat(PathBuf),

    #[error("unsafe path in archive (escapes install root): {0}")]
    PathEscape(String),

    #[error("refusing to overwrite unmanaged path: {0}")]
    UnmanagedOverwrite(PathBuf),

    #[error("app not installed: {0}")]
    NotInstalled(String),

    #[error("archive error: {0}")]
    Archive(String),

    #[error("appimage error: {0}")]
    AppImage(String),

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("release signature verification failed: {0}")]
    Signature(String),

    #[error(
        "downloaded update failed its {algo} integrity check (published {expected}, got {found})"
    )]
    DigestMismatch {
        algo: &'static str,
        expected: String,
        found: String,
    },

    #[error(
        "update blocked: publisher signing key changed for {app} (recorded {expected}, update has {found})"
    )]
    PublisherChanged {
        app: String,
        expected: String,
        found: String,
    },

    #[error("update blocked: untrusted download host {url} for {update_source}")]
    UntrustedDownloadHost { url: String, update_source: String },

    #[error("desktop integration error: {0}")]
    Desktop(String),

    #[error(
        "you don't have the supported dependencies to run this app: {detail}\nTo install it, run this command in your terminal:\n  {fix_command}"
    )]
    MissingRuntimeDependency { detail: String, fix_command: String },

    #[error("this is a {platform} package, not a Linux app.\nHint: {hint}")]
    WrongPlatform {
        platform: &'static str,
        hint: &'static str,
    },

    #[error(
        "this is a {kind} distro package — use your distro's software center or run:\n  {command}"
    )]
    DistroPackage { kind: &'static str, command: String },

    #[error(
        "install location {0} is on a `noexec` filesystem — apps placed there cannot execute.\nHint: remount without `noexec` or symlink ~/Applications to an executable partition."
    )]
    NoExecMount(PathBuf),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

struct ErrorHelp {
    prefix: &'static str,
    hint: &'static str,
}

const ERROR_HINTS: &[ErrorHelp] = &[
    ErrorHelp {
        prefix: "update blocked: publisher signing key changed",
        hint: "Nothing was installed. Confirm the new signing key with the developer, then accept and record it with `easi update <app> --retrust`.",
    },
    ErrorHelp {
        prefix: "update blocked: untrusted download host",
        hint: "The update source sent the download to a host it is not pinned to, which can mean a hijacked release; nothing was installed. If the publisher really moved its downloads, re-record the source with `easi set-source <app> <url>`.",
    },
    ErrorHelp {
        prefix: "downloaded update failed its",
        hint: "The file does not match the checksum its publisher listed; nothing was installed. Retry later — if it persists, the release or download mirror may be tampered with.",
    },
    ErrorHelp {
        prefix: "path not found",
        hint: "Check that the file still exists and was not moved.",
    },
    ErrorHelp {
        prefix: "unsafe path in archive",
        hint: "Blocked for safety: archive attempted to write outside its install directory.",
    },
    ErrorHelp {
        prefix: "refusing to overwrite unmanaged path",
        hint: "A file not created by easi already exists there.",
    },
    ErrorHelp {
        prefix: "unsupported or unrecognized",
        hint: "easi v1 supports AppImage, .tar.{gz,xz,zst,bz2}, and .zip.",
    },
];

/// Render an error with an optional actionable hint from the message table.
pub fn format_user_error(err: &EasiError) -> String {
    let msg = err.to_string();
    for entry in ERROR_HINTS {
        if msg.starts_with(entry.prefix) {
            return format!("{msg}\nHint: {}", entry.hint);
        }
    }
    msg
}
