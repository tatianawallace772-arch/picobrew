//! Rendering for `list`, `info`, `search` and `outdated`.

use crate::db::Db;
use crate::index::{F, Index};
use crate::search::Hit;
use crate::ui;

pub fn print_search(hits: &[Hit<'_>]) {
    if hits.is_empty() {
        ui::note("No formula found. Try a broader query.");
        return;
    }
    let name_w = hits
        .iter()
        .map(|h| h.formula.name.len() + h.via_alias.as_ref().map(|a| a.len() + 4).unwrap_or(0))
        .max()
        .unwrap_or(10)
        .min(40);
    for h in hits {
        let f = h.formula;
        let mut label = f.name.clone();
        if let Some(a) = &h.via_alias {
            label.push_str(&ui::dim(&format!(" ({a})")));
        }
        let mark = if f.disabled {
            ui::red(" ✗ disabled")
        } else if f.deprecated {
            ui::yellow(" ⚠ deprecated")
        } else {
            String::new()
        };
        ui::note(&format!(
            "{:<name_w$}  {} {} {}{}",
            ui::bold(&label),
            ui::cyan(&f.version),
            truncate(&f.desc, 72),
            ui::dim(&format!("[{}]", if f.keg_only { "keg-only" } else { "formula" })),
            mark,
            name_w = name_w + 9
        ));
    }
    ui::note(&format!(
        "\n{} result{}",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    ));
}

pub fn print_list(db: &Db, index: &Index, show_versions: bool) {
    if db.snap.installed.is_empty() {
        ui::note("No formulae installed. Try `picobrew install jq`.");
        return;
    }
    for (name, entry) in &db.snap.installed {
        if show_versions {
            let latest = index
                .get(name)
                .map(|f| f.pkg_version())
                .unwrap_or_default();
            let marker = if index.get(name).map(|f| db.outdated(f)).unwrap_or(false) {
                ui::yellow(&format!(" ⬆ {latest}"))
            } else {
                String::new()
            };
            ui::note(&format!("{} {}{}", name, ui::cyan(&entry.pkg_version()), marker));
        } else {
            ui::note(name);
        }
    }
}

pub fn print_info(
    f: &F,
    installed: Option<&crate::db::Entry>,
    tag: Option<&str>,
    bottle: Option<&crate::index::BottleFile>,
    aliases: &[&str],
    caveats: Option<&str>,
) {
    ui::note(&format!("{} {}", ui::bold(&f.name), ui::cyan(&f.version)));
    if !aliases.is_empty() {
        ui::note(&format!("  {} {}", ui::dim("aliases:"), aliases.join(", ")));
    }
    if !f.desc.is_empty() {
        ui::note(&format!("  {}", f.desc));
    }
    if let Some(t) = tag {
        match bottle {
            Some(b) => ui::note(&format!(
                "  {} available ({t}, {}…)",
                ui::dim("bottle:"),
                &b.sha256[..b.sha256.len().min(12)]
            )),
            None => ui::note(&format!(
                "  {} {}",
                ui::dim("bottle:"),
                ui::yellow(&format!("none for {t}"))
            )),
        }
    }
    if !f.deps.is_empty() {
        ui::note(&format!("  {} {}", ui::dim("deps:"), f.deps.join(", ")));
    }
    if !f.build_deps.is_empty() {
        ui::note(&format!(
            "  {} {}",
            ui::dim("build deps:"),
            f.build_deps.join(", ")
        ));
    }
    if !f.optional_deps.is_empty() {
        ui::note(&format!(
            "  {} {}",
            ui::dim("optional:"),
            f.optional_deps.join(", ")
        ));
    }
    match installed {
        Some(e) => {
            let up = !bottle_up_to_date(e, f);
            ui::note(&format!(
                "  {} {}{}",
                ui::dim("installed:"),
                e.pkg_version(),
                if up { ui::yellow(" (outdated)") } else { String::new() }
            ));
        }
        None => ui::note(&format!("  {} no", ui::dim("installed:"))),
    }
    if f.keg_only {
        ui::note(&format!("  {}", ui::yellow("keg-only: not linked into the prefix")));
    }
    if f.deprecated {
        ui::note(&format!("  {}", ui::red("deprecated by Homebrew")));
    }
    if f.disabled {
        ui::note(&format!("  {}", ui::red("disabled by Homebrew")));
    }
    if let Some(c) = caveats {
        ui::note("  caveats:");
        for line in c.lines() {
            ui::note(&format!("    {line}"));
        }
    }
}

fn bottle_up_to_date(entry: &crate::db::Entry, f: &F) -> bool {
    crate::version::compare(&entry.version, &f.version) != std::cmp::Ordering::Greater
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
