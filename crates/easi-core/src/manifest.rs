//! Per-app TOML manifest store (`~/.local/share/easi/installed/<id>.toml`).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::EasiError;
use crate::model::{AppKind, InstalledApp, UpdateSource};
use crate::roots::InstallRoots;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub app_id: String,
    pub name: String,
    pub version: Option<String>,
    pub source_filename: String,
    pub kind: AppKind,
    pub install_dir: PathBuf,
    #[serde(default)]
    pub developer: Option<String>,
    #[serde(default)]
    pub origin_url: Option<String>,
    #[serde(default)]
    pub signature_summary: Option<String>,
    #[serde(default)]
    pub update_source: Option<UpdateSource>,
    #[serde(default)]
    pub adopted: bool,
    pub created_files: Vec<PathBuf>,
    pub created_dirs: Vec<PathBuf>,
}

impl Manifest {
    pub fn to_installed_app(&self) -> InstalledApp {
        InstalledApp {
            app_id: self.app_id.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            source_filename: self.source_filename.clone(),
            install_dir: self.install_dir.clone(),
            kind: self.kind,
            developer: self.developer.clone(),
            origin_url: self.origin_url.clone(),
            signature_summary: self.signature_summary.clone(),
            adopted: self.adopted,
            size_bytes: dir_size_bytes(&self.install_dir),
            data_size_bytes: 0,
            cache_size_bytes: 0,
        }
    }

    pub fn to_installed_app_with_sizes(&self, roots: &InstallRoots) -> InstalledApp {
        let data_size_bytes = crate::install::find_app_user_data_dirs(roots, self)
            .iter()
            .map(|d| dir_size_bytes(d))
            .sum();
        let cache_size_bytes = crate::install::find_app_cache_dirs(roots, self)
            .iter()
            .map(|d| dir_size_bytes(d))
            .sum();
        InstalledApp {
            data_size_bytes,
            cache_size_bytes,
            ..self.to_installed_app()
        }
    }
}

/// Locate the icon file for `app_id` (checks hicolor theme sizes, then falls back to `.desktop` `Icon=`).
pub fn find_installed_icon(roots: &InstallRoots, app_id: &str) -> Option<PathBuf> {
    let sizes = [
        "scalable", "512x512", "256x256", "128x128", "64x64", "48x48",
    ];
    for sz in sizes {
        for ext in ["svg", "png"] {
            let p = roots.icons_dir.join(format!("{sz}/apps/{app_id}.{ext}"));
            if p.exists() {
                return Some(p);
            }
        }
    }
    let dp = roots.desktop_dir.join(format!("{app_id}.desktop"));
    if let Ok(text) = fs::read_to_string(&dp)
        && let Some(icon_val) = crate::inspect::desktop_field(&text, "Icon")
    {
        let p = PathBuf::from(icon_val.trim());
        if p.is_absolute() && p.exists() {
            return Some(p);
        }
        for sz in sizes {
            for ext in ["svg", "png"] {
                let c = roots
                    .icons_dir
                    .join(format!("{sz}/apps/{}.{ext}", icon_val.trim()));
                if c.exists() {
                    return Some(c);
                }
            }
        }
    }
    None
}

/// Sum regular file bytes under `path` without following symlinks.
pub fn dir_size_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        total = total.saturating_add(dir_size_bytes(&entry.path()));
    }
    total
}

pub fn format_human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn manifest_path(roots: &InstallRoots, app_id: &str) -> PathBuf {
    roots.manifests_dir().join(format!("{app_id}.toml"))
}

pub fn save_manifest(roots: &InstallRoots, manifest: &Manifest) -> Result<PathBuf, EasiError> {
    let dir = roots.manifests_dir();
    fs::create_dir_all(&dir)?;
    let path = manifest_path(roots, &manifest.app_id);
    let text = toml::to_string_pretty(manifest).map_err(|e| EasiError::Manifest(e.to_string()))?;
    fs::write(&path, text)?;
    Ok(path)
}

pub fn load_manifest(path: &Path) -> Result<Manifest, EasiError> {
    let text = fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|e| EasiError::Manifest(e.to_string()))
}

pub fn find_manifest_by_id(roots: &InstallRoots, app_id: &str) -> Option<Manifest> {
    let path = manifest_path(roots, app_id);
    if !path.exists() {
        return None;
    }
    load_manifest(&path).ok()
}

pub fn list_manifests(roots: &InstallRoots) -> Result<Vec<Manifest>, EasiError> {
    let dir = roots.manifests_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut manifests = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("toml")
            && let Ok(m) = load_manifest(&path)
        {
            manifests.push(m);
        }
    }
    manifests.sort_by_key(|a| a.name.to_lowercase());
    Ok(manifests)
}

/// Match an installed app by exact `app_id`, slug without `easi.`, or case-insensitive `name`.
pub fn resolve_installed(roots: &InstallRoots, query: &str) -> Result<Manifest, EasiError> {
    if let Some(m) = find_manifest_by_id(roots, query) {
        return Ok(m);
    }
    let lower = query.to_ascii_lowercase();
    for m in list_manifests(roots)? {
        let bare_id = m.app_id.strip_prefix("easi.").unwrap_or(&m.app_id);
        if m.app_id.eq_ignore_ascii_case(&lower)
            || bare_id.eq_ignore_ascii_case(&lower)
            || m.name.eq_ignore_ascii_case(&lower)
        {
            return Ok(m);
        }
    }
    Err(EasiError::NotInstalled(query.to_string()))
}

pub fn delete_manifest(roots: &InstallRoots, app_id: &str) -> Result<(), EasiError> {
    let path = manifest_path(roots, app_id);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}
