//! Keg linking: `opt` symlinks plus two-level sweeps into `prefix/bin`, `lib`, …
//!
//! The link table is recorded inside each keg (`.picobrew/links.json`) so an
//! unlink never needs to guess — and so a keg installed by pico can be removed
//! even if state is lost.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const SKIP_TOP: [&str; 2] = [".brew", ".picobrew"];

fn links_file(keg: &Path) -> PathBuf {
    keg.join(".picobrew").join("links.json")
}

/// Symlink every `<dir>/<file>` inside the keg into `<prefix>/<dir>/<file>`.
/// Returns the number of links created or refreshed.
pub fn link_keg(paths: &Paths, name: &str, keg: &Path) -> Result<usize> {
    let opt = paths.opt.join(name);
    // opt/<name> → Cellar/<name>/<ver>
    if opt.is_symlink() || opt.exists() {
        let _ = std::fs::remove_file(&opt);
    }
    if let Some(parent) = opt.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::archive::symlink(keg, &opt)?;

    let mut linked: Vec<String> = Vec::new();
    let rd = std::fs::read_dir(keg).with_context(|| format!("reading keg {}", keg.display()))?;
    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname.starts_with('.') || SKIP_TOP.contains(&fname.as_str()) {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        let dir_rel = fname; // "bin", "lib", "share", …
        let Ok(inner) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let prefix_dir = paths.prefix.join(&dir_rel);
        std::fs::create_dir_all(&prefix_dir).ok();
        for f in inner.flatten() {
            let leaf = f.file_name().to_string_lossy().to_string();
            let rel = format!("{dir_rel}/{leaf}");
            let dst = paths.prefix.join(&rel);
            // Relative target: ../Cellar/<name>/<ver>/bin/foo from <prefix>/bin
            let target_rel = Path::new("..")
                .join(&*paths.rel(keg))
                .join(&rel);
            link_one(&dst, &target_rel)?;
            linked.push(rel);
        }
    }
    std::fs::create_dir_all(keg.join(".picobrew")).ok();
    std::fs::write(
        links_file(keg),
        serde_json::to_vec_pretty(&serde_json::json!({ "links": linked }))?,
    )?;
    Ok(linked.len())
}

fn link_one(dst: &Path, target_rel: &Path) -> Result<()> {
    if dst.is_symlink() {
        // Already ours? (same relative target)
        if let Ok(cur) = std::fs::read_link(dst) {
            if cur == target_rel {
                return Ok(());
            }
        }
        let _ = std::fs::remove_file(dst);
    } else if dst.exists() {
        // A real file where we want a symlink: leave it, the user owns it.
        return Ok(());
    }
    crate::archive::symlink(target_rel, dst)
}

/// Remove the links a keg owns. Safe to call twice.
pub fn unlink_keg(paths: &Paths, name: &str, keg: &Path) -> Result<()> {
    let mut rels: Vec<String> = match std::fs::read(links_file(keg)) {
        Ok(data) => serde_json::from_slice::<serde_json::Value>(&data)
            .ok()
            .and_then(|v| {
                v.get("links").and_then(|l| l.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default(),
        Err(_) => {
            // Fall back to walking the keg.
            let mut out = Vec::new();
            if let Ok(rd) = std::fs::read_dir(keg) {
                for e in rd.flatten() {
                    let d = e.file_name().to_string_lossy().to_string();
                    if d.starts_with('.') {
                        continue;
                    }
                    if let Ok(inner) = std::fs::read_dir(e.path()) {
                        for f in inner.flatten() {
                            out.push(format!("{d}/{}", f.file_name().to_string_lossy()));
                        }
                    }
                }
            }
            out
        }
    };
    rels.sort();
    rels.dedup();
    let target_rel = Path::new("..").join(paths.rel(keg));
    for rel in &rels {
        let dst = paths.prefix.join(rel);
        if dst.is_symlink() {
            if let Ok(cur) = std::fs::read_link(&dst) {
                // Only remove links that point into this keg.
                if cur.starts_with(&target_rel) || cur == target_rel.join(rel).parent().unwrap_or(Path::new("")) {
                    let _ = std::fs::remove_file(&dst);
                }
            }
        }
    }
    let opt = paths.opt.join(name);
    if opt.is_symlink() {
        if let Ok(cur) = std::fs::read_link(&opt) {
            let ours = cur == keg
                || cur == paths.rel(keg)
                || cur == paths.prefix.join(paths.rel(keg));
            if ours {
                let _ = std::fs::remove_file(&opt);
            }
        }
    }
    Ok(())
}

/// Executables a keg provides under `bin/`.
pub fn keg_binaries(keg: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(keg.join("bin")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                out.push(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    out
}

/// True when `dir` is on PATH.
pub fn dir_on_path(dir: &Path) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').any(|p| Path::new(p) == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_unlinks_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("pico-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = Paths::new(tmp.join("prefix"), tmp.join("home"));
        paths.ensure_install_dirs().unwrap();
        let keg = paths.cellar.join("jq/1.7.1");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::create_dir_all(keg.join("lib")).unwrap();
        std::fs::write(keg.join("bin/jq"), "bin").unwrap();
        std::fs::write(keg.join("lib/libjq.dylib"), "lib").unwrap();

        let n = link_keg(&paths, "jq", &keg).unwrap();
        assert_eq!(n, 2);
        assert!(paths.prefix.join("bin/jq").is_symlink());
        assert!(paths.opt.join("jq").is_symlink());
        assert!(dir_on_path(Path::new("/nonexistent-pico-test-dir")) == false);

        unlink_keg(&paths, "jq", &keg).unwrap();
        assert!(!paths.prefix.join("bin/jq").exists() && !paths.prefix.join("bin/jq").is_symlink());
        assert!(!paths.opt.join("jq").is_symlink());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn leaves_user_owned_files_alone() {
        let tmp = std::env::temp_dir().join(format!("pico-link2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = Paths::new(tmp.join("prefix"), tmp.join("home"));
        paths.ensure_install_dirs().unwrap();
        std::fs::create_dir_all(paths.prefix.join("bin")).unwrap();
        std::fs::write(paths.prefix.join("bin/jq"), "user file").unwrap();
        let keg = paths.cellar.join("jq/1.7.1");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::write(keg.join("bin/jq"), "keg").unwrap();
        link_keg(&paths, "jq", &keg).unwrap();
        assert!(!paths.prefix.join("bin/jq").is_symlink());
        assert_eq!(std::fs::read_to_string(paths.prefix.join("bin/jq")).unwrap(), "user file");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
