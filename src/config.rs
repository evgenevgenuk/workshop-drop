//! Paths, stored in `config.toml` next to the exe.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const GMOD_APP_ID: u64 = 4000;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    /// Server folder that contains srcds_win64.exe.
    pub server_dir: PathBuf,
    /// The server's garrysmod\addons folder.
    pub addons_dir: PathBuf,
    /// Folder that contains steamcmd.exe.
    pub steamcmd_dir: PathBuf,
    /// Put a small loader addon into addons that calls resource.AddWorkshop for every ws_<id>_* folder.
    #[serde(default = "default_true")]
    pub create_loader: bool,
}

impl Config {
    pub fn steamcmd_exe(&self) -> PathBuf {
        self.steamcmd_dir.join("steamcmd.exe")
    }

    /// `<steamcmd>\steamapps\workshop\content\4000\<id>\`
    pub fn workshop_content_dir(&self, id: u64) -> PathBuf {
        self.steamcmd_dir
            .join("steamapps")
            .join("workshop")
            .join("content")
            .join(GMOD_APP_ID.to_string())
            .join(id.to_string())
    }

    /// The program's own temp folder.
    pub fn temp_dir() -> PathBuf {
        std::env::temp_dir().join("workshop_drop")
    }

    /// First-run guess: if a server folder is picked later, addons is filled in automatically.
    pub fn suggested() -> Config {
        Config { create_loader: true, ..Default::default() }
    }

    /// Checks the paths. Returns non-fatal warnings.
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        if self.addons_dir.as_os_str().is_empty() || !self.addons_dir.is_dir() {
            bail!("addons folder does not exist: {}", self.addons_dir.display());
        }
        if !self.steamcmd_exe().is_file() {
            bail!("steamcmd.exe not found in {}", self.steamcmd_dir.display());
        }
        if !self.server_dir.join("srcds_win64.exe").is_file() && !self.server_dir.join("srcds.exe").is_file() {
            warnings.push(format!("srcds_win64.exe not found in {} (just a warning)", self.server_dir.display()));
        }
        Ok(warnings)
    }
}

/// `config.toml` next to the exe (or in the current folder if the exe path is unknown).
pub fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config.toml")
}

/// `Ok(None)` means there is no config yet (first run).
pub fn load() -> Result<Option<Config>> {
    let path = config_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("error in {}", path.display()))?;
    Ok(Some(cfg))
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path();
    let text = toml::to_string_pretty(cfg)?;
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}
