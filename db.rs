//! Installed-package state (the Cellar) with a cached JSON snapshot.
//!
//! The Cellar on disk is the source of truth (so brew and pico can coexist);
//! `db.json` is a fast cache that is refreshed from the Cellar when it changes.

use crate::index::F;
use crate::paths::Paths;
use crate::version::{compare, keg_dir_name};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub version: String,
    pub revision: u32,
    /// Keg path relative to the prefix (`Cellar/jq/1.7.1`).
    pub rel: PathBuf,
    pub installed_at: u64,
    /// Names of installed dependencies at install time.
    #[serde(default)]
    pub deps: Vec<String>,
}

impl Entry {
    pub fn pkg_version(&self) -> String {
        keg_dir_name(&self.version, self.revision)
    }

    pub fn abs(&self, paths: &Paths) -> PathBuf {
        paths.prefix.join(&self.rel)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub schema: u32,
    /// Cellar fingerprint captured when this snapshot was written.
    #[serde(default)]
    pub cellar_sig: String,
    /// name → installed entry (a single version per name, like brew's active keg).
    #[serde(default)]
    pub installed: BTreeMap<String, Entry>,
}

pub struct Db {
    pub snap: Snapshot,
    paths: Paths,
    cellar_sig: String,
    dirty: bool,
}

impl Db {
    /// Open the state for `paths`. Fast path: no Cellar scan when `db.json`
    /// exists, matches this prefix, and the Cellar's mtime+entry-count is
    /// unchanged since it was written.
    pub fn open(paths: &Paths) -> Result<Self> {
        paths.ensure_home()?;
        let cellar_sig = cellar_signature(&paths.cellar);
        let db_file = paths.db_file.clone();
        if let Ok(data) = std::fs::read(&db_file) {
            if let Ok(snap) = serde_json::from_slice::<Snapshot>(&data) {
                if snap.schema == crate::INDEX_SCHEMA {
                    let trusted = if !snap.cellar_sig.is_empty() {
                        snap.cellar_sig == cellar_sig
                    } else {
                        read_sig(paths).map(|s| s == cellar_sig).unwrap_or(false)
                    };
                    if trusted {
                        return Ok(Self {
                            snap,
                            paths: paths.clone(),
                            cellar_sig,
                            dirty: false,
                        });
                    }
                }
            }
        }
        let mut snap = scan_cellar(paths)?;
        snap.cellar_sig = cellar_sig.clone();
        let mut db = Self {
            snap,
            paths: paths.clone(),
            cellar_sig,
            dirty: true,
        };
        db.save()?;
        db.dirty = false;
        Ok(db)
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.snap.installed.get(name)
    }

    /// True when `name` is installed and the bottle's version is newer.
    pub fn outdated(&self, f: &F) -> bool {
        match self.get(&f.name) {
            Some(e) => compare(&e.version, &f.version) == std::cmp::Ordering::Less,
            None => false,
        }
    }

    pub fn insert(&mut self, name: &str, entry: Entry) {
        self.snap.installed.insert(name.to_string(), entry);
        self.dirty = true;
    }

    pub fn remove(&mut self, name: &str) {
        self.snap.installed.remove(name);
        self.dirty = true;
    }

    /// Persist if changed.
    ///
    /// State is a cache over the Cellar, never a source of truth, so a
    /// read-only prefix (or a full disk) must not break read-only commands:
    /// failures here are swallowed and the next scan simply rebuilds.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.snap.schema = crate::INDEX_SCHEMA;
        let Ok(data) = serde_json::to_vec(&self.snap) else {
            return Ok(());
        };
        let tmp = self.paths.db_file.with_extension("json.tmp");
        if std::fs::create_dir_all(&self.paths.meta_dir).is_ok()
            && std::fs::write(&tmp, data).is_ok()
            && std::fs::rename(&tmp, &self.paths.db_file).is_ok()
        {
            // Our own writes must not count as external drift: record the
            // post-write signature *and* persist it in the db file itself so
            // the next process can trust the snapshot without rescanning.
            self.cellar_sig = cellar_signature(&self.paths.cellar);
            let _ = std::fs::write(sig_path(&self.paths), &self.cellar_sig);
        }
        self.dirty = false;
        Ok(())
    }
}

fn sig_path(paths: &Paths) -> PathBuf {
    paths.home.join("cellar.sig")
}

fn read_sig(paths: &Paths) -> Option<String> {
    std::fs::read_to_string(sig_path(paths))
        .ok()
        .map(|s| s.trim().to_string())
}

/// A cheap fingerprint of the Cellar: mtime (ns) + number of keg dirs.
/// O(number of formula directories) fingerprint. Adding/removing a formula
/// bumps the Cellar's mtime; a `brew link`/`unlink` or a new keg under an
/// existing name is caught by the per-name mtime sum, which stays cheap
/// (one `stat` per installed formula, none per keg).
fn cellar_signature(cellar: &std::path::Path) -> String {
    let cellar_mtime = std::fs::metadata(cellar)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut names = 0usize;
    let mut sum = 0u128;
    if let Ok(rd) = std::fs::read_dir(cellar) {
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                names += 1;
                sum = sum.wrapping_add(
                    std::fs::metadata(entry.path())
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                );
            }
        }
    }
    format!("{cellar_mtime}-{names}-{sum}")
}

/// Rebuild the snapshot from what is physically in the Cellar. Keeps tags from
/// any existing snapshot. A keg that has been `brew unlinked` still counts as
/// installed — matching `brew list`.
pub fn scan_cellar(paths: &Paths) -> Result<Snapshot> {
    let mut snap = Snapshot {
        schema: crate::INDEX_SCHEMA,
        cellar_sig: String::new(),
        installed: BTreeMap::new(),
    };
    let old = std::fs::read(&paths.db_file)
        .ok()
        .and_then(|d| serde_json::from_slice::<Snapshot>(&d).ok());
    let cellar = &paths.cellar;
    let Ok(rd) = std::fs::read_dir(cellar) else {
        return Ok(snap);
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(vers) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let mut best: Option<(String, u32, PathBuf)> = None;
        for v in vers.flatten() {
            if !v.path().is_dir() {
                continue;
            }
            let raw = v.file_name().to_string_lossy().to_string();
            let (ver, rev) = split_keg_name(&raw);
            let cand = (ver.clone(), rev, v.path());
            best = match best {
                None => Some(cand),
                Some(prev) => {
                    let ord = compare(&ver, &prev.0);
                    let take = ord == std::cmp::Ordering::Greater
                        || (ord == std::cmp::Ordering::Equal && rev > prev.1);
                    if take {
                        Some(cand)
                    } else {
                        Some(prev)
                    }
                }
            };
        }
        if let Some((ver, rev, path)) = best {
            let deps = old
                .as_ref()
                .and_then(|o| o.installed.get(&name))
                .map(|e| e.deps.clone())
                .unwrap_or_default();
            let installed_at = old
                .as_ref()
                .and_then(|o| o.installed.get(&name))
                .map(|e| e.installed_at)
                .unwrap_or(0);
            snap.installed.insert(
                name,
                Entry {
                    version: ver,
                    revision: rev,
                    rel: paths.rel(&path),
                    installed_at,
                    deps,
                },
            );
        }
    }
    Ok(snap)
}

/// `1.7.1_2` → (`1.7.1`, 2)
fn split_keg_name(raw: &str) -> (String, u32) {
    if let Some((v, r)) = raw.rsplit_once('_') {
        if !v.is_empty() && !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = r.parse::<u32>() {
                return (v.to_string(), n);
            }
        }
    }
    (raw.to_string(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_keg_names() {
        assert_eq!(split_keg_name("1.7.1"), ("1.7.1".into(), 0));
        assert_eq!(split_keg_name("1.7.1_2"), ("1.7.1".into(), 2));
        assert_eq!(split_keg_name("2024-01-01"), ("2024-01-01".into(), 0));
        assert_eq!(split_keg_name("openssl@4"), ("openssl@4".into(), 0));
    }

    #[test]
    fn scans_cellar_and_picks_newest() {
        let tmp = std::env::temp_dir().join(format!("pico-db-test-{}", std::process::id()));
        let paths = Paths::new(tmp.join("prefix"), tmp.join("home"));
        paths.ensure_install_dirs().unwrap();
        std::fs::create_dir_all(paths.cellar.join("jq/1.7.1/bin")).unwrap();
        std::fs::create_dir_all(paths.cellar.join("jq/1.8.0/bin")).unwrap();
        std::fs::create_dir_all(paths.cellar.join("oniguruma/6.9.9_2")).unwrap();
        let snap = scan_cellar(&paths).unwrap();
        assert_eq!(snap.installed["jq"].version, "1.8.0");
        assert_eq!(snap.installed["oniguruma"].revision, 2);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
