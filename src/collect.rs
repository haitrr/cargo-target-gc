//! Classifying everything under a profile dir into tiers by what deleting it
//! costs you.
//!
//!   A. Provably dead -- no rebuild follows.
//!   B. Unreferenced unit variants -- costs a recompile of any config you did
//!      not `mark`.
//!   C. `incremental/` -- costs the first-edit penalty, per crate, once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::fsutil::{newest_child_mtime, walk_stat, Reclaim};
use crate::lockfile::{self, Locked};
use crate::marks::Marks;
use crate::scan::{find_profile_dirs, fingerprint_is_hashless_lib, hashless_lib_targets, scan_units};

/// Directories directly under `target/` that hold no unit artifacts and are
/// regenerated on demand: rustdoc output, `cargo package` staging, and the
/// scratch dir cargo forgets to empty.
pub const SCRATCH_DIRS: [&str; 3] = ["tmp", "package", "doc"];

/// A finalized rustc session dir inside `incremental/<crate>-<id>/` is
/// `s-<time>-<id>`; a `-working` suffix means it was never finalized -- either a
/// build is running right now (the `.cargo-lock` guard covers that) or one was
/// killed.
fn session_kind(name: &str) -> Option<bool> {
    let rest = name.strip_prefix("s-")?;
    let (a, b) = rest.split_once('-')?;
    let alnum = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !alnum(a) {
        return None;
    }
    match b.split_once('-') {
        Some((id, tail)) if alnum(id) => Some(tail == "working"),
        None if alnum(b) => Some(false),
        _ => None,
    }
}

/// The crate a rustc incremental dir belongs to: `oxy_app-1a2b3c4d5e6f7` minus
/// the base-36 disambiguator rustc derives from the unit's `-C metadata`.
fn incremental_crate_name(dir: &str) -> &str {
    match dir.rfind('-') {
        Some(i) => {
            let suffix = &dir[i + 1..];
            let disambiguator = suffix.len() >= 10
                && suffix.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
            if disambiguator { &dir[..i] } else { dir }
        }
        None => dir,
    }
}

/// Paths already taken by a cheaper category, so nothing is counted twice.
#[derive(Default)]
pub struct Claims(HashSet<PathBuf>);

impl Claims {
    pub fn covered(&self, p: &Path) -> bool {
        let mut cur = Some(p);
        while let Some(c) = cur {
            if self.0.contains(c) {
                return true;
            }
            cur = c.parent();
        }
        false
    }
    fn claim(&mut self, p: &Path) {
        self.0.insert(p.to_path_buf());
    }
}

pub struct Category {
    pub key: &'static str,
    pub tier: char,
    /// Sort key: cheaper tiers are collected first, including under `--budget`.
    pub cost: f64,
    pub blurb: String,
    pub paths: Vec<PathBuf>,
    pub acct: Reclaim,
}

impl Category {
    fn new(key: &'static str, tier: char, cost: f64, blurb: impl Into<String>) -> Self {
        Category {
            key,
            tier,
            cost,
            blurb: blurb.into(),
            paths: Vec::new(),
            acct: Reclaim::default(),
        }
    }

    fn take(&mut self, path: &Path, claims: &mut Claims) {
        if claims.covered(path) {
            return;
        }
        walk_stat(path, &mut self.acct, &|p| claims.covered(p));
        claims.claim(path);
        self.paths.push(path.to_path_buf());
    }

    pub fn merge(&mut self, other: Category) {
        self.paths.extend(other.paths);
        self.acct.merge(&other.acct);
    }
}

pub struct Options<'a> {
    /// `None` = tier B off; `Some` = sweep against these marks.
    pub live: Option<&'a Marks>,
    /// How many incremental crate dirs to keep per crate; `None` keeps them all.
    pub keep_incremental: Option<usize>,
    pub locked: Option<&'a Locked>,
}

pub fn collect(profile_dir: &Path, target_dir: &Path, opts: &Options, claims: &mut Claims) -> Vec<Category> {
    let mut cats: Vec<Category> = Vec::new();
    let units = scan_units(profile_dir);
    // Units whose artifacts carry no hash cannot be joined to their fingerprint,
    // so nothing here can prove them dead. See scan::hashless_lib_targets.
    let hashless = hashless_lib_targets(profile_dir);
    let unprovable = |paths: &Vec<PathBuf>| {
        paths.iter().any(|p| fingerprint_is_hashless_lib(p, &hashless))
    };

    // --- tier A: nothing rebuilds because of these ---------------------------
    let mut orphan = Category::new(
        "orphan-units",
        'A',
        0.0,
        "artifacts with no fingerprint, or fingerprints with no artifacts",
    );
    for (h, paths) in &units.deps {
        if !units.fps.contains_key(h) {
            for p in paths {
                orphan.take(p, claims);
            }
        }
    }
    for (h, paths) in &units.fps {
        if !units.deps.contains_key(h) && !units.builds.contains_key(h) && !unprovable(paths) {
            for p in paths {
                orphan.take(p, claims);
            }
        }
    }
    for (h, paths) in &units.builds {
        if !units.fps.contains_key(h) {
            for p in paths {
                orphan.take(p, claims);
            }
        }
    }
    cats.push(orphan);

    if let Some(locked) = opts.locked {
        let mut stale = Category::new(
            "stale-dep-versions",
            'A',
            0.5,
            "dependency versions Cargo.lock no longer resolves to (recompiles \
             only if you check out a branch that pins them again)",
        );
        for (h, paths) in &units.deps {
            let dep_info: Vec<PathBuf> = paths
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "d"))
                .cloned()
                .collect();
            let Some(src) = lockfile::unit_source(&dep_info) else { continue };
            if !locked.is_stale(&src) {
                continue;
            }
            for p in paths {
                stale.take(p, claims);
            }
            for p in units.fps.get(h).into_iter().chain(units.builds.get(h)).flatten() {
                stale.take(p, claims);
            }
        }
        cats.push(stale);
    }

    let mut abandoned = Category::new(
        "abandoned-sessions",
        'A',
        0.0,
        "rustc sessions left unfinalized by a killed build",
    );
    let mut superseded = Category::new(
        "superseded-sessions",
        'A',
        0.0,
        "older incremental sessions rustc no longer reads",
    );
    let incr_root = profile_dir.join("incremental");
    let mut crate_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&incr_root) {
        let mut crates: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        crates.sort();
        for cdir in crates {
            crate_dirs.push(cdir.clone());
            let mut finalized: Vec<(i64, PathBuf)> = Vec::new();
            let Ok(sessions) = std::fs::read_dir(&cdir) else { continue };
            let mut sessions: Vec<PathBuf> = sessions.flatten().map(|e| e.path()).collect();
            sessions.sort();
            for spath in sessions {
                if !spath.is_dir() {
                    continue;
                }
                let name = spath.file_name().unwrap_or_default().to_string_lossy().into_owned();
                match session_kind(&name) {
                    Some(true) => abandoned.take(&spath, claims),
                    Some(false) => {
                        let mt = crate::fsutil::lmeta(&spath).map(|m| m.mtime).unwrap_or(0);
                        finalized.push((mt, spath));
                    }
                    None => {}
                }
            }
            // rustc reads only the newest finalized session; the rest are dead
            // weight it declined to collect.
            finalized.sort();
            if finalized.len() > 1 {
                for (_, spath) in &finalized[..finalized.len() - 1] {
                    superseded.take(spath, claims);
                }
            }
        }
    }
    cats.push(abandoned);
    cats.push(superseded);

    let mut dsym = Category::new("dsym", 'A', 0.0, "macOS .dSYM debug bundles (regenerated on link)");
    for base in [profile_dir.to_path_buf(), profile_dir.join("deps")] {
        if let Ok(rd) = std::fs::read_dir(&base) {
            let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            names.sort();
            for p in names {
                if p.extension().is_some_and(|e| e == "dSYM") {
                    dsym.take(&p, claims);
                }
            }
        }
    }
    cats.push(dsym);

    // --- tier B: costs a recompile of whatever config you did not mark -------
    if let Some(live) = opts.live {
        let mut unref = Category::new(
            "unreferenced-units",
            'B',
            1.0,
            format!("unit variants no mark covers ({} hashes marked live)", live.len()),
        );
        for (is_fps, table) in [(false, &units.deps), (true, &units.fps), (false, &units.builds)] {
            let mut hashes: Vec<&String> = table.keys().collect();
            hashes.sort();
            for h in hashes {
                if live.contains(h) || (is_fps && unprovable(&table[h])) {
                    continue;
                }
                for p in &table[h] {
                    unref.take(p, claims);
                }
            }
        }
        cats.push(unref);
    }

    // --- tier C: costs the first-edit penalty, per crate, once ---------------
    //
    // rustc garbage-collects sessions INSIDE a crate dir but never removes a
    // crate dir: the dir is keyed by `-C metadata`, so every new config or
    // revision of a crate starts a fresh one and orphans the old one forever.
    // That is unbounded growth by construction -- 959 dirs for 55 crates on the
    // tree this was measured against. Dropping one cannot cascade: cargo never
    // consults incremental state for freshness, so the cost lands only when you
    // next edit that crate, and only as one non-incremental compile of it.
    if let Some(keep) = opts.keep_incremental {
        let mut incr = Category::new(
            "incremental",
            'C',
            2.0,
            format!(
                "rustc incremental caches beyond the newest {keep} per crate \
                 (costs one non-incremental compile of a crate, when you next edit it)"
            ),
        );
        let mut by_crate: HashMap<String, Vec<(i64, PathBuf)>> = HashMap::new();
        for cdir in &crate_dirs {
            let name = cdir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            by_crate
                .entry(incremental_crate_name(&name).to_string())
                .or_default()
                .push((newest_child_mtime(cdir), cdir.clone()));
        }
        let mut names: Vec<&String> = by_crate.keys().collect();
        names.sort();
        for name in names {
            let mut dirs = by_crate[name].clone();
            if dirs.len() <= keep {
                continue;
            }
            dirs.sort();
            for (_, path) in &dirs[..dirs.len() - keep] {
                incr.take(path, claims);
            }
        }
        cats.push(incr);
    }

    let _ = target_dir;
    cats.retain(|c| !c.paths.is_empty());
    cats
}

/// `target/{tmp,package,doc}`, at the target root and beside every profile dir
/// (cross-compiled trees get their own `doc/`).
pub fn collect_scratch(target_dir: &Path, claims: &mut Claims) -> Option<Category> {
    let mut roots: Vec<PathBuf> = vec![target_dir.to_path_buf()];
    for p in find_profile_dirs(target_dir) {
        if let Some(parent) = p.parent() {
            if parent != target_dir && !roots.contains(&parent.to_path_buf()) {
                roots.push(parent.to_path_buf());
            }
        }
    }
    let mut scratch = Category::new("scratch", 'A', 0.0, "rustdoc / cargo-package / tmp output");
    for root in roots {
        for name in SCRATCH_DIRS {
            let p = root.join(name);
            if p.exists() {
                scratch.take(&p, claims);
            }
        }
    }
    (!scratch.paths.is_empty()).then_some(scratch)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::session_kind;

    #[test]
    fn incremental_names() {
        assert_eq!(super::incremental_crate_name("oxy_app-1a2b3c4d5e6f7"), "oxy_app");
        assert_eq!(super::incremental_crate_name("build_script_build-040ezm4u1l6ih"), "build_script_build");
        // not a disambiguator: too short, and a crate may legitimately end in one
        assert_eq!(super::incremental_crate_name("foo-bar"), "foo-bar");
    }

    #[test]
    fn sessions() {
        assert_eq!(session_kind("s-h5k2m3n4-abcdef"), Some(false));
        assert_eq!(session_kind("s-h5k2m3n4-abcdef-working"), Some(true));
        assert_eq!(session_kind("not-a-session"), None);
        assert_eq!(session_kind("s-only"), None);
    }
}
