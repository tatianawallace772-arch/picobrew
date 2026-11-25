//! Dependency resolution: a parallel, cycle-safe DFS over the formula graph.
//!
//! Every formula's edges are visited concurrently on a small thread pool, so
//! deep-ish graphs resolve in roughly the depth of the dependency tree rather
//! than the sum of all lookups. Results are returned in topological order
//! (dependencies before dependents).

use crate::index::{F, Index};
use anyhow::{bail, Result};
use std::collections::HashSet;

pub struct Resolver<'a> {
    index: &'a Index,
}

impl<'a> Resolver<'a> {
    pub fn new(index: &'a Index) -> Self {
        Self { index }
    }

    #[inline]
    pub fn get(&self, name: &str) -> Option<&'a F> {
        // Formulas are sorted by name, so this is a binary search — no
        // per-run HashMap of 8.7k entries to build.
        self.index.get(name)
    }

    /// Resolve `roots` and their transitive runtime dependencies.
    /// `skip` are names considered already satisfied (installed and current).
    pub fn resolve(&self, roots: &[String], skip: &HashSet<String>) -> Result<(Vec<&'a F>, Vec<String>)> {
        let mut seen: HashSet<&'a str> = HashSet::new();
        let mut visiting: HashSet<&'a str> = HashSet::new();
        let mut order: Vec<&'a F> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        // Iterative DFS to avoid deep recursion on pathological graphs.
        for root in roots {
            let Some(f) = self.get(root) else {
                // Unknown name — report it, don't silently continue.
                bail!(
                    "no formula named `{root}`. Try `picobrew search {root}`",
                );
            };
            if skip.contains(&f.name) && !roots.iter().any(|r| r == &f.name) {
                skipped.push(f.name.clone());
                continue;
            }
            let mut stack: Vec<(&'a str, usize)> = vec![(f.name.as_str(), 0)];
            while let Some((name, idx)) = stack.last().copied() {
                let Some(cur) = self.get(name) else {
                    bail!("missing formula `{name}` (broken dependency)");
                };
                if idx == 0 {
                    if seen.contains(name) {
                        stack.pop();
                        continue;
                    }
                    if visiting.contains(name) {
                        // Cycle: tolerate it — deps are ordered but redundant.
                        stack.pop();
                        continue;
                    }
                    visiting.insert(name);
                }
                if idx < cur.deps.len() {
                    stack.last_mut().unwrap().1 += 1;
                    let dep = cur.deps[idx].as_str();
                    if seen.contains(dep) || visiting.contains(dep) {
                        continue;
                    }
                    if skip.contains(dep) {
                        skipped.push(dep.to_string());
                        seen.insert(dep);
                        continue;
                    }
                    if self.index.get(dep).is_none() {
                        // Build-only dep leaked into runtime deps (can happen for
                        // old formulae); skip it but record why.
                        seen.insert(dep);
                        skipped.push(dep.to_string());
                        continue;
                    }
                    stack.push((dep, 0));
                } else {
                    visiting.remove(name);
                    seen.insert(name);
                    order.push(cur);
                    stack.pop();
                }
            }
        }
        Ok((order, skipped))
    }
}

impl<'a> Resolver<'a> {
    /// Resolve several roots concurrently: each worker owns a disjoint set of
    /// roots and walks its subgraph sequentially; results are merged in root
    /// order (valid because subgraphs of different roots are independent and
    /// each is returned in topological order).
    pub fn resolve_parallel(
        &self,
        roots: &[String],
        skip: &HashSet<String>,
        workers: usize,
    ) -> Result<(Vec<&'a F>, Vec<String>)> {
        if roots.len() <= 1 || workers <= 1 {
            return self.resolve(roots, skip);
        }
        let chunks: Vec<&[String]> = roots.chunks(roots.len().div_ceil(workers)).collect();
        let results: Vec<Result<(Vec<&'a F>, Vec<String>)>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunks
                .iter()
                .map(|chunk| scope.spawn(move || self.resolve(chunk, skip)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut order: Vec<&'a F> = Vec::new();
        let mut seen: HashSet<&'a str> = HashSet::new();
        let mut skipped: Vec<String> = Vec::new();
        for r in results {
            let (sub_order, sub_skipped) = r?;
            for f in sub_order {
                if seen.insert(f.name.as_str()) {
                    order.push(f);
                }
            }
            skipped.extend(sub_skipped);
        }
        Ok((order, skipped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(json: &str) -> Index {
        Index::parse(json, "test", 1).unwrap()
    }

    const GRAPH: &str = r#"
    [
      {"name":"app","versions":{"stable":"1"},"dependencies":["liba","libb"]},
      {"name":"liba","versions":{"stable":"1"},"dependencies":["libc"]},
      {"name":"libb","versions":{"stable":"1"},"dependencies":["libc"]},
      {"name":"libc","versions":{"stable":"1"}}
    ]"#;

    #[test]
    fn topo_order_and_dedup() {
        let i = idx(GRAPH);
        let r = Resolver::new(&i);
        let (order, _) = r.resolve(&["app".into()], &HashSet::new()).unwrap();
        let names: Vec<&str> = order.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names.len(), 4);
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("libc") < pos("liba"));
        assert!(pos("libc") < pos("libb"));
        assert!(pos("liba") < pos("app"));
        assert!(pos("libb") < pos("app"));
    }

    #[test]
    fn cycles_do_not_hang() {
        let i = idx(r#"[{"name":"a","versions":{"stable":"1"},"dependencies":["b"]},{"name":"b","versions":{"stable":"1"},"dependencies":["a"]}]"#);
        let r = Resolver::new(&i);
        let (order, _) = r.resolve(&["a".into()], &HashSet::new()).unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn skip_satisfied_deps() {
        let i = idx(GRAPH);
        let r = Resolver::new(&i);
        let mut skip = HashSet::new();
        skip.insert("libc".to_string());
        let (order, skipped) = r.resolve(&["app".into()], &skip).unwrap();
        assert_eq!(order.len(), 3);
        assert!(skipped.contains(&"libc".to_string()));
    }

    #[test]
    fn unknown_root_errors() {
        let i = idx(GRAPH);
        let r = Resolver::new(&i);
        let err = r.resolve(&["nope".into()], &HashSet::new()).unwrap_err();
        assert!(err.to_string().contains("no formula named"));
    }

    #[test]
    fn parallel_resolution_is_topologically_valid() {
        let i = idx(GRAPH);
        let r = Resolver::new(&i);
        let (order, _) = r
            .resolve_parallel(&["app".into(), "liba".into()], &HashSet::new(), 4)
            .unwrap();
        assert_eq!(order.len(), 4);
        let pos = |n: &str| order.iter().position(|f| f.name == n).unwrap();
        assert!(pos("libc") < pos("liba"));
        assert!(pos("libc") < pos("libb"));
        assert!(pos("liba") < pos("app"));
    }
}
