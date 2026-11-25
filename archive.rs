//! Platform-neutral trampoline for symlinks and executable bits.
//! Symlinks matter on unix; on other platforms calls degrade to copies.

use anyhow::{Context, Result};
use std::path::Path;

#[cfg(unix)]
pub fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("linking {} → {}", link.display(), target.display()))
}

#[cfg(not(unix))]
pub fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::fs::copy(target, link)
        .with_context(|| format!("copying {} → {}", link.display(), target.display()))?;
    Ok(())
}
