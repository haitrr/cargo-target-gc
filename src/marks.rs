//! The tier B oracle: ask cargo which units a real build resolves to.
//!
//! Sweeping by mtime (the way `cargo-sweep` does) is not merely imprecise here,
//! it is inverted. Cargo does NOT touch a unit's files or its
//! `.fingerprint/*/invoked.timestamp` when the unit comes back Fresh, so a
//! unit's mtime records when it was last COMPILED, not when it was last USED --
//! and the units you rebuild least are exactly your stable ones. A cutoff sweep
//! deletes the live set and keeps the garbage. So `mark` runs your real build
//! commands with `--message-format=json` and records the live unit hashes;
//! `--sweep` then deletes every hash no mark covers.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::scan::{find_profile_dirs, stem, unit_hash, LIB_EXTS};

pub const MARK_FILE: &str = ".gc-live.json";

#[derive(Serialize, Deserialize, Default)]
pub struct MarkFile {
    pub version: u32,
    /// unit hash -> unix time it was last seen live
    pub hashes: HashMap<String, i64>,
}

#[derive(Default)]
pub struct Marks(HashMap<String, i64>);

impl Marks {
    pub fn contains(&self, h: &str) -> bool {
        self.0.contains_key(h)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn mark_path(target_dir: &Path) -> PathBuf {
    target_dir.join(MARK_FILE)
}

pub fn load(target_dir: &Path) -> Marks {
    let Ok(text) = fs::read_to_string(mark_path(target_dir)) else {
        return Marks::default();
    };
    match serde_json::from_str::<MarkFile>(&text) {
        Ok(m) => Marks(m.hashes),
        Err(_) => Marks::default(),
    }
}

fn save(target_dir: &Path, marks: &Marks) -> Result<()> {
    let path = mark_path(target_dir);
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string(&MarkFile {
        version: 1,
        hashes: marks.0.clone(),
    })?;
    fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Everything in every `deps/` dir, indexed the two ways an *uplifted*
/// artifact can be resolved back to its unit hash.
///
/// A bin or test unit is reported by cargo under its uplifted name
/// (`target/debug/myapp`, not `deps/myapp-<hash>`), so the hash has to be
/// recovered. Two routes, because neither alone is reliable:
///
///   - by inode, when cargo uplifted by hardlink;
///   - by name, when it uplifted by *copy* -- which is what actually happens on
///     macOS/APFS with cargo 1.9x: `target/debug/wapp` and
///     `deps/wapp-<hash>` were observed with different inodes and nlink 1 each.
///     `deps/<base>-<hash><ext>` uplifts to `<base><ext>`, so the base name and
///     extension are the key; size and mtime break ties between variants.
///
/// Getting this wrong is not cosmetic: an unmarked live unit is exactly what
/// `--sweep` deletes, so an unresolved artifact is reported as a warning rather
/// than passed over.
struct Cand {
    hash: String,
    size: u64,
    mtime: i64,
}

#[derive(Default)]
struct DepsIndex {
    by_ino: HashMap<u64, String>,
    by_uplift: HashMap<(String, String), Vec<Cand>>,
    /// (output kind, target name) -> the fingerprints that claim it. The last
    /// resort, and the only route for a unit whose artifacts carry no hash at
    /// all (`cdylib`/`staticlib`); see scan::hashless_lib_targets.
    by_fingerprint: HashMap<(String, String), Vec<Cand>>,
}

/// Split a file name into the part before the first dot and the rest
/// (`libfoo-<h>.rlib` -> `libfoo-<h>` + `.rlib`).
fn split_ext(name: &str) -> (&str, &str) {
    match name.find('.') {
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    }
}

fn build_index(target_dir: &Path) -> DepsIndex {
    let mut idx = DepsIndex::default();
    for profile in find_profile_dirs(target_dir) {
        for sub in ["deps", "examples"] {
            let Ok(rd) = fs::read_dir(profile.join(sub)) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let (st, ext) = split_ext(&name);
                let Some(h) = unit_hash(st) else { continue };
                let Some(m) = crate::fsutil::lmeta(&e.path()) else { continue };
                if m.nlink > 1 {
                    idx.by_ino.insert(m.ino, h.to_string());
                }
                let base = st[..st.len() - 17].to_string();
                idx.by_uplift
                    .entry((base, ext.to_string()))
                    .or_default()
                    .push(Cand { hash: h.to_string(), size: m.size, mtime: m.mtime });
            }
        }
        let Ok(rd) = fs::read_dir(profile.join(".fingerprint")) else { continue };
        for e in rd.flatten() {
            let dirname = e.file_name().to_string_lossy().into_owned();
            let Some(h) = unit_hash(&dirname) else { continue };
            let Ok(inner) = fs::read_dir(e.path()) else { continue };
            for f in inner.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if fname.contains('.') || fname.starts_with("dep-") || fname.starts_with("output-") {
                    continue;
                }
                let Some((kind, target)) = fname.split_once('-') else { continue };
                let mtime = crate::fsutil::lmeta(&f.path()).map(|m| m.mtime).unwrap_or(0);
                idx.by_fingerprint
                    .entry((kind.to_string(), target.to_string()))
                    .or_default()
                    .push(Cand { hash: h.to_string(), size: 0, mtime });
            }
        }
    }
    idx
}

/// The (kind, target) a bare artifact name implies: `libfoo.rlib` is the `lib`
/// output of target `foo`, `myapp` is the `bin` output of target `myapp`.
fn kind_and_targets(name: &str) -> (&'static str, Vec<String>) {
    let (st, ext) = split_ext(name);
    let ext = ext.trim_start_matches('.');
    if LIB_EXTS.contains(&ext) {
        let mut names = vec![st.to_string()];
        if let Some(rest) = st.strip_prefix("lib") {
            names.push(rest.to_string());
        }
        ("lib", names)
    } else {
        ("bin", vec![st.to_string()])
    }
}

impl DepsIndex {
    /// Unit hashes an artifact path belongs to. More than one only when the
    /// uplift is ambiguous, in which case marking every candidate is the safe
    /// direction: keeping a dead variant costs disk, dropping a live one costs
    /// a recompile you did not ask for.
    fn resolve(&self, path: &str) -> Vec<String> {
        let p = Path::new(path);
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return Vec::new();
        };
        if let Some(h) = unit_hash(stem(&name)) {
            return vec![h.to_string()];
        }
        // A build script's OUT_DIR is `build/<pkg>-<hash>/out`: the hash is on
        // the parent directory.
        if let Some(parent) = p.parent().and_then(|d| d.file_name()) {
            if let Some(h) = unit_hash(&parent.to_string_lossy()) {
                return vec![h.to_string()];
            }
        }
        let m = crate::fsutil::lmeta(p);
        if let Some(m) = m.as_ref() {
            if let Some(h) = self.by_ino.get(&m.ino) {
                return vec![h.clone()];
            }
        }
        let (base, ext) = split_ext(&name);
        if let Some(cands) = self.by_uplift.get(&(base.to_string(), ext.to_string())) {
            if cands.len() == 1 {
                return vec![cands[0].hash.clone()];
            }
            if let Some(m) = m.as_ref() {
                let same_size: Vec<&Cand> = cands.iter().filter(|c| c.size == m.size).collect();
                if same_size.len() == 1 {
                    return vec![same_size[0].hash.clone()];
                }
                let exact: Vec<&Cand> =
                    same_size.iter().copied().filter(|c| c.mtime == m.mtime).collect();
                if exact.len() == 1 {
                    return vec![exact[0].hash.clone()];
                }
            }
            return cands.iter().map(|c| c.hash.clone()).collect();
        }
        // Nothing about the file name carries a hash -- a cdylib or staticlib
        // unit. Ask the fingerprints which unit claims this output kind and
        // target, and let the compile time break ties between variants.
        let (kind, targets) = kind_and_targets(&name);
        let cands: Vec<&Cand> = targets
            .iter()
            .filter_map(|t| self.by_fingerprint.get(&(kind.to_string(), t.clone())))
            .flatten()
            .collect();
        if cands.len() > 1 {
            if let Some(m) = m.as_ref() {
                let near: Vec<&&Cand> =
                    cands.iter().filter(|c| (c.mtime - m.mtime).abs() <= 5).collect();
                if !near.is_empty() {
                    return near.iter().map(|c| c.hash.clone()).collect();
                }
            }
        }
        cands.iter().map(|c| c.hash.clone()).collect()
    }
}

/// Run each command with `--message-format=json` and record the unit hashes it
/// resolves to.
///
/// Every command here is one you named: there is no default build, because
/// nothing in a target dir records which command filled it, and a wrong guess
/// is a cold compile of a config whose units were never there. So a cold
/// command is treated as a mistake -- see `cold_foreign_compile` -- and
/// `allow_cold` is what it takes to sit through one deliberately.
pub fn mark(
    target_dir: &Path,
    commands: &[Vec<String>],
    allow_cold: bool,
    quiet: bool,
) -> Result<Marks> {
    if !quiet {
        eprintln!("  index: reading unit hashes from {}", target_dir.display());
    }
    let idx = build_index(target_dir);
    // Each run records the live set afresh: an accumulated mark from last week
    // keeps units that are dead today, which is the whole problem being solved.
    let mut marks = Marks::default();
    let now = crate::collect::now_secs();

    for cmd in commands {
        let (exe, rest) = cmd.split_first().context("empty mark command")?;
        if !quiet {
            eprintln!("  mark: {} (compiles if this config is cold)", cmd.join(" "));
        }
        // Cargo's own progress goes straight through: on a cold tree this
        // command compiles for minutes, and swallowing its stderr is the
        // difference between "it is building" and "it has hung".
        let mut child = Command::new(exe)
            .args(rest)
            .arg("--message-format=json")
            .stdout(Stdio::piped())
            .stderr(if quiet { Stdio::null() } else { Stdio::inherit() })
            .spawn()
            .with_context(|| format!("running `{}`", cmd.join(" ")))?;

        let mut found = 0usize;
        let mut unresolved: Vec<String> = Vec::new();
        let mut cold: Option<String> = None;
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                if !allow_cold {
                    if let Some(pkg) = cold_foreign_compile(&msg) {
                        cold = Some(pkg);
                        break;
                    }
                }
                let mut paths: Vec<String> = Vec::new();
                match msg.get("reason").and_then(|r| r.as_str()) {
                    Some("compiler-artifact") => {
                        if let Some(files) = msg.get("filenames").and_then(|f| f.as_array()) {
                            paths.extend(files.iter().filter_map(|f| f.as_str()).map(String::from));
                        }
                        if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
                            paths.push(exe.to_string());
                        }
                    }
                    // A build script's OUT_DIR never appears as a
                    // compiler-artifact; it arrives here instead.
                    Some("build-script-executed") => {
                        if let Some(d) = msg.get("out_dir").and_then(|d| d.as_str()) {
                            paths.push(d.to_string());
                        }
                    }
                    _ => {}
                }
                for p in paths {
                    let hashes = idx.resolve(&p);
                    if hashes.is_empty() {
                        unresolved.push(p);
                        continue;
                    }
                    for h in hashes {
                        marks.0.insert(h, now);
                    }
                    found += 1;
                }
            }
        }
        if let Some(pkg) = cold {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "`{}` is cold in this tree — it started compiling {pkg}, a registry \
                 dependency, so this is not a build this target/ was made by. Nothing was \
                 deleted.\n  Name the builds you actually run (check your `[alias]`es and \
                 what your editor runs), e.g.\n    cargo target-gc --build 'cargo build \
                 --no-default-features -p foo'\n  or pass --allow-cold to sit through the \
                 cold build and mark it anyway.",
                cmd.join(" ")
            );
        }
        let status = child.wait()?;
        if !status.success() {
            // Sweeping on a partial live set deletes units that are in use, so a
            // failed build has to stop the run rather than narrow it.
            anyhow::bail!(
                "`{}` exited {} — not deleting anything, since its live set is incomplete",
                cmd.join(" "),
                status.code().map(|c| c.to_string()).unwrap_or_else(|| "by signal".into())
            );
        }
        if !quiet {
            eprintln!("         {found} live artifacts");
        }
        // An artifact with no hash is a unit --sweep would delete while it is
        // still in use, so it is loud rather than silent.
        if !unresolved.is_empty() {
            eprintln!(
                "  warning: {} build artifact(s) could not be mapped to a unit hash; \
                 do NOT --sweep until this is understood (first: {})",
                unresolved.len(),
                unresolved[0]
            );
        }
    }

    save(target_dir, &marks)?;
    Ok(marks)
}

/// The package name when this message is a registry dependency that cargo just
/// compiled, rather than found fresh.
///
/// This is the signal that a mark command is not the build this tree was made
/// by. A crate you wrote compiling is the normal case — you edited it since the
/// last build, and marking exists to pick that up. A crate out of the registry
/// or a git checkout compiling is not: those are settled by construction, so one
/// going cold means this command resolved a different unit graph (other
/// features, other RUSTFLAGS, another `--target`) than anything in `deps/`.
/// Continuing would be a full cold build paid to mark units that were never
/// here.
///
/// The test is the cargo cache path rather than "outside the workspace root",
/// because a path dependency in a sibling directory is yours too — you edit it,
/// it recompiles, and that must not read as a wrong config.
pub fn cold_foreign_compile(msg: &serde_json::Value) -> Option<String> {
    if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
        return None;
    }
    if msg.get("fresh").and_then(|f| f.as_bool()) != Some(false) {
        return None;
    }
    let manifest = msg.get("manifest_path").and_then(|m| m.as_str())?;
    if !is_cached_dep(manifest) {
        return None;
    }
    let id = msg.get("package_id").and_then(|p| p.as_str()).unwrap_or("");
    Some(pkg_label(id, manifest))
}

/// True for a manifest under cargo's own caches: `$CARGO_HOME/registry/src/…`
/// or `$CARGO_HOME/git/checkouts/…`, wherever CARGO_HOME points.
fn is_cached_dep(manifest: &str) -> bool {
    let p = manifest.replace('\\', "/");
    p.contains("/registry/src/") || p.contains("/git/checkouts/")
}

/// A package id as a person writes it. Cargo has two spellings:
/// `registry+<url>#proc-macro2@1.0.106` today, `proc-macro2 1.0.106 (registry+…)`
/// before that; the manifest's directory is the fallback for anything else.
fn pkg_label(id: &str, manifest: &str) -> String {
    if let Some(tail) = id.rsplit('#').next() {
        if let Some((name, _ver)) = tail.rsplit_once('@') {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    if let Some(first) = id.split_whitespace().next() {
        if !first.is_empty() && !first.contains("://") {
            return first.to_string();
        }
    }
    Path::new(manifest)
        .parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a dependency".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(id: &str, manifest: &str, fresh: bool) -> serde_json::Value {
        serde_json::json!({
            "reason": "compiler-artifact",
            "package_id": id,
            "manifest_path": manifest,
            "fresh": fresh,
        })
    }

    const REG: &str = "/home/u/.cargo/registry/src/index.crates.io-1949/proc-macro2-1.0.106/Cargo.toml";

    #[test]
    fn a_cold_registry_dep_means_the_command_is_the_wrong_one() {
        let id = "registry+https://github.com/rust-lang/crates.io-index#proc-macro2@1.0.106";
        assert_eq!(cold_foreign_compile(&artifact(id, REG, false)), Some("proc-macro2".into()));
        // the pre-2024 spelling of the same id
        let old = "proc-macro2 1.0.106 (registry+https://github.com/rust-lang/crates.io-index)";
        assert_eq!(cold_foreign_compile(&artifact(old, REG, false)), Some("proc-macro2".into()));
    }

    #[test]
    fn a_fresh_dep_or_an_edited_crate_of_yours_is_not_a_cold_config() {
        let id = "registry+https://github.com/rust-lang/crates.io-index#proc-macro2@1.0.106";
        assert_eq!(cold_foreign_compile(&artifact(id, REG, true)), None);
        // you edited it since the last build; marking exists to pick that up
        let mine = "path+file:///ws/crates/myapp#0.1.0";
        assert_eq!(cold_foreign_compile(&artifact(mine, "/ws/crates/myapp/Cargo.toml", false)), None);
        // a path dependency you keep next door is yours as well
        let nb = "path+file:///elsewhere/shared#0.1.0";
        assert_eq!(cold_foreign_compile(&artifact(nb, "/elsewhere/shared/Cargo.toml", false)), None);
    }
}
