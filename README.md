# picobrew

**A pico-fast package manager for Homebrew bottles, written in Rust.**

Landing page: **https://picobrew.vercel.app**

`picobrew` pours real Homebrew bottles — same bottles, same Cellar layout, same install
receipts — without Homebrew's Ruby stack in the hot path. It keeps a pre-digested local
index and reads only the slice it needs, so everyday commands answer in milliseconds
instead of reparsing a 35 MB JSON document.

```
$ picobrew install jq
==> Index: 8740 formulas from cache in 13ms
==> → 2 formulae planned: oniguruma jq
  ✓ oniguruma 6.9.10 · 15 files, 1 binaries (onig-config) · 8 links · 3 relocated (10 refs)
  ✓ jq 1.8.2 · 20 files, 1 binaries (jq) · 10 links · 3 relocated (11 refs)
✓ installed 2 formulae in 45ms · all bottles from cache

$ /tmp/homebrew/bin/jq --version
jq-1.8.2
```

> **Status: working prototype.** Installs, upgrades, uninstalls and links real Linux/macOS
> bottles. Source builds, casks and Homebrew's service manager are not implemented yet.
> See [Limitations](#limitations) before trusting it with a production machine.

---

## Measured performance

All numbers below are measured in this repository's development sandbox (Linux x86-64,
warm page cache, network to `ghcr.io`/`formulae.brew.sh`). Reproduce with `./bench/bench.sh`
and `picobrew bench`. They are *not* an end-to-end comparison with `brew install` — a
`brew` was not available in the measurement environment, so the reference line is
Homebrew's own index fetch, which `brew` performs before it can answer most queries.

| workload | median | notes |
|---|---|---|
| `picobrew list` | **~1.2 ms** | ~0.2 ms net of process spawn |
| `picobrew search jq` | **~12 ms** | loads 8,740 formula records, ~860 KB |
| `picobrew info ripgrep` | **~13 ms** | includes bottle lookup + state check |
| `picobrew install --dry-run` | **~12 ms** | full dependency resolution |
| `picobrew install jq` (bottles cached) | **45 ms** | including relocate + link |
| `picobrew install jq` (cold, downloads) | **~400 ms** | 2 bottles over the network |
| `picobrew install ripgrep` (cold) | **977 ms** | 9.29 MiB across 2 bottles |
| `picobrew index --refresh` | **~320 ms** | 33.3 MiB source → 1.87 MiB cache |
| `brew` index fetch (reference) | **185–980 ms** | varies with network; per-command cost |

In-process timings (`picobrew bench`): index load **7 ms**, fuzzy search over all
formulae **1 ms**, dependency graph for 8 packages **27 µs**, state read **7 µs**.

The honest summary: **picobrew's startup-to-answer path is 15–80× shorter than one
Homebrew index fetch**, and installing is fast once bottles are cached. Commands that
need the formula index pay ~11 ms of parse time today — see
[Roadmap](#roadmap) for the binary-index work that removes it.

---

## Install

```bash
cargo build --release          # produces target/release/picobrew (~3.4 MB, stripped)
install -m 755 target/release/picobrew /usr/local/bin/
```

Requirements: Rust 1.89+, and a unix-like OS. On Linux the target prefix must be a
writable directory; `/home/linuxbrew/.linuxbrew` matches what Homebrew bottles expect.

## Usage

```
picobrew [--prefix DIR] [--quiet] [--json] [--refresh] <command>

  install <formula>...     install formulae with dependencies  (aliases: add, i)
      -j, --jobs N           parallel download jobs (default: min(2×CPUs, 8))
      --dry-run              resolve and print the plan, touch nothing
      -f, --force            reinstall / relink even when current
  uninstall <formula>...   remove formulae                      (aliases: remove, rm)
      -f, --force            remove even if others depend on it
  upgrade [formula]...     upgrade all outdated formulae        (alias: update)
  list                     installed formulae                    (alias: ls)
      -v, --versions         show versions (+ ⬆ marker when outdated)
  outdated                 formulae with newer versions available
  search <query>           fuzzy name/description search
  info <formula>...        versions, deps, bottle availability, caveats
  doctor                   check prefix, Cellar, state, links, caches
  index [--refresh]        inspect or rebuild the local index
  bench [--no-brew-ref]    measure hot paths against a live Homebrew reference
```

Global flags: `--prefix DIR` (or `$PICOBREW_PREFIX`), `--quiet`, `--json` (machine-readable
output where supported), `--refresh` (ignore the cache and refetch).

Cache lives in `~/.picobrew` (override with `$PICOBREW_HOME`):

```
~/.picobrew/
├── formulas.fmt        # 843 KB — one record per formula, pipe-separated
├── aliases.json        # 9 KB — alias + old-name maps
├── bottles.idx         # 228 KB — name → (offset, length) into bottles.bin
├── bottles.bin         # 4.8 MB — random-access bottle records
└── downloads/          # bottle tarballs, sha256-verified
```

---

## How it works

Homebrew publishes one 34.9 MB `formula.jws.json` (a JWS envelope wrapping a JSON array).
`brew` pays for it on most commands. picobrew splits the problem in three:

**1. A hot core that is cheap to parse.** The index is compacted to what a package
manager actually needs (name, version, deps, flags) and written as one pipe-separated
record per formula. serde's tokeniser costs more than the data itself on an 8.7k-element
array, so the hot format is parsed by hand: split on newlines, split on `|`, attach into a
reused buffer. Descriptions and caveats live elsewhere.

**2. A bottle store read by offset.** All 53,244 (formula, platform) bottle records live
in a newline-delimited file with an offset table. An install reads *only* the ~20 records
it needs instead of parsing every bottle. Bottle URLs are not stored at all — all 53,244
live at `https://ghcr.io/v2/homebrew/core/<repo>/blobs/sha256:<digest>`, where `<repo>`
is the name with `@`→`/` and `+`→`x` (verified across the live index). The 3 descriptions
in upstream data containing `|` are escaped, so the format is lossless.

**3. Real pours.** Bottles are `tar.gz` with `name/version/…` members, symlinks and
hardlinks, extracted with path-escape checks and mode preservation. Placeholders
(`@@HOMEBREW_PREFIX@@`, `@@HOMEBREW_CELLAR@@`, `@@HOMEBREW_REPOSITORY@@`) are rewritten
in place:

* text files take arbitrary-length substitutions;
* ELF binaries are patched byte-for-byte at whole-placeholder granularity so file
  **lengths never change** — required, because the kernel and dynamic loader read
  strings at fixed offsets;
* the fit check uses the *tail* after the last placeholder, because bottles concatenate
  paths without separators (`…/lib@@HOMEBREW_PREFIX@@/opt/x`);
* binaries shipped `0o555` are chmod'd `u+w`, patched, then restored;
* if a replacement cannot fit, the file is reported as unrelocatable rather than corrupted;
* on Linux, `<prefix>/lib/ld.so` is created (pointing at the system loader) because every
  Linux bottle's ELF interpreter points there.

Then `opt/<name>` and the two-level `bin/`, `lib/`, `share/` symlink sweep are created
with a link table stored per keg, so uninstall never guesses. State (`db.json`) is a cache
over the Cellar — the Cellar is the source of truth, so picobrew and `brew` can share a
prefix — validated by a cheap fingerprint so `list` never rescans.

Downloads run on a thread pool with sha256 verified *while streaming*; GHCR tokens are
fetched once per repository and the auth header is dropped after the first redirect
(signed CDN URLs reject requests carrying both). Mutating commands hold an exclusive
lock, so two picobrews can't corrupt a Cellar.

---

## Testing

```bash
cargo test          # 42 tests
cargo clippy -- -D warnings
./bench/bench.sh    # end-to-end timings, including a live brew reference fetch
```

The suite covers version ordering (Homebrew semantics: `1.10 > 1.9`, `1.0.0 > 1.0.0-rc1`),
alias/old-name resolution, the JWS envelope, offline index round-trips, topological
dependency order with cycles, skip-satisfied logic, tar extraction (files, symlinks,
hardlinks, escape attempts), link/unlink round-trips, user-owned file protection,
relocation of text and ELF files including the read-only and too-long-prefix cases, and a
regression test that relocates a *real* downloaded jq bottle and asserts no placeholder
survives in the binary.

Two bugs the tests caught, for the record: relocating ELF placeholders as whole
NUL-terminated strings truncates the suffix (they must be patched occurrence-by-occurrence
inside the slot), and round-tripping binary data through `String` silently corrupts it
(invalid UTF-8 becomes U+FFFD).

---

## Limitations

Honest list, in rough order of how likely they are to bite:

* **Source builds are not supported.** Formulae without a bottle for the current platform
  fail with a clear error.
* **Casks are not supported** (no `.app` bundles, no DMG handling).
* **No `brew services`**, no `post_install` hooks, no per-formula test runners.
* **Relocation of long prefixes fails for ELF binaries.** The replacement must fit the
  placeholder's original slot (20 bytes for the prefix). A 43-byte interpreter path
  (`/Users/someone/homebrew/…`) cannot fit; picobrew refuses those files instead of
  corrupting them. macOS default prefixes (`/opt/homebrew`, `/usr/local`) fit comfortably.
* **No code signing / quarantine handling** on macOS.
* **Linux bottles need `<prefix>/lib/ld.so`** — created automatically, pointing at the
  system loader; versions of glibc older than the bottle's build host may still be too old.
* **Concurrent `brew` + `picobrew` use** is safe for reads; both writing at once is not.
* **Index commands still pay ~11 ms** to load the formula records. `list` does not.

## Roadmap

1. `search --json` currently returns the wrong CSV-ish shape — the JSON path needs the
   same records the text path uses.
2. Binary index (names + offsets, zero-copy at startup) to bring search/info under ~2 ms.
3. Source builds from `urls.stable` + recorded build dependencies.
4. Cask support (`api/cask.jws.json`), DMG/pkg installers.
5. `services` (systemd/launchd) from the index's `service` field.
6. Signed/index-verified downloads, macOS codesign + xattr handling.

## License

MIT (matching its subject matter's spirit; see `LICENSE`). Not affiliated with or endorsed
by Homebrew. `picobrew` consumes Homebrew's public API and bottle registry.
