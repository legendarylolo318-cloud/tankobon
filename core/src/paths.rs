use anyhow::{Context, Result};
use std::path::PathBuf;

/// Resolved application directories.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    /// Overrides the default local-library folder (used on Android).
    pub local: Option<PathBuf>,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let d = directories::ProjectDirs::from("org", "tankobon", "Tankobon")
            .context("cannot determine home directory")?;
        Ok(Self::at(
            d.config_dir().to_path_buf(),
            d.data_dir().to_path_buf(),
            d.cache_dir().to_path_buf(),
        ))
    }

    pub fn at(config: PathBuf, data: PathBuf, cache: PathBuf) -> Self {
        Self { config, data, cache, local: None }
    }

    pub fn ensure(&self) -> Result<()> {
        for p in [&self.config, &self.data, &self.cache, &self.covers_dir(), &self.local_dir()] {
            std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
        }
        Ok(())
    }

    pub fn db_file(&self) -> PathBuf {
        self.data.join("library.sqlite3")
    }
    pub fn covers_dir(&self) -> PathBuf {
        self.cache.join("covers")
    }
    pub fn local_dir(&self) -> PathBuf {
        self.local.clone().unwrap_or_else(|| self.data.join("local"))
    }
    pub fn default_download_dir(&self) -> PathBuf {
        self.data.join("downloads")
    }
}
