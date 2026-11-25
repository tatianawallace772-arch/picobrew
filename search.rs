//! Fast fuzzy search over the compact index.
//!
//! Scoring is deliberately linear and cheap: ~9k formulas are scanned in
//! well under a millisecond, and prefix/exact hits get boosted so `picobrew
//! search jq` puts `jq` first. Case-insensitive by default.

use crate::index::{F, Index};

pub struct Hit<'a> {
    pub formula: &'a F,
    pub score: i64,
    /// True when the match came via an alias (`python3` → `python@3.13`).
    pub via_alias: Option<String>,
}

pub fn search<'a>(index: &'a Index, query: &str, limit: usize) -> Vec<Hit<'a>> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    // Build alias buckets once: canonical name → aliases.
    let mut alias_buckets: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for (a, target) in index.aliases.iter().chain(index.oldnames.iter()) {
        alias_buckets
            .entry(target.as_str())
            .or_default()
            .push(a.as_str());
    }

    let mut hits: Vec<Hit<'a>> = Vec::new();
    for f in &index.formulas {
        let name = f.name.to_ascii_lowercase();
        let mut score: i64 = 0;
        let mut via_alias = None;

        if name == q {
            score = 1000;
        } else if name.starts_with(&q) {
            score = 700 - (name.len() as i64 - q.len() as i64).min(60);
        } else if let Some(pos) = name.find(&q) {
            score = 400 - pos.min(80) as i64;
        } else if let Some(aliases) = alias_buckets.get(f.name.as_str()) {
            for a in aliases {
                let al = a.to_ascii_lowercase();
                if al == q {
                    score = 900;
                    via_alias = Some(a.to_string());
                    break;
                } else if al.starts_with(&q) {
                    score = score.max(600);
                    via_alias = Some(a.to_string());
                }
            }
        }

        // Description match (weaker than any name match).
        if score == 0 {
            let desc = f.desc.to_ascii_lowercase();
            if desc.contains(&q) {
                score = 120;
            }
        }

        // Whole-word / exact token boost inside the name: `python@3` finds `python@3.13`.
        if score > 0 {
            if f.deprecated {
                score -= 40;
            }
            if f.disabled {
                score -= 60;
            }
            hits.push(Hit {
                formula: f,
                score,
                via_alias,
            });
        }
    }

    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.formula.name.cmp(&b.formula.name)));
    hits.truncate(limit);
    hits
}

/// Prefix expansion for commands that take a package name: if `name` is not an
/// exact formula but is an unambiguous prefix, expand it (like brew does).
pub fn expand_prefix<'a>(index: &'a Index, name: &str) -> Option<&'a F> {
    if let Some(f) = index.get(name) {
        return Some(f);
    }
    let lower = name.to_ascii_lowercase();
    let mut matches = index
        .formulas
        .iter()
        .filter(|f| f.name.to_ascii_lowercase().starts_with(&lower));
    let first = matches.next()?;
    if matches.next().is_some() {
        // Ambiguous. Exact alias/old-name resolution may still settle it.
        return index.get(name);
    }
    Some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(json: &str) -> Index {
        Index::parse(json, "t", 1).unwrap()
    }

    const CORPUS: &str = r#"[
      {"name":"jq","desc":"Lightweight and flexible command-line JSON processor","versions":{"stable":"1.7.1"}},
      {"name":"jqp","desc":"TUI playground to experiment with jq","versions":{"stable":"1.1.0"}},
      {"name":"gron","desc":"Make JSON greppable","versions":{"stable":"0.7.1"}},
      {"name":"python@3.13","desc":"Interpreted, interactive, object-oriented programming language","aliases":["python3","python"],"versions":{"stable":"3.13.4"}},
      {"name":"rust","desc":"Safe, concurrent, practical language","versions":{"stable":"1.90.0"}},
      {"name":"oldjq","desc":"legacy jq","versions":{"stable":"0.1"},"deprecated":true}
    ]"#;

    #[test]
    fn exact_name_first() {
        let i = idx(CORPUS);
        let hits = search(&i, "jq", 10);
        assert_eq!(hits[0].formula.name, "jq");
        assert_eq!(hits[0].score, 1000);
    }

    #[test]
    fn prefix_beats_substring() {
        let i = idx(CORPUS);
        let hits = search(&i, "jq", 10);
        let names: Vec<&str> = hits.iter().map(|h| h.formula.name.as_str()).collect();
        assert!(names.iter().position(|n| *n == "jqp") < names.iter().position(|n| *n == "oldjq"));
    }

    #[test]
    fn desc_matches_are_weaker() {
        let i = idx(CORPUS);
        let hits = search(&i, "json", 10);
        assert!(hits.iter().any(|h| h.formula.name == "gron"));
        assert!(matches!(hits[0].formula.name.as_str(), "jq" | "jqp" | "gron"));
    }

    #[test]
    fn alias_matches_rank_high() {
        let i = idx(CORPUS);
        // Name-prefix match wins and does not need the alias.
        let hits = search(&i, "python", 10);
        assert_eq!(hits[0].formula.name, "python@3.13");
        assert_eq!(hits[0].via_alias, None);
        // Alias-exact match for a name that does not start with the query.
        let hits = search(&i, "python3", 10);
        assert_eq!(hits[0].formula.name, "python@3.13");
        assert_eq!(hits[0].via_alias.as_deref(), Some("python3"));
    }

    #[test]
    fn deprecated_ranks_below() {
        let i = idx(CORPUS);
        let hits = search(&i, "jq", 10);
        let dep = hits.iter().find(|h| h.formula.name == "oldjq").unwrap();
        let live = hits.iter().find(|h| h.formula.name == "jq").unwrap();
        assert!(dep.score < live.score);
    }

    #[test]
    fn prefix_expansion() {
        let i = idx(CORPUS);
        assert_eq!(expand_prefix(&i, "gron").unwrap().name, "gron");
        assert!(expand_prefix(&i, "j").is_none()); // ambiguous → None
        assert_eq!(expand_prefix(&i, "python3").unwrap().name, "python@3.13");
    }
}
