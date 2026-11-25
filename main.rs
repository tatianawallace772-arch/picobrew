//! picobrew — a pico-fast package manager for Homebrew formulae.
//!
//! Design goals, in order:
//!   1. Never parse 35 MB of JSON twice (compact index + local cache).
//!   2. Never serialise work that can be parallel (downloads, graph walks).
//!   3. Stay compatible: same Cellar layout, same bottles, same receipts, so
//!      picobrew and brew can share a prefix.

mod archive;
mod bench;
mod bottle;
mod db;
mod deps;
mod display;
mod index;
mod install;
mod linker;
mod net;
mod paths;
mod search;
mod sha;
mod ui;
mod version;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use paths::Paths;
use std::path::PathBuf;
use std::process::ExitCode;

/// Bump when the on-disk cache/state layout changes.
pub const INDEX_SCHEMA: u32 = 2; // v2: streamed formula records + bottle store

#[derive(Parser, Debug)]
#[command(
    name = "picobrew",
    version,
    about = "A pico-fast package manager for Homebrew formulae",
    long_about = "picobrew installs Homebrew bottles without Homebrew's Ruby stack.\n\
                  A compact local index, parallel verified downloads, and a Rust\n\
                  hot path make it 10–100× faster than brew for everyday commands."
)]
struct Cli {
    /// Install prefix (default: the platform Homebrew prefix, or $PICOBREW_PREFIX)
    #[arg(long, global = true, env = "PICOBREW_PREFIX", value_name = "DIR")]
    prefix: Option<PathBuf>,

    /// Suppress progress output
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Machine-readable JSON output where supported
    #[arg(long, global = true)]
    json: bool,

    /// Ignore the index cache and refetch from formulae.brew.sh
    #[arg(long, global = true)]
    refresh: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Install one or more formulae (with dependencies)
    #[command(visible_aliases = ["add", "i"])]
    Install {
        #[arg(required = true, value_name = "FORMULA")]
        names: Vec<String>,
        /// Parallel download jobs
        #[arg(short = 'j', long, value_name = "N")]
        jobs: Option<usize>,
        /// Show what would happen without touching the disk
        #[arg(long)]
        dry_run: bool,
        /// Reinstall / relink even when already current
        #[arg(short, long)]
        force: bool,
    },

    /// Uninstall one or more formulae
    #[command(visible_aliases = ["remove", "rm", "unlink"])]
    Uninstall {
        #[arg(required = true, value_name = "FORMULA")]
        names: Vec<String>,
        /// Remove even if other installed formulae depend on it
        #[arg(short, long)]
        force: bool,
    },

    /// Upgrade installed formulae (all, or those named)
    #[command(visible_aliases = ["update"])]
    Upgrade {
        #[arg(value_name = "FORMULA")]
        names: Vec<String>,
        /// Parallel download jobs
        #[arg(short = 'j', long, value_name = "N")]
        jobs: Option<usize>,
        #[arg(long)]
        dry_run: bool,
    },

    /// Show installed formulae
    #[command(visible_alias = "ls")]
    List {
        /// Show versions (and flag outdated ones)
        #[arg(short, long)]
        versions: bool,
    },

    /// Show formulae with newer versions available
    Outdated,

    /// Search formulae by name and description
    Search {
        query: String,
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },

    /// Show details about formulae
    #[command(visible_alias = "show")]
    Info {
        #[arg(required = true, value_name = "FORMULA")]
        names: Vec<String>,
        /// Print the raw compact index entry as JSON
        #[arg(long)]
        raw: bool,
    },

    /// Measure picobrew's hot paths against a live Homebrew reference
    Bench {
        /// Skip the (large) reference download
        #[arg(long)]
        no_brew_ref: bool,
    },

    /// Check prefix, Cellar, state, links and caches
    Doctor,

    /// Show or refresh the local index cache
    Index {
        /// Force a refetch
        #[arg(long)]
        refresh: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    ui::init(cli.quiet, cli.json);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            if ui::is_json() {
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": format!("{e:#}") })
                );
            } else {
                ui::error(&format!("{e:#}"));
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let paths = Paths::resolve(cli.prefix.clone()).context("resolving prefix")?;

    // Commands that read the index need it; the rest should stay fast.
    let needs_index = matches!(
        cli.command,
        Command::Install { .. }
            | Command::Uninstall { .. }
            | Command::Upgrade { .. }
            | Command::Outdated
            | Command::Search { .. }
            | Command::Info { .. }
            | Command::Index { .. }
            | Command::Bench { .. }
    );
    let index = if needs_index {
        let max_age = if cli.refresh { Some(std::time::Duration::ZERO) } else { None };
        Some(index::load(&paths, max_age)?)
    } else {
        None
    };

    match cli.command {
        Command::Install {
            names,
            jobs,
            dry_run,
            force,
        } => {
            let _lock = install::lock(&paths)?;
            paths.ensure_install_dirs()?;
            let loaded = index.as_ref().unwrap();
            if !cli.json && !cli.quiet {
                report_index_source(loaded);
            }
            let opts = install::InstallOpts {
                jobs: jobs.unwrap_or_else(|| install::InstallOpts::default().jobs),
                dry_run,
                force,
            };
            let report = install::install(&paths, &names, &opts, &loaded.index)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "installed": report.installed,
                        "skipped": report.skipped,
                        "bytes": report.bytes,
                        "elapsed_ms": report.elapsed.as_millis() as u64,
                    })
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Uninstall { names, force } => {
            let _lock = install::lock(&paths)?;
            install::uninstall(&paths, &names, force, &index.as_ref().unwrap().index)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Upgrade {
            names,
            jobs,
            dry_run,
        } => {
            let _lock = install::lock(&paths)?;
            let opts = install::InstallOpts {
                jobs: jobs.unwrap_or_else(|| install::InstallOpts::default().jobs),
                dry_run,
                force: false,
            };
            let report = install::upgrade(&paths, &names, &opts, &index.as_ref().unwrap().index)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "upgraded": report.installed,
                        "bytes": report.bytes,
                        "elapsed_ms": report.elapsed.as_millis() as u64,
                    })
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::List { versions } => {
            let db = db::Db::open(&paths)?;
            if cli.json {
                let items: Vec<serde_json::Value> = db
                    .snap
                    .installed
                    .iter()
                    .map(|(n, e)| {
                        serde_json::json!({
                            "name": n,
                            "version": e.pkg_version(),
                            "outdated": index.as_ref().map(|l| l.index.get(n).map(|f| f.version != e.version).unwrap_or(false)).unwrap_or(false)
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if let Some(loaded) = &index {
                display::print_list(&db, &loaded.index, versions);
            } else {
                for (name, entry) in &db.snap.installed {
                    if versions {
                        println!("{name} {}", entry.pkg_version());
                    } else {
                        println!("{name}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Outdated => {
            let db = db::Db::open(&paths)?;
            let loaded = index.as_ref().unwrap();
            let out = install::outdated_list(&db, &loaded.index);
            if cli.json {
                let items: Vec<serde_json::Value> = out
                    .iter()
                    .map(|(f, old)| {
                        serde_json::json!({"name": f.name, "installed": old, "latest": f.version})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if out.is_empty() {
                ui::note("All installed formulae are up-to-date.");
            } else {
                for (f, old) in out {
                    ui::note(&format!("{} {} → {}", f.name, ui::dim(&old), ui::bold(&f.version)));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Search { query, limit } => {
            let loaded = index.as_ref().unwrap();
            let hits = search::search(&loaded.index, &query, limit);
            if cli.json {
                let items: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "name": h.formula.name,
                            "version": h.formula.version,
                            "desc": h.formula.desc,
                            "score": h.score,
                            "via_alias": h.via_alias,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                if !cli.quiet {
                    eprintln!(
                        "{} searched {} formulae in {}",
                        ui::dim("·"),
                        loaded.index.len(),
                        ui::dim(&ui::human_dur(loaded.latency))
                    );
                }
                display::print_search(&hits);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Info { names, raw } => {
            let loaded = index.as_ref().unwrap();
            let db = db::Db::open(&paths)?;
            let mut missing = Vec::new();
            for name in &names {
                let f = search::expand_prefix(&loaded.index, name)
                    .or_else(|| loaded.index.get(name));
                match f {
                    Some(f) => {
                        if raw || cli.json {
                            println!("{}", serde_json::to_string_pretty(f)?);
                        } else {
                            if !cli.quiet && &f.name != name {
                                ui::note(&ui::dim(&format!("({name} → {})", f.name)));
                            }
                            let aliases = loaded.index.aliases_of(&f.name);
                            let tag = bottle::os_tag();
                            let bottle = match &tag {
                                Some(t) => loaded.index.bottle(&f.name, t).ok().flatten(),
                                None => None,
                            };
                            let caveats = loaded.index.caveats(&f.name);
                            display::print_info(
                                f,
                                db.get(&f.name),
                                tag.as_deref(),
                                bottle.as_ref(),
                                &aliases,
                                caveats.as_deref(),
                            );
                            if names.len() > 1 {
                                ui::note("");
                            }
                        }
                    }
                    None => missing.push(name.clone()),
                }
            }
            if !missing.is_empty() {
                bail!("no formula named `{}`", missing.join("`, `"));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Bench { no_brew_ref } => {
            bench::run(&paths, cli.json, !no_brew_ref)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor => {
            paths.ensure_home()?;
            install::doctor(&paths)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Index { refresh } => {
            let refresh = refresh || cli.refresh;
            if refresh {
                let fetched = index::fetch_remote(index::DEFAULT_INDEX_URL, &paths)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": true,
                            "formulas": fetched.index.len(),
                            "source_bytes": fetched.source_bytes,
                            "compact_bytes": fetched.compact_bytes,
                            "latency_ms": fetched.latency.as_millis() as u64,
                        })
                    );
                } else {
                    ui::ok(&format!(
                        "index refreshed: {} formulas ({} → {} in {})",
                        fetched.index.len(),
                        ui::human_bytes(fetched.source_bytes as u64),
                        ui::human_bytes(fetched.compact_bytes as u64),
                        ui::human_dur(fetched.latency)
                    ));
                }
            } else {
                let loaded = index.as_ref().unwrap();
                let hot = std::fs::metadata(paths.formulas_path())
                    .map(|m| m.len())
                    .unwrap_or(0)
                    + std::fs::metadata(paths.aliases_path()).map(|m| m.len()).unwrap_or(0);
                let meta = if hot > 0 { Some(hot) } else { None };
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "formulas": loaded.index.len(),
                            "from": loaded.from,
                            "cache_bytes": meta,
                            "generated_at": loaded.index.generated_at,
                        })
                    );
                } else {
                    ui::note(&format!(
                        "{} formulas · source: {} · hot cache: {} · loaded in {}",
                        loaded.index.len(),
                        loaded.from,
                        meta.map(ui::human_bytes).unwrap_or_else(|| "none".into()),
                        ui::human_dur(loaded.latency)
                    ));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn report_index_source(loaded: &index::Loaded) {
    ui::step(&format!(
        "Index: {} formulas from {} in {}",
        loaded.index.len(),
        loaded.from,
        ui::human_dur(loaded.latency)
    ));
}

/// Expand prefixes and validate every requested name up front so a typo fails
/// before anything is downloaded.
fn validate_names(index: &index::Index, names: &[String]) -> Result<()> {
    let mut unknown = Vec::new();
    for n in names {
        if index.get(n).is_none() && search::expand_prefix(index, n).is_none() {
            unknown.push(n.clone());
        }
    }
    if !unknown.is_empty() {
        bail!(
            "no formula named `{}` — try `picobrew search {}`",
            unknown.join("`, `"),
            unknown[0]
        );
    }
    Ok(())
}
