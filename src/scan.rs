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

/// Whether this profile holds build units, check units, or both -- asked about
/// *this workspace's own* targets.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub build: bool,
    pub check: bool,
}

/// Which compile modes this tree has actually run, read off `deps/`.
///
/// A check unit emits metadata only, so it appears as `<name>-<hash>.rmeta`
/// with no `.rlib`/`.dylib` beside it -- and it is a *different unit* from the
/// build unit of the same crate, with its own hash. Neither command's units are
/// reachable from the other, so each mode has to be marked by the matching
/// command, and running a mode this tree has never used is a cold compile paid
/// to mark nothing. That is the whole question here.
///
/// It is asked only about the workspace's own targets, because a dependency
/// answers it wrong in both directions: `cargo check` compiles build-script
/// dependencies for real (they have to run), so a checked-only tree is full of
/// registry `.rlib`s, and a built tree still carries `.rmeta` for every
/// dependency whose rlib was pipelined. What settles it is whether *your* lib
/// was linked or only metadata-checked.
///
/// A profile with no unit of ours at all concludes nothing -- both false --
/// rather than "never built": that is the shape of a tree this run could not
/// read, and skipping the default build on it would leave live units unmarked.
pub fn built_modes(profile_dir: &Path, ws_targets: &[(&str, bool)]) -> Modes {
    let mut ours: HashSet<String> = HashSet::new();
    for (name, is_lib) in ws_targets {
        let under = name.replace('-', "_");
        if *is_lib {
            // a lib artifact is `lib<crate_name>`, always with underscores
            ours.insert(format!("lib{under}"));
            ours.insert(under);
        } else {
            // a bin keeps the target name as written, dashes and all
            ours.insert(name.to_string());
            ours.insert(under);
        }
    }

    let mut units: HashMap<String, (bool, bool)> = HashMap::new();
    let Ok(rd) = fs::read_dir(profile_dir.join("deps")) else { return Modes::default() };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let st = stem(&name);
        let Some(h) = unit_hash(st) else { continue };
        if !ours.contains(&st[..st.len() - 17]) {
            continue;
        }
        let ext = name[st.len()..].trim_start_matches('.');
        let u = units.entry(h.to_string()).or_insert((false, false));
        match ext {
            "rmeta" => u.0 = true,
            // no extension at all is a bin: linked, and the clearest proof of a
            // real build there is
            "rlib" | "dylib" | "so" | "dll" | "a" | "lib" | "" => u.1 = true,
            _ => {}
        }
    }
    Modes {
        build: units.values().any(|(_, linked)| *linked),
        check: units.values().any(|(rmeta, linked)| *rmeta && !*linked),
    }
}

/// The extra `cargo build` target-selection flags this profile needs on top of
/// a plain `cargo build`, in a stable order, or `None` when the fingerprints
/// could not be read and nothing can be concluded.
///
/// A fingerprint dir holds one file per unit -- `lib-<name>`, `bin-<name>`,
/// `example-<name>`, `bench-<name>`, plus the test-mode variants `test-lib-…`,
/// `test-bin-…`, `test-integration-test-…`, `test-bench-…`. So the tree already
/// records which kinds a build here has ever produced, and `--all-targets` is
/// only ever *wider* than that.
///
/// Narrowing is safe in the one direction that matters: a kind with no
/// fingerprint has no units in `deps/` for a sweep to delete, so leaving its
/// flag off cannot leave anything live unmarked -- it only skips compiling
/// something that was never here. `--all-targets` on a tree that has never
/// built its benches is a cold compile of every bench and dev-dependency, paid
/// to mark nothing.
///
/// Nothing here is matched against the workspace's declared targets, because
/// nothing needs to be: cargo never builds a *dependency's* tests, examples or
/// benches, so one of those prefixes anywhere in the profile can only belong to
/// a workspace target. Lib and bin are the exception -- every registry crate
/// leaves a `lib-*` fingerprint -- which is why they are not flags here at all.
/// A plain `cargo build` already selects every lib and bin of every selected
/// package, and unlike `--lib` it does not fail on a workspace member that
/// happens to have no library target.
pub fn built_kinds(profile_dir: &Path) -> Option<Vec<&'static str>> {
    let mut names: HashSet<String> = HashSet::new();
    let rd = fs::read_dir(profile_dir.join(".fingerprint")).ok()?;
    for e in rd.flatten() {
        let Ok(inner) = fs::read_dir(e.path()) else { continue };
        for f in inner.flatten() {
            let n = f.file_name().to_string_lossy().into_owned();
            let n = n.strip_suffix(".json").unwrap_or(&n);
            let n = n.strip_prefix("dep-").or_else(|| n.strip_prefix("output-")).unwrap_or(n);
            names.insert(n.to_string());
        }
    }
    // `test-bench-<name>` is a bench target compiled in test mode, so it is a
    // test unit and not proof that `cargo bench` ever ran -- the `test-` arm
    // matching first is what keeps those apart.
    Some(
        [("test-", "--tests"), ("example-", "--examples"), ("bench-", "--benches")]
            .into_iter()
            .filter(|(prefix, _)| names.iter().any(|n| n.starts_with(prefix)))
            .map(|(_, flag)| flag)
            .collect(),
    )
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
    /// `tag` keeps concurrently-running tests off each other's tree: these
    /// fixtures rebuild from scratch, so two tests sharing a path race between
    /// one's remove_dir_all and the other's create_dir_all.
    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("cargo-target-gc-test-{}-{tag}", std::process::id()))
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
        let root = fixture("examples");
        let units = scan_units(&root);
        // the example's fingerprint joins to examples/, so it is not orphaned
        assert!(units.deps.contains_key("ea73957bd44e0bd0"));
        assert!(units.fps.contains_key("ea73957bd44e0bd0"));
    }

    /// A profile whose fingerprints say: a bin and its tests were built, a
    /// registry dep contributed a lib, and no bench or example ever ran.
    fn kinds_fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("cargo-target-gc-kinds-{}-{tag}", std::process::id()))
            .join("debug");
        let _ = fs::remove_dir_all(&root);
        for (dir, files) in [
            ("serde-1111111111111111", &["lib-serde", "lib-serde.json"][..]),
            ("wapp-2222222222222222", &["bin-wapp"][..]),
            ("wapp-3333333333333333", &["test-bin-wapp"][..]),
            ("wapp-4444444444444444", &["test-integration-test-it"][..]),
            // a bench target built in test mode -- a test unit, not a bench run
            ("wapp-5555555555555555", &["test-bench-throughput"][..]),
        ] {
            fs::create_dir_all(root.join(".fingerprint").join(dir)).unwrap();
            for f in files {
                fs::write(root.join(".fingerprint").join(dir).join(f), b"x").unwrap();
            }
        }
        root
    }

    #[test]
    fn built_kinds_are_only_what_was_built() {
        let root = kinds_fixture("built");
        let kinds = built_kinds(&root).unwrap();
        // lib and bin are covered by a plain `cargo build`, never by a flag:
        // `--lib` fails outright on a workspace member with no library target.
        assert_eq!(kinds, vec!["--tests"], "{kinds:?}");
    }

    #[test]
    fn a_bench_run_is_distinguished_from_a_bench_built_for_test() {
        let root = kinds_fixture("bench");
        fs::create_dir_all(root.join(".fingerprint/wapp-6666666666666666")).unwrap();
        fs::write(root.join(".fingerprint/wapp-6666666666666666/bench-throughput"), b"x").unwrap();
        assert_eq!(built_kinds(&root).unwrap(), vec!["--tests", "--benches"]);
    }

    #[test]
    fn unreadable_fingerprints_conclude_nothing() {
        assert_eq!(built_kinds(Path::new("/nonexistent-target/debug")), None);
    }

    #[test]
    fn cdylib_units_are_unprovable() {
        let root = fixture("cdylib");
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

#[cfg(test)]
mod mode_tests {
    use super::*;

    /// deps/ for a workspace whose own crates were only ever `cargo check`ed:
    /// its lib is rmeta-only. Build-script dependencies are still compiled for
    /// real by a check -- they have to run -- so rlibs exist here regardless,
    /// which is why the question is asked about *this workspace's* targets and
    /// not about the profile as a whole.
    fn deps_fixture(tag: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("cargo-target-gc-modes-{}-{tag}", std::process::id()))
            .join("debug");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("deps")).unwrap();
        for f in files {
            fs::write(root.join("deps").join(f), b"x").unwrap();
        }
        root
    }

    const WS: [(&str, bool); 2] = [("wapp", true), ("wapp", false)];

    #[test]
    fn a_checked_workspace_is_not_a_built_one() {
        let root = deps_fixture(
            "checked",
            &[
                "libwapp-1111111111111111.rmeta",
                "wapp-1111111111111111.d",
                // a build script's own dependency, compiled for real by a check
                "libautocfg-2222222222222222.rlib",
                "libautocfg-2222222222222222.rmeta",
            ],
        );
        assert_eq!(built_modes(&root, &WS), Modes { build: false, check: true });
    }

    #[test]
    fn a_built_workspace_reports_a_build() {
        let root = deps_fixture(
            "built",
            &["libwapp-1111111111111111.rlib", "libwapp-1111111111111111.rmeta"],
        );
        assert_eq!(built_modes(&root, &WS), Modes { build: true, check: false });
    }

    #[test]
    fn a_bin_target_carries_no_extension() {
        let root = deps_fixture("bin", &["wapp-3333333333333333", "wapp-3333333333333333.d"]);
        assert_eq!(built_modes(&root, &WS), Modes { build: true, check: false });
    }

    /// Both, which is the normal state of a tree with an editor on it.
    #[test]
    fn a_tree_can_hold_both_modes() {
        let root = deps_fixture(
            "both",
            &[
                "libwapp-1111111111111111.rlib",
                "libwapp-1111111111111111.rmeta",
                "libwapp-4444444444444444.rmeta",
            ],
        );
        assert_eq!(built_modes(&root, &WS), Modes { build: true, check: true });
    }

    /// Nothing of ours here at all: concluding "never built" would skip the
    /// default build on a tree that simply could not be read.
    #[test]
    fn no_workspace_units_concludes_nothing() {
        let root = deps_fixture("empty", &["libserde-5555555555555555.rlib"]);
        assert_eq!(built_modes(&root, &WS), Modes { build: false, check: false });
    }
}
