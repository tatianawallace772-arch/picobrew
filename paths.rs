//! Filesystem layout: where the prefix, Cellar, caches and state live.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    /// State/cache directory (index cache, download cache).
    pub home: PathBuf,
    /// Install prefix (defaults to the platform Homebrew prefix).
    pub prefix: PathBuf,
    pub cellar: PathBuf,
    pub opt: PathBuf,
    pub downloads: PathBuf,
    pub meta_dir: PathBuf,
    pub db_file: PathBuf,
}

impl Paths {
    /// Build the layout from an explicit prefix + home (used by tests).
    pub fn new(prefix: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        let prefix = prefix.into();
        let home = home.into();
        Self {
            cellar: prefix.join("Cellar"),
            opt: prefix.join("opt"),
            downloads: home.join("downloads"),
            meta_dir: prefix.join(".picobrew"),
            db_file: prefix.join(".picobrew").join("db.json"),
            prefix,
            home,
        }
    }

    /// Resolve from CLI flag / env vars / platform defaults.
    pub fn resolve(prefix_override: Option<PathBuf>) -> Result<Self> {
        let home = match std::env::var_os("PICOBREW_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => user_home()?.join(".picobrew"),
        };
        let prefix = prefix_override
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| {
                std::env::var_os("PICOBREW_PREFIX")
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_prefix);
        Ok(Self::new(prefix, home))
    }

    pub fn index_cache(&self) -> PathBuf {
        self.home.join("index.json")
    }

    pub fn formulas_path(&self) -> PathBuf {
        self.home.join("formulas.fmt")
    }

    pub fn aliases_path(&self) -> PathBuf {
        self.home.join("aliases.json")
    }

    pub fn ensure_home(&self) -> Result<()> {
        std::fs::create_dir_all(&self.home)
            .with_context(|| format!("creating cache dir {}", self.home.display()))
    }

    pub fn ensure_install_dirs(&self) -> Result<()> {
        for d in [&self.home, &self.cellar, &self.opt, &self.downloads, &self.meta_dir] {
            std::fs::create_dir_all(d)
                .with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// Path relative to the prefix (`Cellar/jq/1.7.1`), used for portable state.
    pub fn rel(&self, p: &Path) -> PathBuf {
        p.strip_prefix(&self.prefix).unwrap_or(p).to_path_buf()
    }
}

/// The platform-default Homebrew prefix.
pub fn default_prefix() -> PathBuf {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            PathBuf::from("/opt/homebrew")
        } else {
            PathBuf::from("/usr/local")
        }
    } else {
        PathBuf::from("/home/linuxbrew/.linuxbrew")
    }
}

fn user_home() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => anyhow::bail!("HOME is not set; set PICOBREW_HOME to choose a cache directory"),
    }
}
