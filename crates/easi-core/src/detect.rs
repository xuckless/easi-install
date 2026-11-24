//! Magic-byte format detection and read-only archive / AppImage inspection.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use backhand::{FilesystemReader, InnerNode};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::Archive;
use xz2::read::XzDecoder;
use zip::ZipArchive;

use crate::error::EasiError;
use crate::model::Format;

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
    let mut header = [0u8; 64];
    let n = file.read(&mut header)?;
    detect_format(&header[..n]).ok_or_else(|| EasiError::UnsupportedFormat(path.to_path_buf()))
}

/// Metadata collected for one entry during read-only inspection (no disk extraction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_elf: bool,
    pub is_script: bool,
}

/// Small metadata files extracted in memory during inspection.
#[derive(Debug, Clone, Default)]
pub struct PackageContents {
    pub entries: Vec<ArchiveEntry>,
    pub desktop_files: Vec<(String, String)>,
    pub metainfo_files: Vec<(String, String)>,
    pub embedded_icons: Vec<(String, Vec<u8>)>,
    pub has_appimage_sig_section: bool,
}

const MAX_METADATA_BYTES: u64 = 256 * 1024;
const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;

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

/// Inspect archive or AppImage without extracting to disk or executing untrusted code.
pub fn inspect_contents(path: &Path, format: Format) -> Result<PackageContents, EasiError> {
    match format {
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
        Format::Zip => list_zip(path),
        Format::AppImageType2 => list_appimage_type2(path),
        Format::AppImageType1 => list_appimage_type1(path),
    }
}

fn list_tar<R: Read>(reader: R) -> Result<PackageContents, EasiError> {
    let mut archive = Archive::new(reader);
    let mut contents = PackageContents::default();
    let entries = archive
        .entries()
        .map_err(|e| EasiError::Archive(e.to_string()))?;

    for item in entries {
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
        let mut is_script = false;

        if !is_dir && !is_symlink && size > 0 {
            if (is_desktop_path(&raw_path) || is_metainfo_path(&raw_path))
                && size <= MAX_METADATA_BYTES
            {
                let mut text = String::new();
                if entry.read_to_string(&mut text).is_ok() {
                    if is_desktop_path(&raw_path) {
                        contents.desktop_files.push((raw_path.clone(), text));
                    } else {
                        contents.metainfo_files.push((raw_path.clone(), text));
                    }
                }
            } else {
                let mut magic = [0u8; 4];
                let n = entry.read(&mut magic).unwrap_or(0);
                is_elf = n >= 4 && &magic == b"\x7fELF";
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
            is_script,
        });
    }
    Ok(contents)
}

fn list_zip(path: &Path) -> Result<PackageContents, EasiError> {
    let file = File::open(path)?;
    let mut zip =
        ZipArchive::new(BufReader::new(file)).map_err(|e| EasiError::Archive(e.to_string()))?;
    let mut contents = PackageContents::default();

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
        let mut is_script = false;

        if is_symlink && size <= 4096 {
            let mut target = String::new();
            if entry.read_to_string(&mut target).is_ok() {
                symlink_target = Some(target);
            }
        } else if !is_dir && size > 0 {
            if (is_desktop_path(&raw_path) || is_metainfo_path(&raw_path))
                && size <= MAX_METADATA_BYTES
            {
                let mut text = String::new();
                if entry.read_to_string(&mut text).is_ok() {
                    if is_desktop_path(&raw_path) {
                        contents.desktop_files.push((raw_path.clone(), text));
                    } else {
                        contents.metainfo_files.push((raw_path.clone(), text));
                    }
                }
            } else {
                let mut magic = [0u8; 4];
                let n = entry.read(&mut magic).unwrap_or(0);
                is_elf = n >= 4 && &magic == b"\x7fELF";
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

fn list_appimage_type2(path: &Path) -> Result<PackageContents, EasiError> {
    let mut file = File::open(path)?;
    let (offset, has_sig_section) = find_appimage_squashfs_offset(&mut file)?;
    let reader = BufReader::new(file);
    let fs = FilesystemReader::from_reader_with_offset(reader, offset)
        .map_err(|e| EasiError::AppImage(e.to_string()))?;

    let mut contents = PackageContents {
        has_appimage_sig_section: has_sig_section,
        ..Default::default()
    };

    for node in fs.files() {
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
                    is_script: false,
                });
            }
            InnerNode::File(f) => {
                let size = f.file_len() as u64;
                if (is_desktop_path(&rel) || is_metainfo_path(&rel)) && size <= MAX_METADATA_BYTES {
                    let mut buf = Vec::new();
                    if fs.file(f).reader().read_to_end(&mut buf).is_ok()
                        && let Ok(text) = String::from_utf8(buf)
                    {
                        if is_desktop_path(&rel) {
                            contents.desktop_files.push((rel.clone(), text));
                        } else {
                            contents.metainfo_files.push((rel.clone(), text));
                        }
                    }
                } else if is_icon_path(&rel) && size <= MAX_ICON_BYTES {
                    let mut buf = Vec::new();
                    if fs.file(f).reader().read_to_end(&mut buf).is_ok() {
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

fn list_appimage_type1(path: &Path) -> Result<PackageContents, EasiError> {
    // Type 1 AppImages embed an ISO9660 image. We treat the AppImage itself as the executable
    // and avoid executing it; metadata falls back cleanly to filename-based synthesis.
    let meta = std::fs::metadata(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app.AppImage".into());
    Ok(PackageContents {
        entries: vec![ArchiveEntry {
            path: name,
            size: meta.len(),
            mode: 0o755,
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_elf: true,
            is_script: false,
        }],
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
