//! User settings: `<config>/nxnativev2/settings.toml` (Linux `~/.config/nxnativev2`, Windows `%APPDATA%\nxnativev2\config`).
//! Small values only; the large data lives under the data folder (`store.rs`).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_ANISETTE: &str = "https://mac-ani.jkcoxson.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Data folder; empty = the platform default (Linux `~/.local/share/nxnativev2`, Windows `%LOCALAPPDATA%\nxnativev2\data`)
    pub data_dir: Option<PathBuf>,
    /// The nxnative checkout whose scripts build the IPA
    pub nxnative_repo: Option<PathBuf>,
    /// Python that runs the scripts (needs lz4, capstone, fonttools, Pillow; backports.zstd before 3.14); empty = `<repo>/.venv`, else `python3`
    pub python: Option<PathBuf>,
    /// Anisette server for Apple sign-in (a Mac identity: Apple blocks Android ones since October 2026)
    pub anisette_url: String,
    /// The Apple ID last signed in with
    pub apple_id: Option<String>,
    /// Ask Apple for the Increased Memory Limit capability when installing
    pub increased_memory_limit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: None,
            nxnative_repo: None,
            python: None,
            anisette_url: DEFAULT_ANISETTE.into(),
            apple_id: None,
            increased_memory_limit: true,
        }
    }
}

pub fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "nxnativev2").context("no home folder")
}

fn path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("settings.toml"))
}

impl Settings {
    pub fn load() -> Result<Self> {
        let p = path()?;
        if !p.exists() {
            return Ok(Self::default());
        }
        toml::from_str(&fs::read_to_string(&p)?).with_context(|| format!("read {}", p.display()))
    }

    pub fn save(&self) -> Result<()> {
        let p = path()?;
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn data_dir(&self) -> Result<PathBuf> {
        match &self.data_dir {
            Some(d) => Ok(d.clone()),
            None => Ok(project_dirs()?.data_local_dir().to_path_buf()),
        }
    }
}
