# Design notes

## The problem

`brew` is not slow because Ruby is slow. It is slow because most invocations begin by
loading a 34.9 MB JSON document describing 8,740 formulae and 53,244 bottles. Even with a
warm cache the parse dominates; with a cold cache the network transfer joins it.

picobrew's thesis: split that document by *access pattern*, not by convenience.

| data | access pattern | where it lives |
|---|---|---|
| name, version, deps, flags | every command | `formulas.fmt` — 843 KB, hand-parsed |
| descriptions | `search` only | same file (cheap enough, needed there) |
| caveats | `info`, after install | bottle record |
| bottle sha256 + cellar per platform | installs only, ~20 rows | `bottles.bin` + offset table |
| bottle URLs | never | derived from name + digest (100% of 53,244) |

## Why a hand-rolled format?

serde is excellent and slow. On a 8.7k-element array it spends most of its time in
tokeniser bookkeeping: peeking bytes, tracking depth, allocating per-Vec. Two changes
bought ~2× each:

1. **Records, not an array.** One JSON object per line lets the loader reuse a buffer and
   skip the array-state machine. Modest win alone.
2. **Pipe-separated fields, version first.** No escapes in the common case, no per-record
   object construction, direct `split` into fields, `parse()` for the single integer.
   Descriptions (44 chars average) dominate what remains.

The format is *not* a toy: fields are escaped (`\|`, `\n`, `\\`) so it is lossless whatever
upstream publishes. The live index contains three descriptions with `|`; they round-trip.

Measured: 1.94 MB JSON records → **843 KB** `.fmt`, ~11 ms → **~7 ms** to load.

## Why derive bottle URLs?

Storing 53,244 URLs costs ~4.8 MB of the cache and, worse, makes the store a blob to
regex-scan instead of a table to seek. All current bottles live at

```
https://ghcr.io/v2/homebrew/core/<repo>/blobs/sha256:<digest>
<repo> = name.replace("@", "/").replace("+", "x")
```

Verified against every bottle in the live index: **53,244 / 53,244**. Non-conforming URLs
are stored inline anyway (3 fields long), so the design degrades gracefully if Homebrew
changes its layout: the rare exception costs a few bytes, not a redesign.

## Why offsets instead of a second index?

An install needs ~20 bottle records out of 8,731. Parsing 4.8 MB to find them wastes
everything the hot path saved. `bottles.idx` maps name → (offset, length); the reader
seeks and parses one line. With a file-backed store the first lookup touches ~200 bytes.

## Relocation, precisely

Bottles are built with a placeholder prefix and patched at pour time ("relocation").
Text files take arbitrary-length substitution. ELF binaries cannot: every path lives in a
slot the linker sized at build time, so a patch that grows or shrinks the file breaks
offsets that the kernel and loader trust.

Rules that make this safe in practice:

* Patch **inside** the slot, zero-fill the remainder, `set_len()` never changes for ELF.
* Treat the *occurrence* as the unit, not the NUL-terminated string. Bottles concatenate
  (`…/lib@@HOMEBREW_PREFIX@@/opt/oniguruma/lib`), so a whole-string replacement truncates.
* The binding constraint is the tail after the **last** placeholder in a slot.
* Never round-trip binary data through `String`: invalid UTF-8 becomes U+FFFD and every
  byte after it shifts. Patch `[u8]`.
* Refuse and report rather than corrupt.

## State

`db.json` is a cache over the Cellar, never truth. It is validated by a fingerprint
(Cellar mtime + per-formula mtime sum) so `list` takes ~7 µs instead of walking kegs.
Two writers are excluded by a lock; readers never write.

## Known weaknesses

* Index load is ~7 ms, dominated by 843 KB of `String` allocations. A binary layout
  (name table + fixed-size records, borrowed at load) removes it.
* `search` scans linearly. At 8.7k formulas that is 1 ms — fine, and simpler than an index.
* Parallel resolution splits roots across threads; shared subgraphs are resolved twice
  (deduped at merge). Worth it for wall-clock, not for CPU.
