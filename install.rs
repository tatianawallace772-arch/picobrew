//! The install / upgrade / uninstall pipeline.
//!
//! Shape of `install`:
//!   1. load the compact index (cached: single-digit ms),
//!   2. resolve the dependency graph in parallel,
//!   3. download all needed bottles concurrently (sha256 verified while streaming),
//!   4. extract + link in topological order,
//!   5. record state and summarise.
//!
//! Mutating commands hold an exclusive lock on the cache dir, so two picobrew
//! processes can never corrupt the Cellar.

use crate::bottle;
use crate::db::{Db, Entry};
use crate::deps::Resolver;
use crate::index::{self, F};
use crate::linker;
use crate::net;
use crate::paths::Paths;
use crate::ui::{self, human_bytes, human_dur, Progress};
use crate::version::compare;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct InstallOpts {
    pub jobs: usize,
    pub dry_run: bool,
    pub force: bool,
}

impl Default for InstallOpts {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            jobs: (cpus * 2).clamp(1, 8),
            dry_run: false,
            force: false,
        }
    }
}

pub struct Report {
    pub installed: Vec<String>,
    pub skipped: Vec<String>,
    pub bytes: u64,
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------
// Locking
// ---------------------------------------------------------------------------

/// Exclusive lock over the picobrew home. Released when the guard drops.
pub struct Lock {
    _file: std::fs::File,
}

pub fn lock(paths: &Paths) -> Result<Lock> {
    paths.ensure_home()?;
    let path = paths.home.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Lock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another picobrew process is running (lock: {})", path.display())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("acquiring lock"),
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub fn install(
    paths: &Paths,
    names: &[String],
    opts: &InstallOpts,
    index: &index::Index,
) -> Result<Report> {
    let started = Instant::now();
    if names.is_empty() {
        bail!("no formulae given");
    }
    let tag = bottle::os_tag().ok_or_else(|| anyhow!("unsupported platform"))?;

    super::validate_names(index, names)?;

    let db = Db::open(paths)?;
    let installed_current: HashSet<String> = db
        .snap
        .installed
        .iter()
        .filter(|(name, e)| match index.get(name) {
            Some(f) => e.pkg_version() == f.pkg_version() || compare(&e.version, &f.version).is_ge(),
            None => true,
        })
        .map(|(n, _)| n.clone())
        .collect();

    let resolver = Resolver::new(index);
    // Installed-and-current formulae still count as *roots* when explicitly asked
    // for (so `picobrew install jq` is a no-op message, not an error).
    let skip: HashSet<String> = installed_current.clone();
    let (order, _skipped) = resolver.resolve_parallel(names, &skip, opts.jobs)?;

    let mut plan: Vec<&F> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    for f in order {
        let requested = names.iter().any(|n| n == &f.name);
        let installed = db.get(&f.name);
        let current = installed
            .map(|e| e.version == f.version && e.revision == f.revision)
            .unwrap_or(false);
        if current && !opts.force {
            if requested {
                already.push(f.name.clone());
            }
            continue;
        }
        plan.push(f);
    }

    if plan.is_empty() {
        for a in &already {
            ui::note(&format!("{} {} is already installed and up-to-date", ui::green("✓"), ui::bold(a)));
        }
        if already.is_empty() {
            ui::note("Nothing to do.");
        }
        return Ok(Report {
            installed: Vec::new(),
            skipped: already,
            bytes: 0,
            elapsed: started.elapsed(),
        });
    }

    // Bottle availability check up front: fail before downloading anything.
    let mut jobs: Vec<Job> = Vec::with_capacity(plan.len());
    for f in &plan {
        if f.disabled {
            bail!("{} is disabled in Homebrew", f.name);
        }
        let b = index.bottle(&f.name, &tag)?.ok_or_else(|| {
            anyhow!(
                "{} {} has no bottle for `{tag}` (source builds are not supported yet)",
                f.name,
                f.version
            )
        })?;
        jobs.push(Job {
            name: f.name.clone(),
            version: f.pkg_version(),
            url: b.url,
            sha256: b.sha256,
            cellar: b.cellar,
            archive: paths
                .downloads
                .join(format!("{}--{}.{}.bottle.tar.gz", f.name, f.pkg_version(), tag)),
        });
    }

    let total_names: Vec<String> = plan.iter().map(|f| f.name.clone()).collect();
    if ui::is_json() {
        // --json: emit the plan and stop (machine-readable dry run).
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "plan": total_names,
                "already_installed": already,
                "dry_run": true
            }))?
        );
        return Ok(Report {
            installed: Vec::new(),
            skipped: already,
            bytes: 0,
            elapsed: started.elapsed(),
        });
    }
    ui::step(&format!(
        "{} {} formula{} planned: {}",
        ui::green("→"),
        plan.len(),
        if plan.len() == 1 { "" } else { "e" },
        total_names.join(" ")
    ));
    if opts.dry_run {
        for f in &plan {
            let deps = if f.deps.is_empty() {
                "no deps".to_string()
            } else {
                format!("deps: {}", f.deps.join(", "))
            };
            ui::note(&format!(
                "  {} {} {}",
                f.name,
                ui::cyan(&f.version),
                ui::dim(&format!("({deps})"))
            ));
        }
        ui::note(&ui::dim("dry run: nothing downloaded"));
        return Ok(Report {
            installed: Vec::new(),
            skipped: already,
            bytes: 0,
            elapsed: started.elapsed(),
        });
    }

    // --- downloads (parallel) ------------------------------------------------
    let to_fetch: Vec<usize> = jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| !cached_valid(&j.archive, &j.sha256))
        .map(|(i, _)| i)
        .collect();
    let fetch_bytes = {
        let progress = Progress::new(to_fetch.len());
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, Result<u64>)>> = Mutex::new(Vec::new());
        let workers = opts.jobs.min(to_fetch.len().max(1));
        if !to_fetch.is_empty() {
            ui::step(&format!(
                "Downloading {} bottle{} in parallel ({} job{})",
                to_fetch.len(),
                if to_fetch.len() == 1 { "" } else { "s" },
                workers,
                if workers == 1 { "" } else { "s" }
            ));
        }
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= to_fetch.len() {
                        break;
                    }
                    let job = &jobs[to_fetch[i]];
                    progress.file_start(&job.name);
                    let hook = |n: u64| progress.add_bytes(n);
                    let r = net::download_verified(&job.url, &job.archive, &job.sha256, &hook)
                        .map(|(bytes, _)| bytes)
                        .with_context(|| format!("downloading {} {}", job.name, job.version));
                    progress.file_done();
                    results.lock().unwrap().push((i, r));
                });
            }
        });
        progress.finish();
        let results = results.into_inner().unwrap();
        let mut bytes = 0u64;
        let mut errors = Vec::new();
        for (_, r) in results {
            match r {
                Ok(b) => bytes += b,
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            for e in &errors {
                ui::error(&format!("{e:#}"));
            }
            bail!("{} download(s) failed; nothing was installed", errors.len());
        }
        bytes
    };

    // Linux bottles ask for <prefix>/lib/ld.so; provide it before any binary
    // gets linked, so the very first install is runnable.
    bottle::ensure_linux_loader(&paths.prefix)?;

    // --- extract + link in topological order ---------------------------------
    ui::step(&format!("Installing {} keg{}…", plan.len(), if plan.len() == 1 { "" } else { "s" }));
    let mut db = Db::open(paths)?;
    let job_index: HashMap<&str, &Job> = jobs.iter().map(|j| (j.name.as_str(), j)).collect();
    let mut installed = Vec::with_capacity(plan.len());
    for f in plan.iter().copied() {
        let job = job_index
            .get(f.name.as_str())
            .ok_or_else(|| anyhow!("internal: missing job for {}", f.name))?;
        install_one(paths, f, job, opts.force)?;
        let keg = paths.cellar.join(&f.name).join(f.pkg_version());
        let entry = Entry {
            version: f.version.clone(),
            revision: f.revision,
            rel: paths.rel(&keg),
            installed_at: now(),
            deps: f.deps.clone(),
        };
        db.insert(&f.name, entry);
        db.save()?; // crash-safe: state advances with the Cellar
        installed.push(f.name.clone());
    }

    // --- summary -------------------------------------------------------------
    for f in &plan {
        if let Some(caveats) = index.caveats(&f.name) {
            ui::note("");
            ui::note(&format!("{} {}", ui::yellow("Caveats"), ui::bold(&f.name)));
            for line in caveats.lines() {
                ui::note(&format!("  {line}"));
            }
        }
    }
    let elapsed = started.elapsed();
    let bin_dir = paths.prefix.join("bin");
    if !linker::dir_on_path(&bin_dir) {
        ui::note("");
        ui::note(&format!(
            "{} {} is not on your PATH. Add this to your shell profile:",
            ui::yellow("note:"),
            bin_dir.display()
        ));
        ui::note(&format!("  export PATH=\"{}:$PATH\"", bin_dir.display()));
    }
    ui::note("");
    let transfer = if fetch_bytes == 0 {
        "all bottles from cache".to_string()
    } else {
        format!("{} downloaded", human_bytes(fetch_bytes))
    };
    ui::note(&format!(
        "{} installed {} formula{} in {} · {}{}",
        ui::green("✓"),
        installed.len(),
        if installed.len() == 1 { "" } else { "e" },
        ui::bold(&human_dur(elapsed)),
        transfer,
        if already.is_empty() {
            String::new()
        } else {
            format!(" · {} already current", already.len())
        }
    ));
    Ok(Report {
        installed,
        skipped: already,
        bytes: fetch_bytes,
        elapsed,
    })
}

struct Job {
    name: String,
    version: String,
    url: String,
    sha256: String,
    cellar: String,
    archive: PathBuf,
}

fn cached_valid(archive: &PathBuf, sha256: &str) -> bool {
    if !archive.exists() {
        return false;
    }
    // Cheap check: size match when we know it, else trust the name (digests are
    // verified after every download anyway).
    match std::fs::read(archive) {
        Ok(data) => crate::sha::hex_of(&data) == sha256.to_ascii_lowercase(),
        Err(_) => false,
    }
}

fn install_one(paths: &Paths, f: &F, job: &Job, force: bool) -> Result<()> {
    let tag = bottle::os_tag().unwrap_or_default();
    if !bottle::cellar_compatible(&job.cellar, &paths.prefix) {
        bail!(
            "{} {} bottle is built for Cellar {} and cannot relocate to {}",
            f.name,
            f.version,
            job.cellar,
            paths.prefix.join("Cellar").display()
        );
    }
    let name_dir = paths.cellar.join(&f.name);
    let keg = name_dir.join(&job.version);
    std::fs::create_dir_all(&name_dir)?;

    // Replace-in-place: move any old keg aside first so a failed extraction
    // never leaves a half-written Cellar.
    let staging = name_dir.join(format!(".staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).ok();
    }
    std::fs::create_dir_all(&staging)?;
    let files = bottle::extract(&job.archive, &staging)
        .with_context(|| format!("extracting {} ({} {})", job.archive.display(), f.name, tag))?;
    bottle::keg_is_populated(&staging)?;

    if keg.exists() {
        if !force {
            // Same version re-install: keep the existing keg unless forced.
            std::fs::remove_dir_all(&staging).ok();
            return Ok(());
        }
        std::fs::remove_dir_all(&keg).ok();
    }
    // Pour: swap bottle placeholders for this prefix (ELF lengths preserved).
    let reloc = bottle::relocate(&staging, &paths.prefix)
        .with_context(|| format!("relocating {}", f.name))?;
    if !reloc.unrelocatable.is_empty() {
        ui::warn(&format!(
            "{}: {} file(s) need a shorter prefix to relocate (left untouched): {}",
            f.name,
            reloc.unrelocatable.len(),
            reloc.unrelocatable.join(", ")
        ));
    }
    std::fs::rename(&staging, &keg)
        .with_context(|| format!("moving keg into {}", keg.display()))?;

    // Homebrew bottles ship `.brew/<name>.rb` and an install receipt; make sure
    // a receipt exists so `brew`-side tooling recognises the keg.
    if bottle::install_receipt(&keg).is_none() {
        let receipt = bottle::synth_receipt(&f.name, &f.version);
        let brew_dir = keg.join(".brew");
        std::fs::create_dir_all(&brew_dir).ok();
        let _ = std::fs::write(
            brew_dir.join(format!("{}.INSTALL_RECEIPT.json", f.name)),
            serde_json::to_vec_pretty(&receipt)?,
        );
    }

    let nlinks = linker::link_keg(paths, &f.name, &keg)
        .with_context(|| format!("linking {}", f.name))?;
    if !ui::is_quiet() {
        let bins = linker::keg_binaries(&keg);
        let what = if bins.is_empty() {
            format!("{files} files")
        } else {
            format!("{files} files, {} binaries ({})", bins.len(), bins.join(", "))
        };
        let reloc_note = if reloc.files_patched > 0 {
            format!(
                " · {} relocated ({} refs)",
                reloc.files_patched, reloc.occurrences
            )
        } else {
            String::new()
        };
        ui::note(&format!(
            "  {} {} {} {}",
            ui::green("✓"),
            ui::bold(&f.name),
            f.version,
            ui::dim(&format!("· {what} · {nlinks} links{reloc_note}"))
        ));
    }
    Ok(())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

pub fn uninstall(paths: &Paths, names: &[String], force: bool, index: &index::Index) -> Result<()> {
    let mut db = Db::open(paths)?;
    let mut errors = 0usize;
    for name in names {
        let Some(entry) = db.get(name).cloned() else {
            ui::warn(&format!("{name} is not installed"));
            continue;
        };
        // Refuse when another installed formula depends on this one.
        let dependents: Vec<String> = db
            .snap
            .installed
            .keys()
            .filter(|other| *other != name)
            .filter(|other| {
                index
                    .get(other)
                    .map(|f| f.deps.iter().any(|d| d == name))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !dependents.is_empty() && !force {
            ui::error(&format!(
                "cannot uninstall {name}: required by {} (use --force)",
                dependents.join(", ")
            ));
            errors += 1;
            continue;
        }
        let keg = entry.abs(paths);
        linker::unlink_keg(paths, name, &keg)?;
        let name_dir = paths.cellar.join(name);
        if name_dir.exists() {
            std::fs::remove_dir_all(&name_dir)
                .with_context(|| format!("removing {}", name_dir.display()))?;
        }
        db.remove(name);
        db.save()?;
        ui::note(&format!(
            "{} uninstalled {} {}",
            ui::green("✓"),
            ui::bold(name),
            entry.pkg_version()
        ));
    }
    if errors > 0 {
        bail!("{errors} formula(e) not removed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// upgrade
// ---------------------------------------------------------------------------

pub fn outdated_list(db: &Db, index: &index::Index) -> Vec<(F, String)> {
    let mut out = Vec::new();
    for (name, entry) in &db.snap.installed {
        if let Some(f) = index.get(name) {
            if compare(&entry.version, &f.version) == std::cmp::Ordering::Less {
                out.push((f.clone(), entry.pkg_version()));
            }
        }
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    out
}

pub fn upgrade(
    paths: &Paths,
    names: &[String],
    opts: &InstallOpts,
    index: &index::Index,
) -> Result<Report> {
    let db = Db::open(paths)?;
    let outdated = outdated_list(&db, index);
    if outdated.is_empty() {
        ui::note("All installed formulae are up-to-date.");
        return Ok(Report {
            installed: Vec::new(),
            skipped: Vec::new(),
            bytes: 0,
            elapsed: Duration::ZERO,
        });
    }
    let targets: Vec<String> = if names.is_empty() {
        outdated.iter().map(|(f, _)| f.name.clone()).collect()
    } else {
        for n in names {
            if !outdated.iter().any(|(f, _)| &f.name == n) {
                ui::warn(&format!("{n} is not outdated (or not installed)"));
            }
        }
        names.to_vec()
    };
    if targets.is_empty() {
        return Ok(Report {
            installed: Vec::new(),
            skipped: Vec::new(),
            bytes: 0,
            elapsed: Duration::ZERO,
        });
    }
    for (f, old) in &outdated {
        if targets.iter().any(|t| t == &f.name) {
            ui::note(&format!(
                "  {} {} → {}",
                f.name,
                ui::dim(old),
                ui::bold(&f.version)
            ));
        }
    }
    // Old kegs are replaced by install_one's staging dance; force is implied.
    let mut o = opts.clone();
    o.force = true;
    install(paths, &targets, &o, index)
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

pub fn doctor(paths: &Paths) -> Result<()> {
    let started = Instant::now();
    ui::step("picobrew doctor");
    let mut problems = 0;

    // 1. prefix + cellar
    check(
        paths.prefix.is_dir(),
        &format!("prefix {} exists", paths.prefix.display()),
        &format!("prefix {} missing", paths.prefix.display()),
        &mut problems,
    );
    check(
        paths.cellar.is_dir(),
        &format!("Cellar {} ready", paths.cellar.display()),
        &format!("Cellar {} missing (run any install to create it)", paths.cellar.display()),
        &mut problems,
    );

    // 2. state vs Cellar
    let db = Db::open(paths)?;
    let scanned = crate::db::scan_cellar(paths)?;
    let drift: Vec<String> = db
        .snap
        .installed
        .keys()
        .filter(|n| !scanned.installed.contains_key(*n))
        .cloned()
        .collect();
    check(
        drift.is_empty(),
        &format!("{} installed formula(e) tracked", db.snap.installed.len()),
        &format!("state lists missing kegs: {}", drift.join(", ")),
        &mut problems,
    );

    // 3. dangling symlinks in prefix dirs
    let mut dangling = Vec::new();
    for d in ["bin", "lib", "share"] {
        let dir = paths.prefix.join(d);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_symlink() && !p.exists() {
                    dangling.push(p.display().to_string());
                }
            }
        }
    }
    if dangling.is_empty() {
        ui::note(&format!("{} no dangling symlinks", ui::green("✓")));
    } else {
        problems += 1;
        ui::note(&format!(
            "{} {} dangling symlink(s): {}",
            ui::yellow("!"),
            dangling.len(),
            dangling.join(", ")
        ));
    }

    // 4. cache health
    match std::fs::metadata(paths.formulas_path()) {
        Ok(m) => ui::note(&format!(
            "{} index cache: {} ({})",
            ui::green("✓"),
            human_bytes(m.len()),
            m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|a| format!("{} old", human_dur(a)))
                .unwrap_or_else(|| "age unknown".into())
        )),
        Err(_) => {
            problems += 1;
            ui::note(&format!("{} no index cache yet (first run downloads it)", ui::yellow("!")))
        }
    }
    let downloads = std::fs::read_dir(&paths.downloads)
        .map(|rd| rd.flatten().map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum::<u64>())
        .unwrap_or(0);
    ui::note(&format!(
        "  download cache: {}",
        human_bytes(downloads)
    ));

    // 5. platform + tag
    ui::note(&format!(
        "  platform: {} / {} → bottle tag `{}`",
        std::env::consts::OS,
        std::env::consts::ARCH,
        bottle::os_tag().unwrap_or_else(|| "none".into())
    ));

    if problems == 0 {
        ui::note(&format!(
            "{} all checks passed ({})",
            ui::green("✓"),
            human_dur(started.elapsed())
        ));
        Ok(())
    } else {
        bail!("{problems} problem(s) found")
    }
}

fn check(ok: bool, good: &str, bad: &str, problems: &mut usize) {
    if ok {
        ui::note(&format!("{} {good}", ui::green("✓")));
    } else {
        *problems += 1;
        ui::note(&format!("{} {bad}", ui::yellow("!")));
    }
}
