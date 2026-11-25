//! Fuzz the untrusted-input parsers: magic-byte detection and every format's inspector.
//! Run with `cargo +nightly fuzz run inspect_package` from the repo root.

#![no_main]

use easi_core::detect::{detect_format, inspect_contents};
use easi_core::model::Format;
use libfuzzer_sys::fuzz_target;

const FORMATS: &[Format] = &[
    Format::AppImageType1,
    Format::AppImageType2,
    Format::Tar,
    Format::TarGz,
    Format::TarXz,
    Format::TarZst,
    Format::TarBz2,
    Format::TarLz4,
    Format::Zip,
    Format::SevenZ,
    Format::Jar,
    Format::BareElf,
    Format::ShellInstaller,
];

fuzz_target!(|data: &[u8]| {
    let _ = detect_format(data);
    // First byte picks the parser so one corpus covers every format.
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let format = FORMATS[usize::from(selector) % FORMATS.len()];
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let path = dir.path().join("package");
    if std::fs::write(&path, body).is_ok() {
        let _ = inspect_contents(&path, format);
    }
});
