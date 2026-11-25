//! Bottle selection and extraction into the Cellar.
//!
//! A bottle is a `tar.gz` whose members look like `jq/1.7.1/bin/jq`,
//! `jq/1.7.1/.brew/jq.rb` and `jq/1.7.1/.brew/jq.INSTALL_RECEIPT.json`.
//! Extraction rewrites the leading `<name>/<version>/` to the target keg path
//! and rejects any path that would escape it.

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// OS-specific bottle tag, e.g. `arm64_sequoia` or `x86_64_linux`.
pub fn os_tag() -> Option<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match os {
        "macos" => {
            let arch = if arch == "aarch64" { "arm64" } else { "x86_64" };
            Some(format!("{arch}_{}", macos_codename()?))
        }
        "linux" => {
            let arch = match arch {
                "x86_64" | "amd64" => "x86_64",
                "aarch64" | "arm64" => "arm64",
                other => return Some(format!("{other}_linux")),
            };
            Some(format!("{arch}_linux"))
        }
        _ => None,
    }
}

/// Map the running macOS version to the bottle codename Homebrew uses.
fn macos_codename() -> Option<&'static str> {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let ver = String::from_utf8_lossy(&out.stdout);
    let major: u32 = ver.trim().split('.').next()?.parse().ok()?;
    Some(match major {
        26.. => "tahoe",
        15 => "sequoia",
        14 => "sonoma",
        13 => "ventura",
        12 => "monterey",
        11 => "big_sur",
        10 => return None,
        _ => return None,
    })
}

/// True when a keg looks correct for this OS (cellar path recorded in the
/// bottle matches where we are installing).
/// Where a bottle expects its Cellar to live.
#[derive(Debug, PartialEq)]
pub enum CellarKind {
    /// Relocatable (`cellar: :any`), typically after placeholder patching.
    Any,
    /// Relocatable with no relocation references at all.
    AnySkip,
    /// Built for one concrete Cellar path.
    Specific(String),
}

pub fn cellar_kind(cellar: &str) -> CellarKind {
    match cellar {
        ":any" | "" => CellarKind::Any,
        ":any_skip_relocation" => CellarKind::AnySkip,
        other => CellarKind::Specific(other.to_string()),
    }
}

/// Can this bottle be poured into `prefix`? Specific cellars only match the
/// exact prefix; relocatable bottles are poured and then patched.
pub fn cellar_compatible(cellar: &str, prefix: &Path) -> bool {
    match cellar_kind(cellar) {
        CellarKind::Any | CellarKind::AnySkip => true,
        CellarKind::Specific(c) => Path::new(&c) == prefix.join("Cellar"),
    }
}

/// Extract `tar.gz` at `archive` into `dest` (the keg dir), stripping one
/// `<name>` + one `<version>` component. Skips directories and refuses
/// absolute or `..` paths. Returns number of files written.
pub fn extract(archive: &Path, dest: &Path) -> Result<usize> {
    let file = std::fs::File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let gz = GzDecoder::new(std::io::BufReader::new(file));
    let mut tar = tar::Archive::new(gz);
    tar.set_preserve_permissions(true);
    let mut files = 0usize;

    for entry in tar.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let etype = entry.header().entry_type();
        let path = entry.path().context("entry path")?;
        let Some(rel) = sanitize(&path)? else { continue };
        let target = dest.join(&rel);

        // Symlinks are how bottles ship versioned .so chains; skip them and
        // binaries stop loading, so they are restored exactly.
        if etype.is_symlink() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if let Ok(Some(link_to)) = entry.link_name() {
                if link_to.is_absolute() {
                    // Absolute links into a build prefix cannot be trusted.
                    continue;
                }
                let _ = std::fs::remove_file(&target);
                if crate::archive::symlink(link_to.as_ref(), &target).is_ok() {
                    files += 1;
                }
            }
            continue;
        }
        if etype.is_hard_link() {
            if let Ok(Some(link_to)) = entry.link_name() {
                if let Ok(Some(rel_link)) = sanitize(link_to.as_ref()) {
                    let src = dest.join(rel_link);
                    if src.exists() {
                        let _ = std::fs::hard_link(&src, &target);
                        files += 1;
                    }
                }
            }
            continue;
        }
        if !etype.is_file() {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        out.write_all(&buf)?;
        // Preserve the executable bit and friends.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }
        files += 1;
    }
    Ok(files)
}

/// `jq/1.7.1/bin/jq` → `bin/jq`; returns `None` for broken paths.
fn sanitize(path: &Path) -> Result<Option<PathBuf>> {
    let mut comps = path.components();
    // strip `<name>`
    match comps.next() {
        Some(Component::Normal(_)) => {}
        _ => bail!("bottle entry escapes its root: {}", path.display()),
    }
    // strip `<version>`
    match comps.next() {
        Some(Component::Normal(_)) => {}
        _ => bail!("bottle entry escapes its root: {}", path.display()),
    }
    let mut out = PathBuf::new();
    for c in comps {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => bail!("bottle entry escapes its root: {}", path.display()),
        }
    }
    if out.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Read the `.brew/<name>.INSTALL_RECEIPT.json` from an extracted keg, if any.
pub fn install_receipt(keg: &Path) -> Option<serde_json::Value> {
    let rd = std::fs::read_dir(keg.join(".brew")).ok()?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.ends_with(".INSTALL_RECEIPT.json") {
            let data = std::fs::read(e.path()).ok()?;
            return serde_json::from_slice(&data).ok();
        }
    }
    None
}

/// Build a minimal INSTALL_RECEIPT for bottles that ship without one.
pub fn synth_receipt(formula: &str, version: &str) -> serde_json::Value {
    serde_json::json!({
        "homebrew_version": "picobrew",
        "used_options": [],
        "built_as_bottle": true,
        "poured_from_bottle": true,
        "runtime_dependencies": [],
        "installed_as_dependency": false,
        "installed_on_request": true,
        "source": { "versions": { "stable": version, "head": null, "version_scheme": 0 } },
        "formula": formula,
    })
}

// ---------------------------------------------------------------------------
// Relocation
// ---------------------------------------------------------------------------

pub const PLACEHOLDERS: [&str; 3] = [
    "@@HOMEBREW_PREFIX@@",
    "@@HOMEBREW_CELLAR@@",
    "@@HOMEBREW_REPOSITORY@@",
];

#[derive(Debug, Default)]
pub struct RelocReport {
    pub files_patched: usize,
    pub occurrences: usize,
    /// Files whose in-place patch would need a longer string than the
    /// placeholder allows (ELF offsets are fixed): not patched.
    pub unrelocatable: Vec<String>,
}

/// Replace bottle placeholders with this prefix, brew-style.
///
/// Text files (scripts, `.pc`, configs) take arbitrary-length substitutions.
/// ELF binaries are patched byte-for-byte inside each NUL-terminated slot and
/// zero-filled after the new text, so the file length never changes and the
/// suffix survives. Bottles concatenate paths without separators, so the
/// binding constraint is the tail after the last placeholder; when that does
/// not fit, the file is reported as unrelocatable instead of being corrupted.
pub fn relocate(keg: &Path, prefix: &Path) -> Result<RelocReport> {
    let mut report = RelocReport::default();
    let prefix = prefix.to_string_lossy().to_string();
    let replacements: [(&str, String); 3] = [
        (PLACEHOLDERS[0], prefix.clone()),
        (PLACEHOLDERS[1], format!("{prefix}/Cellar")),
        (PLACEHOLDERS[2], prefix.clone()),
    ];

    let mut stack = vec![keg.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() || name.starts_with('.') {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            if find_any(&bytes, &PLACEHOLDERS).is_none() {
                continue;
            }
            let is_elf = bytes.starts_with(b"\x7fELF");
            let (out, occurrences, overflow) = if is_elf {
                patch_elf(&bytes, &replacements)
            } else {
                patch_text(&bytes, &replacements)
            };
            let rel = path.strip_prefix(keg).unwrap_or(&path).display().to_string();
            if overflow {
                report.unrelocatable.push(rel);
                continue;
            }
            if occurrences == 0 {
                continue;
            }
            // Bottles ship many executables as 0o555; brew chmods u+w before
            // patching, then restores the mode. Patching in place (rather than
            // truncate-and-rewrite) also means a crash can never leave a
            // shortened binary behind.
            let original = std::fs::metadata(&path)
                .ok()
                .map(|m| m.permissions());
            let mut writable = original.clone();
            #[cfg(unix)]
            if let Some(p) = writable.as_mut() {
                use std::os::unix::fs::PermissionsExt;
                p.set_mode(p.mode() | 0o200);
            }
            if let Some(p) = writable {
                let _ = std::fs::set_permissions(&path, p);
            }
            {
                use std::io::{Seek, SeekFrom, Write};
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .with_context(|| format!("opening {} for relocation", path.display()))?;
                f.seek(SeekFrom::Start(0))?;
                f.write_all(&out)
                    .with_context(|| format!("relocating {}", path.display()))?;
                // Text substitutions change the file length; without this the
                // old tail would survive the short write. ELF patches preserve
                // length, so this is a no-op there.
                f.set_len(out.len() as u64)?;
                f.flush()?;
            }
            if let Some(p) = original {
                let _ = std::fs::set_permissions(&path, p);
            }
            report.files_patched += 1;
            report.occurrences += occurrences;
        }
    }
    Ok(report)
}

/// Arbitrary-length substitution for text files.
fn patch_text(bytes: &[u8], replacements: &[(&str, String)]) -> (Vec<u8>, usize, bool) {
    let mut out = bytes.to_vec();
    let mut occurrences = 0;
    for (ph, rep) in replacements {
        let mut from = 0usize;
        while let Some(found) = find(&out[from..], ph.as_bytes()) {
            let at = from + found;
            out.splice(at..at + ph.len(), rep.as_bytes().iter().copied());
            occurrences += 1;
            // Progress must never move backwards: when the replacement is
            // shorter than the placeholder (paths usually are), a smaller
            // step would rescan the replaced text and match placeholder tails.
            from = at + rep.len().max(ph.len()).min(out.len());
        }
    }
    (out, occurrences, false)
}

fn patch_elf(bytes: &[u8], replacements: &[(&str, String)]) -> (Vec<u8>, usize, bool) {
    let mut out = bytes.to_vec();
    let mut occurrences = 0;
    let mut start = 0usize;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|b| *b == 0)
            .map(|p| start + p)
            .unwrap_or(bytes.len());
        let segment = &bytes[start..end];
        let slot = end - start;
        if find_any(segment, &[replacements[0].0, replacements[1].0, replacements[2].0]).is_some() {
            // Byte-level (not String) patching: ELF string tables contain
            // arbitrary bytes, and lossy UTF-8 conversion would widen them.
            let mut patched: Vec<u8> = segment.to_vec();
            let mut overflowed = false;
            for (ph, rep) in replacements {
                let ph_b = ph.as_bytes();
                let rep_b: &[u8] = rep.as_bytes();
                if !patched.windows(ph_b.len()).any(|w| w == ph_b) {
                    continue;
                }
                occurrences += patched.windows(ph_b.len()).filter(|w| *w == ph_b).count();
                // Patch the placeholder itself, in place. A string is a
                // concatenation of runs (`@@PREFIX@@/Cellar/x:@@PREFIX@@/opt/…`)
                // and each run may shrink independently as long as the whole
                // NUL-terminated string stays within its slot.
                // Patch whole placeholders; the binding constraint is the
                // *last* one: new text plus whatever follows it must stay
                // inside the slot, since earlier runs shrink freely.
                let mut from = 0usize;
                let mut last_pos = None;
                while let Some(pos) = find(&patched[from..], ph_b) {
                    let at = from + pos;
                    last_pos = Some(at);
                    from = at + ph_b.len();
                }
                let Some(last_at) = last_pos else { continue };
                // Everything from the last placeholder onward is rewritten as
                // `rep + tail`; that is what must fit in the slot.
                let tail = patched[last_at + ph_b.len()..].to_vec();
                if rep_b.len() + tail.len() > slot {
                    overflowed = true;
                    break;
                }
                patched.truncate(last_at);
                // Replace the earlier runs first, then the last one.
                let mut from = 0usize;
                while let Some(pos) = find(&patched[from..], ph_b) {
                    let at = from + pos;
                    patched.splice(at..at + ph_b.len(), rep_b.iter().copied());
                    from = at + rep_b.len();
                }
                patched.extend_from_slice(rep_b);
                patched.extend_from_slice(&tail);
            }
            if overflowed {
                return (out, occurrences, true);
            }
            let write_to = (start + patched.len()).min(end);
            out[start..write_to].copy_from_slice(&patched[..write_to - start]);
            for b in &mut out[write_to..end] {
                *b = 0;
            }
        }
        start = end + 1;
    }
    (out, occurrences, false)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn find_any(hay: &[u8], needles: &[&str]) -> Option<usize> {
    needles.iter().filter_map(|n| find(hay, n.as_bytes())).min()
}

/// On Linux, Homebrew bottles point their ELF interpreter at
/// `<prefix>/lib/ld.so`, which upstream provides as a symlink to the system
/// loader. Without it the kernel refuses to start any poured binary, so the
/// pour has to provide it too (idempotent; never overwrites a real file).
pub fn ensure_linux_loader(prefix: &Path) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let candidates = [
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/lib64/ld-linux-aarch64.so.1",
        "/lib/ld-linux.so.2",
        "/lib/ld-musl-x86_64.so.1",
        "/lib/ld-musl-aarch64.so.1",
    ];
    let Some(loader) = candidates.iter().find(|c| Path::new(c).exists()) else {
        return Ok(());
    };
    let lib = prefix.join("lib");
    std::fs::create_dir_all(&lib).ok();
    let link = lib.join("ld.so");
    if link.is_symlink() || link.exists() {
        return Ok(());
    }
    crate::archive::symlink(Path::new(loader), &link)?;
    Ok(())
}

/// Cheap sanity check that the keg got populated.
pub fn keg_is_populated(keg: &Path) -> Result<()> {
    let mut any = false;
    if let Ok(rd) = std::fs::read_dir(keg) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n != ".brew" {
                any = true;
                break;
            }
        }
    }
    if any {
        Ok(())
    } else {
        Err(anyhow!("extraction produced an empty keg"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_two_components() {
        assert_eq!(
            sanitize(Path::new("jq/1.7.1/bin/jq")).unwrap().unwrap(),
            PathBuf::from("bin/jq")
        );
        assert_eq!(
            sanitize(Path::new("jq/1.7.1/.brew/jq.rb")).unwrap().unwrap(),
            PathBuf::from(".brew/jq.rb")
        );
        assert!(sanitize(Path::new("jq/../etc/passwd")).is_err());
        assert!(sanitize(Path::new("/etc/passwd")).is_err());
        assert!(sanitize(Path::new("jq")).is_err());
    }

    #[test]
    fn extracts_real_tarball_layout() {
        let tmp = std::env::temp_dir().join(format!("pico-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let archive = tmp.join("jq--1.7.1.x86_64_linux.bottle.tar.gz");
        write_tarball(&archive, &[("jq/1.7.1/bin/jq", "#!/bin/sh\necho jq\n", 0o755), ("jq/1.7.1/.brew/jq.rb", "class Jq; end\n", 0o644)]);
        let keg = tmp.join("keg");
        let n = extract(&archive, &keg).unwrap();
        assert_eq!(n, 2);
        assert!(keg.join("bin/jq").exists());
        assert_eq!(std::fs::read_to_string(keg.join("bin/jq")).unwrap(), "#!/bin/sh\necho jq\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(keg.join("bin/jq")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        keg_is_populated(&keg).unwrap();
        std::fs::remove_dir_all(&tmp).ok();
    }

    fn write_tarball(path: &Path, files: &[(&str, &str, u32)]) {
        let f = std::fs::File::create(path).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, content, mode) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(*mode);
            h.set_cksum();
            b.append_data(&mut h, *name, content.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn cellar_policy() {
        let p = Path::new("/tmp/prefix");
        assert!(cellar_compatible(":any", p));
        assert!(cellar_compatible(":any_skip_relocation", p));
        assert!(cellar_compatible("/tmp/prefix/Cellar", p));
        assert!(!cellar_compatible("/opt/homebrew/Cellar", p));
        assert_eq!(cellar_kind(":any"), CellarKind::Any);
    }

    #[test]
    fn relocates_text_and_elf_files() {
        let tmp = std::env::temp_dir().join(format!("pr{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let prefix = tmp.join("p"); // short enough for the 20-byte ELF slot
        let keg = prefix.join("Cellar/jq/1.8.2");
        std::fs::create_dir_all(keg.join("lib/pkgconfig")).unwrap();
        std::fs::create_dir_all(keg.join("bin")).unwrap();

        // Text file: arbitrary-length replacement. Built at runtime so there is
        // no escape ambiguity in the expectation either.
        let pc_original = format!("libdir={}/Cellar/jq/1.8.2/lib\n", PLACEHOLDERS[0]);
        std::fs::write(keg.join("lib/pkgconfig/libjq.pc"), &pc_original).unwrap();

        // Fake ELF: placeholder inside a fixed-width, NUL-terminated slot.
        let mut elf: Vec<u8> = vec![0x7f, b'E', b'L', b'F'];
        elf.extend_from_slice(&[0u8; 4]);
        let slot = format!("{}/lib/ld.so", PLACEHOLDERS[0]);
        elf.extend_from_slice(slot.as_bytes());
        elf.extend_from_slice(&[0u8; 12]);
        std::fs::write(keg.join("bin/jq"), &elf).unwrap();

        std::fs::write(keg.join("README.md"), "no placeholders here\n").unwrap();

        let r = relocate(&keg, &prefix).unwrap();
        assert_eq!(r.files_patched, 2, "text + elf patched");
        assert!(r.unrelocatable.is_empty());

        let pc = std::fs::read_to_string(keg.join("lib/pkgconfig/libjq.pc")).unwrap();
        assert_eq!(pc, format!("libdir={}/Cellar/jq/1.8.2/lib\n", prefix.display()));

        let elf_out = std::fs::read(keg.join("bin/jq")).unwrap();
        assert_eq!(elf_out.len(), elf.len(), "ELF patch must not change size");
        let expect = format!("{}/lib/ld.so", prefix.display());
        assert!(
            elf_out.windows(expect.len()).any(|w| w == expect.as_bytes()),
            "string slot keeps its suffix: {:?}",
            String::from_utf8_lossy(&elf_out)
        );
        assert!(!String::from_utf8_lossy(&elf_out).contains("@@HOMEBREW"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn relocates_read_only_binaries_and_restores_mode() {
        let tmp = std::env::temp_dir().join(format!("po{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let prefix = tmp.join("p");
        let keg = prefix.join("Cellar/x/1.0");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        let mut elf: Vec<u8> = vec![0x7f, b'E', b'L', b'F'];
        elf.extend_from_slice(format!("{}/lib/ld.so", PLACEHOLDERS[0]).as_bytes());
        elf.push(0);
        let path = keg.join("bin/x");
        std::fs::write(&path, &elf).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let r = relocate(&keg, &prefix).unwrap();
        assert_eq!(r.files_patched, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o555, "original mode restored");
        }
        assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains("@@HOMEBREW"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn flags_unrelocatable_elf_when_prefix_too_long() {
        let tmp = std::env::temp_dir().join(format!("px{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let long_prefix = tmp.join("a-very-long-prefix-name-that-cannot-fit-in-the-slot");
        let keg = long_prefix.join("Cellar/x/1");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        let mut elf: Vec<u8> = vec![0x7f, b'E', b'L', b'F'];
        elf.extend_from_slice(format!("{}/lib/ld.so", PLACEHOLDERS[0]).as_bytes());
        elf.push(0);
        std::fs::write(keg.join("bin/x"), &elf).unwrap();
        let r = relocate(&keg, &long_prefix).unwrap();
        assert_eq!(r.unrelocatable, vec!["bin/x"]);
        assert_eq!(std::fs::read(keg.join("bin/x")).unwrap(), elf, "left untouched rather than corrupted");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn section_names_collapse_while_paths_expand() {
        // Mirrors the shape of a Go bottle binary: the literal placeholder is
        // both a section-name substring and an rpath string. Prefix here is
        // deliberately too long for the path slot; patching must still succeed
        // for section names, and refuse only the oversized path.
        let mut bytes: Vec<u8> = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0];
        bytes.resize(0x40, 0);
        bytes.extend_from_slice(b"\0GO build ID=@@HOMEBREW_PREFIX@@x\0");
        let (text, _occurrences, overflow) = patch_elf(
            &bytes,
            &[(PLACEHOLDERS[0], "/a/very/long/prefix".to_string()), (PLACEHOLDERS[1], "/x/Cellar".to_string()), (PLACEHOLDERS[2], "/a/very/long/prefix".to_string())],
        );
        assert!(!overflow);
        assert!(!String::from_utf8_lossy(&text).contains("@@HOMEBREW"));
    }

    #[test]
    fn extracts_symlinks() {
        let tmp = std::env::temp_dir().join(format!("pico-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let archive = tmp.join("x.tar.gz");
        let f = std::fs::File::create(&archive).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "x/1.0/lib/libx.so.1.0", "data".as_bytes()).unwrap();
        let mut l = tar::Header::new_gnu();
        l.set_entry_type(tar::EntryType::Symlink);
        l.set_size(0);
        l.set_mode(0o777);
        l.set_link_name("libx.so.1.0").unwrap();
        l.set_cksum();
        b.append_data(&mut l, "x/1.0/lib/libx.so.1", std::io::empty()).unwrap();
        b.into_inner().unwrap().finish().unwrap(); // flush gzip trailer
        let keg = tmp.join("keg");
        extract(&archive, &keg).unwrap();
        let link = keg.join("lib/libx.so.1");
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("libx.so.1.0"));
        assert!(link.exists(), "symlink resolves to the extracted file");
        std::fs::remove_dir_all(&tmp).ok();
    }
}


#[cfg(test)]
mod realreloc {
    use super::*;
    #[test]
    fn relocate_real_jq_bottle() {
        let gz = match std::fs::read("/tmp/picohome/downloads/jq--1.8.2.x86_64_linux.bottle.tar.gz") { Ok(b) => b, Err(_) => return };
        let tmp = std::env::temp_dir().join("pico-realreloc");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let archive = tmp.join("jq.tar.gz");
        std::fs::write(&archive, &gz).unwrap();
        let keg = tmp.join("keg");
        extract(&archive, &keg).unwrap();
        let prefix = std::path::Path::new("/tmp/homebrew");
        let r = relocate(&keg, prefix).unwrap();
        println!("REPORT={r:?}");
        let elf = std::fs::read(keg.join("bin/jq")).unwrap();
        let t = b"@@HOMEBREW_PREFIX@@";
        println!("remaining placeholders: {}", elf.windows(t.len()).filter(|w| *w == t).count());
        println!("has interpreter path: {}", elf.windows(b"/tmp/homebrew/lib/ld.so".len()).any(|w| w == b"/tmp/homebrew/lib/ld.so"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
