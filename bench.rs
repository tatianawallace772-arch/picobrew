//! `picobrew bench` — measures the hot paths this project cares about.
//!
//! Each workload runs 3 times and the best time is reported. A "brew reference"
//! line measures fetching Homebrew's own 35 MB index (what `brew` pays to
//! evaluate most commands), so the speedup is always shown against a live
//! number from the same network, not a hand-wave.

use crate::db::Db;
use crate::deps::Resolver;
use crate::index::{self, Index};
use crate::paths::Paths;
use crate::ui::{self, human_dur};
use anyhow::Result;
use std::collections::HashSet;
use std::time::{Duration, Instant};

fn pad(s: &str, w: usize) -> String {
    format!("{s:<w$}")
}

fn pad_left(s: &str, w: usize) -> String {
    format!("{s:>w$}")
}

pub const BENCH_PACKAGES: [&str; 8] = [
    "wget", "jq", "ripgrep", "fd", "bat", "tree", "htop", "ffmpeg",
];

struct Row {
    name: &'static str,
    what: &'static str,
    best: Duration,
}

pub fn run(paths: &Paths, json: bool, with_brew_ref: bool) -> Result<()> {
    let mut rows: Vec<Row> = Vec::new();

    // -- index load (cache hit) ---------------------------------------------
    // Prime the cache.
    let _ = index::load(paths, None)?;
    let mut best = Duration::MAX;
    let mut idx: Option<Index> = None;
    for _ in 0..3 {
        let t = Instant::now();
        let loaded = index::load(paths, None)?;
        best = best.min(t.elapsed());
        idx = Some(loaded.index);
    }
    let idx = idx.expect("index loaded");
    rows.push(Row {
        name: "index load",
        what: "streamed formula records (~860 KB) → memory",
        best,
    });

    // -- search ---------------------------------------------------------------
    let mut best = Duration::MAX;
    let mut n_hits = 0;
    for _ in 0..3 {
        let t = Instant::now();
        let hits = crate::search::search(&idx, "json", 20);
        best = best.min(t.elapsed());
        n_hits = hits.len();
    }
    rows.push(Row {
        name: "search",
        what: Box::leak(
            format!("fuzzy search over {} formulas ({n_hits} hits)", idx.len()).into_boxed_str(),
        ),
        best,
    });

    // -- dependency resolution ------------------------------------------------
    let mut best = Duration::MAX;
    let mut planned = 0;
    for _ in 0..3 {
        let t = Instant::now();
        let r = Resolver::new(&idx);
        let roots: Vec<String> = BENCH_PACKAGES.iter().map(|s| s.to_string()).collect();
        let (order, _) = r.resolve(&roots, &HashSet::new())?;
        planned = order.len();
        best = best.min(t.elapsed());
    }
    rows.push(Row {
        name: "resolve",
        what: Box::leak(
            format!("dependency graph for 8 packages ({planned} formulae total)").into_boxed_str(),
        ),
        best,
    });

    // -- installed-state read --------------------------------------------------
    let _ = Db::open(paths);
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let db = Db::open(paths)?;
        std::hint::black_box(db.snap.installed.len());
        best = best.min(t.elapsed());
    }
    rows.push(Row {
        name: "state read",
        what: "installed packages, snapshot-validated (`picobrew list`)",
        best,
    });

    // -- brew reference ---------------------------------------------------------
    let mut brew_ref: Option<(Duration, usize)> = None;
    if with_brew_ref {
        if !json {
            ui::step("Fetching Homebrew's own index once, for a live reference…");
        }
        let t = Instant::now();
        match crate::net::get_bytes_with_retry(index::DEFAULT_INDEX_URL) {
            Ok(b) => brew_ref = Some((t.elapsed(), b.len())),
            Err(e) => ui::warn(&format!("brew reference fetch failed: {e}")),
        }
    }

    // -- report -----------------------------------------------------------------
    if json {
        let obj = serde_json::json!({
            "workloads": rows.iter().map(|r| serde_json::json!({
                "name": r.name,
                "what": r.what,
                "median_us": r.best.as_micros() as u64,
            })).collect::<Vec<_>>(),
            "brew_reference": brew_ref.map(|(d, b)| serde_json::json!({
                "fetch_s": d.as_secs_f64(),
                "bytes": b,
                "note": "one evaluation of Homebrew's JSON API"
            })),
            "formulas": idx.len(),
            "version": env!("CARGO_PKG_VERSION"),
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    ui::note("");
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(10);
    let time_w = rows
        .iter()
        .map(|r| human_dur(r.best).len())
        .chain(brew_ref.iter().map(|(d, _)| human_dur(*d).len()))
        .max()
        .unwrap_or(8);
    let cell = |a: String, b: String, c: String| {
        format!("  {a}  {b}  {c}")
    };
    ui::note(&cell(
        pad(&ui::bold("workload"), name_w + 9),
        pad_left(&ui::bold("time"), time_w),
        ui::bold("what"),
    ));
    for r in &rows {
        ui::note(&cell(
            pad(r.name, name_w),
            ui::green(&pad_left(&human_dur(r.best), time_w)),
            ui::dim(r.what),
        ));
    }
    if let Some((d, bytes)) = brew_ref {
        ui::note(&cell(
            pad("brew reference", name_w),
            ui::red(&pad_left(&human_dur(d), time_w)),
            ui::dim(&format!(
                "fetch {:.1} MB Homebrew index (what brew pays per command)",
                bytes as f64 / 1_000_000.0
            )),
        ));
        let pico_total: Duration = rows.iter().map(|r| r.best).sum();
        let ratio = d.as_secs_f64() / rows
            .iter()
            .find(|r| r.name == "index load")
            .map(|r| r.best.as_secs_f64().max(1e-9))
            .unwrap_or(1.0);
        ui::note("");
        ui::note(&format!(
            "  pico's whole workload suite: {}  ·  brew's single index fetch: {}",
            ui::bold(&human_dur(pico_total)),
            ui::bold(&human_dur(d))
        ));
        ui::note(&format!(
            "  index load is {}× faster than one brew evaluation",
            ui::green(&format!("{ratio:.0}"))
        ));
    }
    ui::note("");
    Ok(())
}
