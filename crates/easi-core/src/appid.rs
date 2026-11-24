//! Declarative App ID resolver, version/arch token stripper, and StartupWMClass guesser.

use std::path::Path;

/// Tokens stripped from filenames/dirnames when synthesizing an app slug.
const STRIP_TOKENS: &[&str] = &[
    "linux", "x86_64", "x86-64", "amd64", "aarch64", "arm64", "armv7l", "armhf", "x64", "x86",
    "i686", "i386", "unknown", "gnu", "musl", "glibc", "appimage", "portable", "release", "stable",
    "beta", "alpha", "rc", "bin", "binary", "desktop", "tar", "gz", "xz", "zst", "bz2", "zip",
];

#[derive(Debug, Default)]
pub struct IdInputs<'a> {
    pub metainfo_xml: Option<&'a str>,
    pub desktop_filename: Option<&'a str>,
    pub top_dir: Option<&'a str>,
    pub source_filename: Option<&'a str>,
    pub main_exec: Option<&'a str>,
}

/// Resolve `(app_id, synthesized)` following the locked source-priority table:
/// 1. AppStream `metainfo/*.xml` `<id>`
/// 2. Embedded `.desktop` basename (without `.desktop`)
/// 3. Synthesized `easi.<slug>` from archive top dir / source filename / main ELF name
pub fn resolve_app_id(inputs: &IdInputs<'_>) -> (String, bool) {
    if let Some(xml) = inputs.metainfo_xml
        && let Some(id) = extract_xml_tag(xml, "id")
    {
        let clean = id.trim().trim_end_matches(".desktop");
        if !clean.is_empty() {
            return (clean.to_string(), false);
        }
    }

    if let Some(desktop) = inputs.desktop_filename {
        let base = Path::new(desktop)
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        let stem = base.trim_end_matches(".desktop");
        if !stem.is_empty() {
            return (stem.to_string(), false);
        }
    }

    for candidate in [inputs.top_dir, inputs.source_filename, inputs.main_exec]
        .into_iter()
        .flatten()
    {
        let (slug, _) = slug_and_version(candidate);
        if !slug.is_empty() {
            return (format!("easi.{slug}"), true);
        }
    }

    ("easi.app".into(), true)
}

/// Extract the first `<tag>...</tag>` inner text from an AppStream XML snippet.
pub fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}

/// Strip known archive extensions, arch/OS tokens, and version numbers.
/// Returns `(lowercase_slug, first_detected_version)`.
pub fn slug_and_version(raw: &str) -> (String, Option<String>) {
    let base = Path::new(raw)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string());

    // Strip compound archive extensions first so `.tar.gz` doesn't leave stray tokens.
    let mut stem = base.as_str();
    for ext in [
        ".tar.gz",
        ".tar.xz",
        ".tar.zst",
        ".tar.bz2",
        ".tar.lz4",
        ".tgz",
        ".txz",
        ".AppImage",
        ".appimage",
        ".zip",
        ".7z",
        ".jar",
        ".tar",
        ".run",
        ".sh",
        ".bin",
        ".desktop",
    ] {
        if let Some(s) = stem.strip_suffix(ext) {
            stem = s;
            break;
        }
    }

    // If the name is reverse-DNS (e.g. org.godotengine.Godot), keep last segment for slug.
    let parts: Vec<&str> = stem
        .split(['-', '_', ' '])
        .filter(|s| !s.is_empty())
        .collect();
    let mut kept = Vec::new();
    let mut version = None;

    for part in parts {
        let lower = part.to_ascii_lowercase();
        if is_version_token(&lower) {
            if version.is_none() {
                version = Some(lower.trim_start_matches('v').to_string());
            }
            continue;
        }
        if STRIP_TOKENS.contains(&lower.as_str()) {
            continue;
        }
        let cleaned: String = lower
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
            .collect();
        if !cleaned.is_empty() {
            kept.push(cleaned);
        }
    }

    let slug = kept.join("-");
    (slug, version)
}

fn is_version_token(token: &str) -> bool {
    let t = token.strip_prefix('v').unwrap_or(token);
    let mut chars = t.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_digit() && t.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Convert a slug or raw filename into a clean display name.
pub fn display_name_from_slug(slug: &str) -> String {
    slug.split(['-', '_', '.'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Guess `StartupWMClass` from the main executable basename.
pub fn guess_startup_wm_class(exec_path: &str) -> Option<String> {
    let base = Path::new(exec_path).file_name()?.to_string_lossy();
    let stem = base
        .trim_end_matches(".AppImage")
        .trim_end_matches(".appimage")
        .trim_end_matches(".sh");
    let (slug, _) = slug_and_version(stem);
    if slug.is_empty() { None } else { Some(slug) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_resolver_table() {
        struct Row<'a> {
            name: &'a str,
            inputs: IdInputs<'a>,
            expected_id: &'a str,
            expected_synth: bool,
        }

        let rows = [
            Row {
                name: "appstream metainfo wins over desktop and filename",
                inputs: IdInputs {
                    metainfo_xml: Some("<component><id>org.godotengine.Godot</id></component>"),
                    desktop_filename: Some("godot.desktop"),
                    source_filename: Some("Godot_v4.3-stable_linux.x86_64.zip"),
                    ..Default::default()
                },
                expected_id: "org.godotengine.Godot",
                expected_synth: false,
            },
            Row {
                name: "embedded desktop wins over filename",
                inputs: IdInputs {
                    desktop_filename: Some("usr/share/applications/obsidian.desktop"),
                    source_filename: Some("Obsidian-1.6.7.AppImage"),
                    ..Default::default()
                },
                expected_id: "obsidian",
                expected_synth: false,
            },
            Row {
                name: "real-world firefox tarball",
                inputs: IdInputs {
                    source_filename: Some("firefox-128.0.tar.xz"),
                    ..Default::default()
                },
                expected_id: "easi.firefox",
                expected_synth: true,
            },
            Row {
                name: "real-world ripgrep musl tarball",
                inputs: IdInputs {
                    source_filename: Some("ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz"),
                    ..Default::default()
                },
                expected_id: "easi.ripgrep",
                expected_synth: true,
            },
            Row {
                name: "real-world vscode tarball",
                inputs: IdInputs {
                    source_filename: Some("VSCode-linux-x64-1.92.0.tar.gz"),
                    ..Default::default()
                },
                expected_id: "easi.vscode",
                expected_synth: true,
            },
            Row {
                name: "real-world obsidian appimage",
                inputs: IdInputs {
                    source_filename: Some("Obsidian-1.6.7-arm64.AppImage"),
                    ..Default::default()
                },
                expected_id: "easi.obsidian",
                expected_synth: true,
            },
        ];

        for row in rows {
            let (id, synth) = resolve_app_id(&row.inputs);
            assert_eq!(id, row.expected_id, "id mismatch in '{}'", row.name);
            assert_eq!(
                synth, row.expected_synth,
                "synthesized mismatch in '{}'",
                row.name
            );
        }
    }

    #[test]
    fn version_extraction_from_filenames() {
        let cases = [
            ("Obsidian-1.6.7.AppImage", "obsidian", Some("1.6.7")),
            (
                "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz",
                "ripgrep",
                Some("14.1.0"),
            ),
            ("my-cool-app_v2.3.4_linux.zip", "my-cool-app", Some("2.3.4")),
        ];
        for (input, exp_slug, exp_ver) in cases {
            let (slug, ver) = slug_and_version(input);
            assert_eq!(slug, exp_slug);
            assert_eq!(ver.as_deref(), exp_ver);
        }
    }
}
