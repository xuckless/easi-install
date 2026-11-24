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

    #[error("desktop integration error: {0}")]
    Desktop(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

struct ErrorHelp {
    prefix: &'static str,
    hint: &'static str,
}

const ERROR_HINTS: &[ErrorHelp] = &[
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
