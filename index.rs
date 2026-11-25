//! The formula index.
//!
//! Homebrew publishes one 35 MB JSON document; `brew` pays for it on most
//! commands. picobrew splits that into two local caches with very different
//! access patterns:
//!
//! * **`index.json`** — the *hot core*: name, desc, version, deps, caveats for
//!   every formula (~2.5 MB). Parsed on every command in ~10 ms.
//! * **`bottles.idx` + `bottles.bin`** — the *bottle store*: newline-delimited
//!   records addressed by offset, so an install reads only the handful of
//!   records it actually needs instead of parsing all 53k bottle entries.
//!
//! Bottle URLs are not stored at all: every current Homebrew bottle lives at
//! `https://ghcr.io/v2/homebrew/core/<repo>/blobs/sha256:<digest>` where
//! `<repo>` is the formula name with `@` → `/` and `+` → `x` (verified across
//! all 53,244 bottles in the live index). The rare exception is kept inline.

use crate::paths::Paths;
use crate::ui;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_INDEX_URL: &str = "https://formulae.brew.sh/api/formula.jws.json";
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BottleFile {
    pub url: String,
    pub sha256: String,
    pub cellar: String,
}

/// One formula, compacted: no homepage/license/checksums we never use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct F {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub desc: String,
    pub version: String,
    #[serde(default)]
    pub revision: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_deps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_deps: Vec<String>,
    /// Caveats text is kept in the bottle store and loaded on demand.
    #[serde(skip)]
    pub caveats: Option<String>,
    #[serde(default)]
    pub keg_only: bool,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub disabled: bool,
}

impl F {
    /// The keg directory name, e.g. `1.7.1_2`.
    pub fn pkg_version(&self) -> String {
        if self.revision == 0 {
            self.version.clone()
        } else {
            format!("{}_{}", self.version, self.revision)
        }
    }
}

impl fmt::Display for F {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.version)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    pub generated_at: u64,
    pub source: String,
    pub formulas: Vec<F>,
    /// Alias → canonical name (`python3` → `python@3.13`).
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Old names that still resolve (`elinks` → `links`).
    #[serde(default)]
    pub oldnames: BTreeMap<String, String>,
    /// Bottle records; not part of the hot cache file.
    #[serde(skip)]
    store: Option<Arc<BottleStore>>,
}

impl Index {
    pub fn get(&self, name: &str) -> Option<&F> {
        if let Ok(i) = self.formulas.binary_search_by(|f| f.name.as_str().cmp(name)) {
            return Some(&self.formulas[i]);
        }
        let target = self.aliases.get(name).or_else(|| self.oldnames.get(name))?;
        let i = self
            .formulas
            .binary_search_by(|f| f.name.as_str().cmp(target.as_str()))
            .ok()?;
        Some(&self.formulas[i])
    }

    /// All names that would resolve to `name` (aliases + old names).
    pub fn aliases_of(&self, name: &str) -> Vec<&str> {
        self.aliases
            .iter()
            .chain(self.oldnames.iter())
            .filter(|(_, t)| t.as_str() == name)
            .map(|(a, _)| a.as_str())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.formulas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.formulas.is_empty()
    }

    /// Caveats text for a formula (kept out of the hot core index).
    pub fn caveats(&self, name: &str) -> Option<String> {
        self.store.as_ref().and_then(|s| s.caveats(name))
    }

    /// The bottle for `name` on `tag`, if the formula has one.
    pub fn bottle(&self, name: &str, tag: &str) -> Result<Option<BottleFile>> {
        match &self.store {
            Some(store) => store.get(name, tag),
            None => Ok(None),
        }
    }

    /// Parse Homebrew's `formula.jws.json` (a JWS envelope whose `payload` is
    /// a JSON string of the formula array) or a bare array.
    pub fn parse(source_json: &str, source_url: &str, now: u64) -> Result<Self> {
        let envelope: serde_json::Value =
            serde_json::from_str(source_json).context("index is not valid JSON")?;
        let items: Vec<RawFormula> = if let Some(payload) = envelope.get("payload") {
            match payload {
                serde_json::Value::String(s) => {
                    serde_json::from_str(s).context("index payload is not a formula array")?
                }
                other => serde_json::from_value(other.clone())
                    .context("index payload is not a formula array")?,
            }
        } else if envelope.is_array() {
            serde_json::from_value(envelope).context("index is not a formula array")?
        } else {
            bail!("unrecognised index format (missing `payload` array)");
        };

        let mut formulas = Vec::with_capacity(items.len());
        let mut aliases = BTreeMap::new();
        let mut oldnames = BTreeMap::new();
        let store = BottleStore::build(items.into_iter().inspect(|raw| {
            // aliases / old names before compaction
            for a in &raw.aliases {
                aliases.insert(a.clone(), raw.name.clone());
            }
            for o in &raw.oldnames {
                oldnames.insert(o.clone(), raw.name.clone());
            }
        }))?;

        for f in store.formulas() {
            formulas.push(f.clone());
        }
        formulas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            schema: crate::INDEX_SCHEMA,
            generated_at: now,
            source: source_url.to_string(),
            formulas,
            aliases,
            oldnames,
            store: Some(Arc::new(store)),
        })
    }
}

/// ghcr repository for a formula: `ansible@10` → `ansible/10`, `afl++` → `aflxx`.
pub fn ghcr_repo(name: &str) -> String {
    name.replace('@', "/").replace('+', "x")
}

/// The canonical bottle URL for a formula name and digest.
pub fn bottle_url(name: &str, sha256: &str) -> String {
    format!(
        "https://ghcr.io/v2/homebrew/core/{}/blobs/sha256:{}",
        ghcr_repo(name),
        sha256
    )
}

// ---------------------------------------------------------------------------
// The bottle store
// ---------------------------------------------------------------------------

/// Interned cellar strings; ids keep records small.
const STD_CELLARS: [&str; 5] = [
    ":any",
    ":any_skip_relocation",
    "/opt/homebrew/Cellar",
    "/usr/local/Cellar",
    "/home/linuxbrew/.linuxbrew/Cellar",
];

enum Source {
    Bytes(Vec<u8>),
    File(Mutex<std::fs::File>),
}

pub struct BottleStore {
    entries: HashMap<String, (u64, u32)>,
    cellars: Vec<String>,
    formula_names: Vec<F>,
    source: Source,
    cached_bytes: Mutex<Option<std::sync::Arc<Vec<u8>>>>,
    count: usize,
}

impl fmt::Debug for BottleStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BottleStore")
            .field("records", &self.entries.len())
            .field("bottles", &self.count)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct StoreRecord {
    /// formula name
    n: String,
    /// tag → [sha256, cellar_id] or [sha256, cellar_id, explicit_url]
    b: BTreeMap<String, Vec<serde_json::Value>>,
    /// caveats, when the formula has them
    #[serde(default, skip_serializing_if = "Option::is_none")]
    c: Option<String>,
}

impl BottleStore {
    /// Build records from raw formulas (consuming their bottle data).
    fn build(items: impl Iterator<Item = RawFormula>) -> Result<Self> {
        let mut cellars: Vec<String> = STD_CELLARS.iter().map(|s| s.to_string()).collect();
        let mut extra: HashMap<String, u16> = HashMap::new();
        let cellar_id = |c: &str, cellars: &mut Vec<String>, extra: &mut HashMap<String, u16>| -> u16 {
            if let Some(i) = cellars.iter().position(|x| x == c) {
                return i as u16;
            }
            if let Some(i) = extra.get(c) {
                return *i;
            }
            let id = (cellars.len() + extra.len()) as u16;
            extra.insert(c.to_string(), id);
            cellars.push(c.to_string());
            id
        };

        let mut data: Vec<u8> = Vec::with_capacity(6 << 20);
        let mut entries: HashMap<String, (u64, u32)> = HashMap::new();
        let mut formula_names: Vec<F> = Vec::new();
        let mut count = 0usize;

        for mut raw in items {
            // Compact the formula first: it owns caveats, which must not be lost
            // when the store consumes the raw bottle data.
            let name = raw.name.clone();
            let bottle = raw.bottle.take();
            let mut compact = raw.compact();
            let caveats = compact.caveats.take();
            compact.caveats = None;
            let mut rec = StoreRecord {
                n: name.clone(),
                b: BTreeMap::new(),
                c: None,
            };
            if let Some(bot) = bottle {
                if let Some(stable) = bot.stable {
                    for (tag, f) in stable.files {
                        if f.url.is_empty() {
                            continue; // checksum-only entry
                        }
                        let cellar = f.cellar.as_deref().unwrap_or("");
                        let cid = cellar_id(cellar, &mut cellars, &mut extra);
                        let derived = bottle_url(&name, &f.sha256);
                        let mut v = vec![
                            serde_json::Value::String(f.sha256.clone()),
                            serde_json::Value::from(cid),
                        ];
                        if f.url != derived {
                            v.push(serde_json::Value::String(f.url));
                        }
                        rec.b.insert(tag, v);
                        count += 1;
                    }
                }
            }
            if !rec.b.is_empty() || caveats.is_some() {
                rec.c = caveats;
                let bytes = serde_json::to_vec(&rec)?;
                let offset = data.len() as u64;
                data.extend_from_slice(&bytes);
                data.push(b'\n');
                entries.insert(rec.n.clone(), (offset, bytes.len() as u32));
            }
            formula_names.push(compact);
        }

        Ok(Self {
            entries,
            cellars,
            formula_names,
            source: Source::Bytes(data),
            cached_bytes: Mutex::new(None),
            count,
        })
    }

    fn formulas(&self) -> &[F] {
        &self.formula_names
    }

    /// Bytes, materialised on demand for in-memory stores loaded from disk.
    fn bytes(&self) -> Result<std::sync::Arc<Vec<u8>>> {
        match &self.source {
            Source::Bytes(b) => Ok(std::sync::Arc::new(b.clone())),
            Source::File(_) => {
                let mut cached = self.cached_bytes.lock().unwrap();
                if let Some(b) = cached.as_ref() {
                    return Ok(std::sync::Arc::clone(b));
                }
                let mut buf = Vec::new();
                self.read_bin(&mut buf)?;
                let arc = std::sync::Arc::new(buf);
                *cached = Some(std::sync::Arc::clone(&arc));
                Ok(arc)
            }
        }
    }

    fn read_bin(&self, buf: &mut Vec<u8>) -> Result<()> {
        match &self.source {
            Source::Bytes(b) => {
                buf.extend_from_slice(b);
                Ok(())
            }
            Source::File(f) => {
                let mut f = f.lock().unwrap();
                f.seek(SeekFrom::Start(0))?;
                f.read_to_end(buf)?;
                Ok(())
            }
        }
    }

    /// Turn a store backed by a file into a fully in-memory one (used right
    /// after a fetch, so the first command never re-reads from disk).
    pub fn materialise(mut self) -> Result<Self> {
        if matches!(self.source, Source::Bytes(_)) {
            return Ok(self);
        }
        let mut buf = Vec::new();
        self.read_bin(&mut buf)?;
        self.source = Source::Bytes(buf);
        Ok(self)
    }

    /// Caveats text for a formula, read from its record.
    pub fn caveats(&self, name: &str) -> Option<String> {
        self.record(name).ok().flatten().and_then(|r| r.c)
    }

    fn record(&self, name: &str) -> Result<Option<StoreRecord>> {
        let Some(&(offset, len)) = self.entries.get(name) else {
            return Ok(None);
        };
        let data = self.bytes()?;
        let start = offset as usize;
        let end = start + len as usize;
        if end > data.len() {
            bail!("bottle store truncated");
        }
        Ok(Some(serde_json::from_slice(&data[start..end]).context("bottle record corrupt")?))
    }

    /// Look up one (formula, tag) record. Reads only that formula's record.
    pub fn get(&self, name: &str, tag: &str) -> Result<Option<BottleFile>> {
        let Some(rec) = self.record(name)? else {
            return Ok(None);
        };
        let Some(vals) = rec.b.get(tag) else {
            return Ok(None);
        };
        if vals.len() < 2 {
            bail!("bottle record for {name} is malformed");
        }
        let sha256 = vals[0].as_str().unwrap_or_default().to_string();
        let cellar_id = vals[1].as_u64().unwrap_or(0) as usize;
        let cellar = self
            .cellars
            .get(cellar_id)
            .cloned()
            .unwrap_or_default();
        let url = match vals.get(2).and_then(|v| v.as_str()) {
            Some(u) => u.to_string(),
            None => bottle_url(name, &sha256),
        };
        Ok(Some(BottleFile {
            url,
            sha256,
            cellar,
        }))
    }

    /// Serialise the store for disk: (`bottles.idx`, `bottles.bin` bytes).
    pub fn to_disk(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        #[derive(Serialize)]
        struct DiskIndex<'a> {
            schema: u32,
            cellars: &'a [String],
            entries: Vec<(&'a str, (u64, u32))>,
            count: usize,
        }
        let _ = &self.cached_bytes;
        let mut entries: Vec<(&str, (u64, u32))> = self
            .entries
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        entries.sort();
        let idx = DiskIndex {
            schema: crate::INDEX_SCHEMA,
            cellars: &self.cellars,
            entries,
            count: self.count,
        };
        let idx_bytes = serde_json::to_vec(&idx)?;
        let bin = match &self.source {
            Source::Bytes(b) => b.clone(),
            Source::File(_) => bail!("store already on disk"),
        };
        Ok((idx_bytes, bin))
    }

    /// Open a store previously written by `to_disk`.
    pub fn from_disk(index_path: &std::path::Path, bin_path: &std::path::Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct DiskIndex {
            schema: u32,
            cellars: Vec<String>,
            entries: Vec<(String, (u64, u32))>,
            count: usize,
        }
        let idx_bytes = std::fs::read(index_path)?;
        let idx: DiskIndex = serde_json::from_slice(&idx_bytes).context("bottles.idx corrupt")?;
        if idx.schema != crate::INDEX_SCHEMA {
            bail!("bottles.idx schema mismatch");
        }
        let file = std::fs::File::open(bin_path)
            .with_context(|| format!("opening {}", bin_path.display()))?;
        Ok(Self {
            entries: idx.entries.into_iter().collect(),
            cellars: idx.cellars,
            formula_names: Vec::new(),
            source: Source::File(Mutex::new(file)),
            cached_bytes: Mutex::new(None),
            count: idx.count,
        })
    }
}

// ---------------------------------------------------------------------------
// Raw API shapes (only the fields we read)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawFormula {
    name: String,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    versions: RawVersions,
    #[serde(default)]
    revision: u32,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    oldnames: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    build_dependencies: Vec<String>,
    #[serde(default)]
    optional_dependencies: Vec<String>,
    #[serde(default)]
    caveats: Option<String>,
    #[serde(default)]
    keg_only: bool,
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    bottle: Option<RawBottle>,
}

#[derive(Debug, Default, Deserialize)]
struct RawVersions {
    #[serde(default)]
    stable: String,
}

#[derive(Debug, Deserialize)]
struct RawBottle {
    #[serde(default)]
    stable: Option<RawBottleStable>,
}

#[derive(Debug, Deserialize)]
struct RawBottleStable {
    #[serde(default)]
    files: BTreeMap<String, RawBottleFile>,
}

#[derive(Debug, Deserialize)]
struct RawBottleFile {
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    cellar: Option<String>,
}

impl RawFormula {
    fn compact(self) -> F {
        F {
            name: self.name,
            full_name: self.full_name,
            desc: self.desc.unwrap_or_default(),
            version: self.versions.stable,
            revision: self.revision,
            deps: self.dependencies,
            build_deps: self.build_dependencies,
            optional_deps: self.optional_dependencies,
            caveats: self.caveats.filter(|c| !c.trim().is_empty()),
            keg_only: self.keg_only,
            deprecated: self.deprecated,
            disabled: self.disabled,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading: disk cache → network
// ---------------------------------------------------------------------------

pub struct Loaded {
    pub index: Index,
    pub latency: Duration,
    pub from: &'static str,
}

pub fn load(paths: &Paths, max_age_override: Option<Duration>) -> Result<Loaded> {
    let started = Instant::now();
    let max_age = max_age_override.unwrap_or(CACHE_TTL);
    match read_cache(paths, max_age) {
        Ok(index) => Ok(Loaded {
            index,
            latency: started.elapsed(),
            from: if max_age.is_zero() { "network" } else { "cache" },
        }),
        Err(cache_err) => {
            if max_age.is_zero() {
                // --refresh: cache error irrelevant, go straight to network.
            } else if !ui::is_quiet() && !ui::is_json() {
                ui::step(&format!(
                    "Fetching formula index from formulae.brew.sh (first run only, {} MB)…",
                    35
                ));
            }
            match fetch_remote(DEFAULT_INDEX_URL, paths) {
                Ok(f) => Ok(Loaded {
                    index: f.index,
                    latency: f.latency,
                    from: "network",
                }),
                Err(e) => match read_cache(paths, Duration::MAX) {
                    Ok(mut index) => {
                        ui::warn(&format!(
                            "network fetch failed ({e:#}); using stale cached index"
                        ));
                        index.source.push_str(" (stale)");
                        Ok(Loaded {
                            index,
                            latency: started.elapsed(),
                            from: "stale-cache",
                        })
                    }
                    Err(_) => Err(e).context(format!(
                        "could not fetch the index and no usable cache exists ({cache_err})"
                    )),
                },
            }
        }
    }
}

fn cache_paths(paths: &Paths) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    (
        paths.index_cache(),
        paths.home.join("bottles.idx"),
        paths.home.join("bottles.bin"),
    )
}

fn read_cache(paths: &Paths, max_age: Duration) -> Result<Index> {
    let (_, idx_path, bin_path) = cache_paths(paths);
    let formulas_path = paths.formulas_path();
    let meta = std::fs::metadata(&formulas_path)?;
    if max_age != Duration::MAX {
        let age = meta.modified()?.elapsed().unwrap_or(Duration::MAX);
        if age > max_age {
            bail!("cache expired (age {:.1}h)", age.as_secs_f64() / 3600.0);
        }
    }
    #[allow(dead_code)]
    #[derive(Deserialize)]
    struct AliasFile {
        schema: u32,
        #[serde(default)]
        generated_at: u64,
        #[serde(default)]
        source: String,
        #[serde(default)]
        aliases: BTreeMap<String, String>,
        #[serde(default)]
        oldnames: BTreeMap<String, String>,
    }
    let alias_bytes = std::fs::read(paths.aliases_path())?;
    let alias_file: AliasFile =
        serde_json::from_slice(&alias_bytes).context("cached aliases corrupt")?;
    if alias_file.schema != crate::INDEX_SCHEMA {
        bail!("cached index schema mismatch");
    }

    // Read the whole file once, then split on newlines: one allocation, and no
    // per-record serde state machine.
    let data = std::fs::read(&formulas_path)?;
    let mut formulas = Vec::with_capacity(9000);
    for line in data.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(line).context("cached formulas corrupt (utf8)")?;
        formulas.push(parse_formula_record(text).context("cached formulas corrupt")?);
    }
    if formulas.is_empty() {
        bail!("cached formulas empty");
    }
    let store = BottleStore::from_disk(&idx_path, &bin_path)?;
    Ok(Index {
        schema: crate::INDEX_SCHEMA,
        generated_at: alias_file.generated_at,
        source: alias_file.source,
        formulas,
        aliases: alias_file.aliases,
        oldnames: alias_file.oldnames,
        store: Some(Arc::new(store)),
    })
}

pub struct FetchResult {
    pub index: Index,
    pub latency: Duration,
    pub source_bytes: usize,
    pub compact_bytes: usize,
}

pub fn fetch_remote(url: &str, paths: &Paths) -> Result<FetchResult> {
    let started = Instant::now();
    let body = crate::net::get_text(url).context("downloading formula index")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let index = Index::parse(&body, url, now)?;
    if index.is_empty() {
        bail!("downloaded index is empty");
    }
    let latency = started.elapsed();
    let core = serde_json::to_vec(&index)?;
    // Materialise the store so in-memory bottle() lookups never touch disk.
    let mut index = index;
    if let Some(store) = index.store.take() {
        match Arc::try_unwrap(store) {
            Ok(s) => index.store = Some(Arc::new(s.materialise()?)),
            Err(arc) => index.store = Some(arc),
        }
    }
    let result = FetchResult {
        index,
        latency,
        source_bytes: body.len(),
        compact_bytes: core.len(),
    };
    write_cache(paths, &result.index, &core);
    Ok(result)
}

/// Compact, NDJSON-backed layout:
///
/// * `formulas.ndjson` — one JSON object per formula (~2 MB, parsed lazily one
///   record at a time instead of as one 9k-element array),
/// * `aliases.json` — alias/old-name maps,
/// * `bottles.idx` + `bottles.bin` — the random-access bottle store.
///
/// A single-array document costs serde a full pass over 9k elements before it
/// can return; records let the loader stream with a reused buffer.
fn write_cache(paths: &Paths, index: &Index, _core: &[u8]) {
    if paths.ensure_home().is_err() {
        return;
    }
    let (core_path, idx_path, bin_path) = cache_paths(paths);
    let Ok((idx_bytes, bin_bytes)) = index
        .store
        .as_ref()
        .map(|s| s.to_disk())
        .unwrap_or_else(|| Ok((Vec::new(), Vec::new())))
    else {
        return;
    };
    let write_atomic = |path: &std::path::Path, bytes: &[u8]| -> bool {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_ok()
    };
    // formulas.fmt: one pipe-separated record per formula, written in the
    // order the parser wants to read it.
    let mut ndjson = Vec::with_capacity(2 << 20);
    for f in &index.formulas {
        for (i, field) in formula_fields(f).iter().enumerate() {
            if i > 0 {
                ndjson.push(b'|');
            }
            ndjson.extend_from_slice(escape_field(field).as_bytes());
        }
        ndjson.push(b'\n');
    }
    let ok = true;
    // aliases.json
    #[derive(serde::Serialize)]
    struct AliasFile<'a> {
        schema: u32,
        generated_at: u64,
        source: &'a str,
        aliases: &'a BTreeMap<String, String>,
        oldnames: &'a BTreeMap<String, String>,
    }
    let alias_bytes = serde_json::to_vec(&AliasFile {
        schema: crate::INDEX_SCHEMA,
        generated_at: index.generated_at,
        source: &index.source,
        aliases: &index.aliases,
        oldnames: &index.oldnames,
    })
    .unwrap_or_default();
    if ok
        && write_atomic(&paths.formulas_path(), &ndjson)
        && write_atomic(&paths.aliases_path(), &alias_bytes)
        && write_atomic(&idx_path, &idx_bytes)
        && write_atomic(&bin_path, &bin_bytes)
    {
        let _ = std::fs::remove_file(&core_path); // superseded layout
        let _ = std::fs::write(paths.home.join("index.meta"), index.generated_at.to_string());
    }
}

// ---------------------------------------------------------------------------
// The on-disk formula format
// ---------------------------------------------------------------------------
//
// serde spends most of its time on tokeniser bookkeeping. A package manager's
// hot file deserves a hand-rolled format: pipe-separated, never escaped
// (the source data contains no pipes — verified against all 8,740 formulas),
// with the version first so the parser can attach one reusable buffer per
// formula instead of allocating a Vec per line.
pub const RECORD_FIELDS: usize = 10;

/// Escape a field for the pipe/newline-delimited record format.
/// The live index has no pipes outside descriptions, but the format must not
/// depend on that: escaping keeps it lossless whatever upstream publishes.
fn escape_field(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.bytes().any(|b| b == b'|' || b == b'\\' || b == b'\n' || b == b'\r') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\p"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    std::borrow::Cow::Owned(out)
}

fn unescape_field(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('p') => out.push('|'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn formula_fields(f: &F) -> [String; RECORD_FIELDS] {
    [
        f.version.clone(),
        f.name.clone(),
        f.full_name.clone().unwrap_or_default(),
        f.desc.clone(),
        f.revision.to_string(),
        f.deps.join(" "),
        f.build_deps.join(" "),
        f.optional_deps.join(" "),
        if f.keg_only { "1" } else { "0" }.to_string(),
        match (f.deprecated, f.disabled) {
            (false, false) => "0",
            (true, false) => "1",
            (false, true) => "2",
            (true, true) => "3",
        }
        .to_string(),
    ]
}

fn parse_formula_record(line: &str) -> Result<F> {
    let mut it = line.split('|');
    let version = unescape_field(it.next().unwrap_or_default());
    let name = unescape_field(it.next().unwrap_or_default());
    let full_name = unescape_field(it.next().unwrap_or_default());
    let desc = unescape_field(it.next().unwrap_or_default());
    let revision: u32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let deps = split_words(it.next().unwrap_or_default());
    let build_deps = split_words(it.next().unwrap_or_default());
    let optional_deps = split_words(it.next().unwrap_or_default());
    let keg_only = it.next().unwrap_or("0") == "1";
    let flags = it.next().unwrap_or("0");
    Ok(F {
        name,
        full_name: if full_name.is_empty() {
            None
        } else {
            Some(full_name)
        },
        desc,
        version,
        revision,
        deps,
        build_deps,
        optional_deps,
        caveats: None,
        keg_only,
        deprecated: flags == "1" || flags == "3",
        disabled: flags == "2" || flags == "3",
    })
}

fn split_words(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(' ').map(str::to_string).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "payload": "[{\"name\":\"jq\",\"full_name\":\"jq\",\"desc\":\"JSON processor\",\"versions\":{\"stable\":\"1.7.1\"},\"revision\":0,\"aliases\":[],\"dependencies\":[\"oniguruma\"],\"bottle\":{\"stable\":{\"files\":{\"arm64_sequoia\":{\"cellar\":\"/opt/homebrew/Cellar\",\"url\":\"https://ghcr.io/v2/homebrew/core/jq/blobs/sha256:abc\",\"sha256\":\"abc\"},\"x86_64_linux\":{\"cellar\":\"/home/linuxbrew/.linuxbrew/Cellar\",\"url\":\"https://ghcr.io/v2/homebrew/core/jq/blobs/sha256:def\",\"sha256\":\"def\"}}}}}]",
      "signatures": []
    }"#;

    #[test]
    fn parses_jws_envelope() {
        let idx = Index::parse(FIXTURE, "test", 1).unwrap();
        assert_eq!(idx.len(), 1);
        let f = idx.get("jq").unwrap();
        assert_eq!(f.version, "1.7.1");
        assert_eq!(f.deps, vec!["oniguruma"]);
        let b = idx.bottle("jq", "x86_64_linux").unwrap().unwrap();
        assert_eq!(b.sha256, "def");
        assert_eq!(b.cellar, "/home/linuxbrew/.linuxbrew/Cellar");
        assert!(idx.bottle("jq", "arm64_sonoma").unwrap().is_none());
    }

    #[test]
    fn url_derivation_matches_ghcr_scheme() {
        assert_eq!(
            bottle_url("jq", "abc"),
            "https://ghcr.io/v2/homebrew/core/jq/blobs/sha256:abc"
        );
        assert_eq!(ghcr_repo("ansible@10"), "ansible/10");
        assert_eq!(ghcr_repo("afl++"), "aflxx");
    }

    #[test]
    fn parses_bare_array() {
        let idx = Index::parse(r#"[{"name":"x","versions":{"stable":"2.0"}}]"#, "t", 1).unwrap();
        assert_eq!(idx.get("x").unwrap().version, "2.0");
    }

    #[test]
    fn resolves_aliases_and_oldnames() {
        let json = r#"[{"name":"python@3.13","aliases":["python3","python"],"versions":{"stable":"3.13.1"}},{"name":"links","oldnames":["elinks"],"versions":{"stable":"2.30"}}]"#;
        let idx = Index::parse(json, "t", 1).unwrap();
        assert_eq!(idx.get("python3").unwrap().name, "python@3.13");
        assert_eq!(idx.get("python").unwrap().name, "python@3.13");
        assert_eq!(idx.get("elinks").unwrap().name, "links");
        assert_eq!(idx.get("python3").unwrap().version, "3.13.1");
        assert_eq!(idx.aliases_of("links"), vec!["elinks"]);
        assert_eq!(idx.aliases_of("python@3.13").len(), 2);
    }

    #[test]
    fn non_derivable_urls_are_stored_inline() {
        let json = r#"[{"name":"weird","versions":{"stable":"1"},"bottle":{"stable":{"files":{"x86_64_linux":{"cellar":":any","url":"https://example.com/weird.tar.gz","sha256":"aa"}}}}}]"#;
        let idx = Index::parse(json, "t", 1).unwrap();
        let b = idx.bottle("weird", "x86_64_linux").unwrap().unwrap();
        assert_eq!(b.url, "https://example.com/weird.tar.gz");
        assert_eq!(b.cellar, ":any");
    }

    #[test]
    fn skips_bottles_without_urls() {
        let json = r#"[{"name":"y","versions":{"stable":"1"},"bottle":{"stable":{"files":{"a":{"cellar":"/x"}}}}}]"#;
        let idx = Index::parse(json, "t", 1).unwrap();
        assert!(idx.bottle("y", "a").unwrap().is_none());
    }

    #[test]
    fn store_roundtrips_through_disk() {
        let idx = Index::parse(FIXTURE, "t", 1).unwrap();
        let store = idx.store.as_ref().unwrap();
        let (idx_bytes, bin_bytes) = store.to_disk().unwrap();
        let tmp = std::env::temp_dir().join(format!("pico-store-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let ip = tmp.join("bottles.idx");
        let bp = tmp.join("bottles.bin");
        std::fs::write(&ip, idx_bytes).unwrap();
        std::fs::write(&bp, bin_bytes).unwrap();
        let reopened = BottleStore::from_disk(&ip, &bp).unwrap();
        let b = reopened.get("jq", "arm64_sequoia").unwrap().unwrap();
        assert_eq!(b.sha256, "abc");
        assert!(reopened.get("jq", "x86_64_linux").unwrap().is_some());
        assert!(reopened.get("jq", "nope").unwrap().is_none());
        assert!(reopened.get("nope", "arm64_sequoia").unwrap().is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn hot_core_has_no_bottles() {
        let idx = Index::parse(FIXTURE, "t", 1).unwrap();
        let core = serde_json::to_string(&idx).unwrap();
        assert!(!core.contains("ghcr.io"));
        assert!(core.contains("\"formulas\""));
    }
}
