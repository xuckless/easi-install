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
    pub update_source: Option<UpdateSource>,
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
        }
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
