//! Indexing a target dir: profile dirs, cargo's unit hash, the build lock.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Cargo's `-C extra-filename` hash: exactly 16 lowercase hex at the end of the
/// stem. Anchoring on the stem end matters -- `build_script_build-<hash>` and
/// `libfoo-<hash>` both end this way, but `foo-1.2.3` (a directory in `build/`)
/// does not, and neither does a crate whose own name has a hex-looking segment.
pub fn unit_hash(stem: &str) -> Option<&str> {
    let b = stem.as_bytes();
    if b.len() < 17 || b[b.len() - 17] != b'-' {
        return None;
    }
    let tail = &stem[stem.len() - 16..];
    tail.bytes()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        .then_some(tail)
}

/// The part of a file name before its first dot: `libfoo-<h>.rlib` and
/// `foo-<h>.d` both reduce to the stem the hash is anchored on.
pub fn stem(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// Every unit hash appearing in one directory's entry names, mapped to the
/// paths that carry it.
fn index_dir(dir: &Path, use_stem: bool) -> HashMap<String, Vec<PathBuf>> {
    let mut out: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = if use_stem { stem(&name) } else { name.as_str() };
        if let Some(h) = unit_hash(key) {
            out.entry(h.to_string()).or_default().push(entry.path());
        }
    }
    out
}

pub struct Units {
    pub deps: HashMap<String, Vec<PathBuf>>,
    pub fps: HashMap<String, Vec<PathBuf>>,
    pub builds: HashMap<String, Vec<PathBuf>>,
}

/// Index `deps/`, `.fingerprint/` and `build/` by cargo's unit hash.
///
/// The hash is the join key across all three: a unit compiled as
/// `deps/libfoo-<h>.rlib` has its freshness data in `.fingerprint/foo-<h>/`
/// and, if it runs a build script, its output in `build/foo-<h>/`. Keying on
/// the hash and never on the name is what makes the mismatched cases a
/// non-issue -- an integration test at `tests/it.rs` lands in `deps/` as
/// `it-<h>` but is fingerprinted under the *package* name.
pub fn scan_units(profile_dir: &Path) -> Units {
    // `examples/` counts as an artifact dir: cargo writes example units there
    // with their hash, not into `deps/`, so a fingerprint whose unit is an
    // example looks orphaned unless this dir is indexed too.
    let mut deps = index_dir(&profile_dir.join("deps"), true);
    for (h, paths) in index_dir(&profile_dir.join("examples"), true) {
        deps.entry(h).or_default().extend(paths);
    }
    Units {
        deps,
        fps: index_dir(&profile_dir.join(".fingerprint"), false),
        builds: index_dir(&profile_dir.join("build"), false),
    }
}

/// Extensions that mark a file as a library artifact rather than an executable.
pub const LIB_EXTS: [&str; 8] = ["rlib", "rmeta", "dylib", "so", "dll", "a", "lib", "wasm"];

/// Library artifacts whose file name carries NO unit hash, by target name.
///
/// Cargo omits `-C extra-filename` for units that produce a `cdylib` or
/// `staticlib`, because those file names have to be predictable for whatever
/// links them: a crate with `crate-type = ["cdylib", "rlib"]` lands in `deps/`
/// as `libfoo.rlib` and `libfoo.dylib`, with no hash anywhere in the name, while
/// its fingerprint dir is still `.fingerprint/foo-<hash>/`. There is then no way
/// to join the two by hash, and treating the fingerprint as orphaned deletes a
/// live unit -- which costs a recompile of that crate and everything downstream
/// of it. So these targets are exempted from collection entirely.
pub fn hashless_lib_targets(profile_dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    for sub in ["deps", "examples"] {
        let Ok(rd) = fs::read_dir(profile_dir.join(sub)) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let st = stem(&name);
            if unit_hash(st).is_some() {
                continue;
            }
            let ext = name.rsplit('.').next().unwrap_or("");
            if !LIB_EXTS.contains(&ext) {
                continue;
            }
            out.insert(st.to_string());
            if let Some(rest) = st.strip_prefix("lib") {
                out.insert(rest.to_string());
            }
        }
    }
    out
}

/// True if this profile holds `cargo check` units.
///
/// A check unit emits metadata only, so it appears in `deps/` as
/// `<name>-<hash>.rmeta` with no `.rlib`/`.dylib` beside it -- and it is a
/// *different unit* from the build unit of the same crate, with its own hash.
/// A build command therefore never resolves to one, so unless `cargo check` is
/// named too, checking runs cold after a collection. (Proc-macro units emit a
/// dylib, so they are not mistaken for check units.)
pub fn has_check_units(profile_dir: &Path) -> bool {
    let mut exts: HashMap<String, (bool, bool)> = HashMap::new();
    let Ok(rd) = fs::read_dir(profile_dir.join("deps")) else { return false };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let st = stem(&name);
        let Some(h) = unit_hash(st) else { continue };
        let ext = name[st.len()..].trim_start_matches('.').to_string();
        let entry = exts.entry(h.to_string()).or_insert((false, false));
        match ext.as_str() {
            "rmeta" => entry.0 = true,
            "rlib" | "dylib" | "so" | "dll" | "a" | "lib" => entry.1 = true,
            _ => {}
        }
    }
    exts.values().any(|(rmeta, linked)| *rmeta && !*linked)
}

/// The lib target names a fingerprint dir is about.
///
/// A fingerprint dir holds one file per output kind -- `lib-foo`, `bin-foo`,
/// `test-it` -- plus `dep-`/`output-` variants and `invoked.timestamp`.
pub fn fingerprint_lib_targets(dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let name = name.strip_suffix(".json").unwrap_or(&name);
        let name = name.strip_prefix("dep-").or_else(|| name.strip_prefix("output-")).unwrap_or(name);
        if let Some(target) = name.strip_prefix("lib-") {
            out.insert(target.to_string());
        }
    }
    out
}

/// True if this fingerprint belongs to a unit whose artifacts carry no hash, so
/// nothing can prove it dead.
pub fn fingerprint_is_hashless_lib(dir: &Path, hashless: &HashSet<String>) -> bool {
    !hashless.is_empty() && fingerprint_lib_targets(dir).iter().any(|t| hashless.contains(t))
}

/// Profile dirs under a target dir, at depth 1 (`target/debug`) and depth 2
/// (`target/<triple>/release`, so cross-compiled trees are collected too).
/// A profile dir is anything holding a `.fingerprint/`.
pub fn find_profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(target) else { return out };
    let mut level1: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    level1.sort();
    for d1 in level1 {
        if d1.join(".fingerprint").is_dir() {
            out.push(d1);
            continue;
        }
        if let Ok(rd2) = fs::read_dir(&d1) {
            let mut level2: Vec<PathBuf> =
                rd2.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            level2.sort();
            for d2 in level2 {
                if d2.join(".fingerprint").is_dir() {
                    out.push(d2);
                }
            }
        }
    }
    out
}

/// True if a cargo build owns this profile dir right now.
///
/// Cargo flocks `target/<profile>/.cargo-lock` for the length of a build.
/// Taking it non-blocking is the cheapest way to not delete artifacts out from
/// under a compile that is mid-flight -- and it is also what makes the
/// "-working" incremental session rule safe, since an in-flight session is only
/// abandoned if nobody holds this.
#[cfg(unix)]
pub fn build_lock_held(profile_dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;

    let lock = profile_dir.join(".cargo-lock");
    if !lock.exists() {
        return false;
    }
    let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(&lock) else {
        return false;
    };
    let fd = f.as_raw_fd();
    // SAFETY: fd is owned by `f` and valid for the duration of both calls.
    unsafe {
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 {
            libc::flock(fd, libc::LOCK_UN);
            return false;
        }
    }
    let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    e == libc::EWOULDBLOCK || e == libc::EACCES
}

/// Without flock, fall back to an exclusive open, which Windows denies while
/// cargo holds the file.
#[cfg(not(unix))]
pub fn build_lock_held(profile_dir: &Path) -> bool {
    let lock = profile_dir.join(".cargo-lock");
    lock.exists() && fs::OpenOptions::new().read(true).write(true).open(&lock).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile dir shaped like the two cases that used to be misread as dead
    /// units: a `cdylib` crate, whose artifacts carry no hash at all, and an
    /// example, whose artifacts live in `examples/` rather than `deps/`.
    fn fixture() -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("cargo-target-gc-test-{}", std::process::id()))
            .join("debug");
        let _ = fs::remove_dir_all(&root);
        for sub in ["deps", "examples", ".fingerprint/wdyl-713fd3ae45c909df", ".fingerprint/wlib-ea73957bd44e0bd0"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for f in ["deps/libwdyl.rlib", "deps/libwdyl.dylib", "deps/wdyl.d"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        fs::write(root.join("examples/demo-ea73957bd44e0bd0"), b"x").unwrap();
        fs::write(root.join("examples/demo"), b"x").unwrap();
        for f in ["lib-wdyl", "lib-wdyl.json", "dep-lib-wdyl", "invoked.timestamp"] {
            fs::write(root.join(".fingerprint/wdyl-713fd3ae45c909df").join(f), b"x").unwrap();
        }
        fs::write(root.join(".fingerprint/wlib-ea73957bd44e0bd0/example-demo"), b"x").unwrap();
        root
    }

    #[test]
    fn examples_are_indexed_as_artifacts() {
        let root = fixture();
        let units = scan_units(&root);
        // the example's fingerprint joins to examples/, so it is not orphaned
        assert!(units.deps.contains_key("ea73957bd44e0bd0"));
        assert!(units.fps.contains_key("ea73957bd44e0bd0"));
    }

    #[test]
    fn cdylib_units_are_unprovable() {
        let root = fixture();
        let hashless = hashless_lib_targets(&root);
        assert!(hashless.contains("wdyl"), "{hashless:?}");
        // .d files are dep-info, not artifacts, and must not exempt anything
        assert!(!hashless.contains("wdyl.d"));
        let fp = root.join(".fingerprint/wdyl-713fd3ae45c909df");
        assert!(fingerprint_is_hashless_lib(&fp, &hashless));
        // the cdylib's fingerprint has no artifact under its hash...
        let units = scan_units(&root);
        assert!(!units.deps.contains_key("713fd3ae45c909df"));
        // ...so only the exemption keeps it from being collected
        let other = root.join(".fingerprint/wlib-ea73957bd44e0bd0");
        assert!(!fingerprint_is_hashless_lib(&other, &hashless));
    }
}
