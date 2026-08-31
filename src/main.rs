//! cargo-target-gc — reclaim disk from a Rust workspace's `target/` by rebuild
//! cost, instead of `cargo clean`'s all-or-nothing.
//!
//! `cargo clean` deletes the bulk of `target/` that is registry dependencies you
//! never edit, and you pay a full cold rebuild for them. What actually grows is
//! the part cargo never collects -- stale unit variants in `deps/` from
//! feature/profile/RUSTFLAGS configs you no longer build, abandoned rustc
//! sessions in `incremental/`, orphaned fingerprints. This walks `target/`,
//! sorts what it finds by what a rebuild would cost, and lets you delete by that
//! cost. Nothing under `target/` is source; every category here only ever costs
//! time.

mod collect;
mod fsutil;
mod lockfile;
mod marks;
mod report;
mod scan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Parser;

use collect::{Category, Claims, Options};
use fsutil::{human, walk_stat, Reclaim};

const AFTER_HELP: &str = "\
HOW IT DECIDES
  Everything under target/ is either reachable from a build you run, or it is
  garbage cargo will never collect. There is no timestamp that tells the two
  apart — cargo does not touch a unit when it comes back Fresh, so mtime records
  when a unit was last COMPILED, not when it was last USED. So this asks cargo
  directly: it runs your build (a no-op if the tree is warm), records the units
  that build resolves to, and deletes the rest.

  The default build is narrowed to what this tree can be shown to have built:
  the target kinds in .fingerprint/, and the compile modes in deps/ — a tree an
  editor only ever `cargo check`ed defaults to `cargo check`, not to a cold
  `cargo build`. Anything with no units here has nothing to sweep, so compiling
  it would mark nothing.

  Name every config you actually use, or the ones you leave out get deleted and
  cold-rebuild the next time you switch to them:

    cargo target-gc --apply --build 'cargo build' --build 'cargo clippy --all-targets'

  What no artifact records is which FEATURES built it, so that part cannot be
  narrowed — and it is where a default goes wrong on a workspace whose real loop
  is `--no-default-features -p foo`. So a mark command that starts compiling a
  REGISTRY dependency is taken as proof it resolves a unit graph this tree does
  not have: the run stops in seconds instead of sitting through the cold build,
  and names what to pass to --build. (--allow-cold overrides.) A crate of your
  own recompiling is just an edit, and is what marking is for.

WHAT IT DELETES
  unit variants     every unit no named build resolves to
  incremental/      all but the newest --keep-incremental dirs per crate; rustc
                    never removes these itself, so they grow without bound
  free leftovers    orphaned units, dead fingerprints, abandoned rustc sessions,
                    .dSYM, doc/tmp/package, and units built from dependency
                    versions Cargo.lock no longer resolves to

EXAMPLES
  cargo target-gc                       report what would go, delete nothing
  cargo target-gc --apply               report, then ask before deleting
  cargo target-gc --no-build            report without running any build
  cargo target-gc --apply --yes         delete without asking (scripts, CI)
  cargo target-gc --budget 20G --apply  free cheapest-first until target/ fits
";

#[derive(Parser)]
#[command(
    name = "cargo-target-gc",
    bin_name = "cargo target-gc",
    version,
    about = "Reclaim target/ disk by rebuild cost, instead of cargo clean's all-or-nothing.",
    after_long_help = AFTER_HELP,
    after_help = AFTER_HELP
)]
struct Cli {
    /// Target dir to collect (default: `cargo metadata`, else $CARGO_TARGET_DIR, else ./target)
    #[arg(long, value_name = "DIR")]
    target_dir: Option<PathBuf>,

    /// Workspace manifest to locate the target dir from
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,

    /// Actually delete; without it this only reports
    #[arg(long)]
    apply: bool,

    /// A build whose units to keep, repeatable [default: the compile modes and
    /// target kinds this tree has already built]
    #[arg(long, value_name = "CMD")]
    build: Vec<String>,

    /// Do not run any build: report and collect only what needs no live set
    #[arg(long, conflicts_with = "build")]
    no_build: bool,

    /// Let a mark command compile a registry dependency from cold instead of
    /// stopping (it means that command is not a build this tree was made by)
    #[arg(long)]
    allow_cold: bool,

    /// Incremental cache dirs to keep per crate, or `all` to keep every one
    #[arg(long, value_name = "N|all", default_value = "2")]
    keep_incremental: String,

    /// Collect cheapest-first only until target/ fits in SIZE (e.g. 20G)
    #[arg(long, value_name = "SIZE")]
    budget: Option<String>,

    /// Delete without the confirmation prompt (required for --apply off a terminal)
    #[arg(long, short = 'y')]
    yes: bool,

    /// Machine-readable summary
    #[arg(long)]
    json: bool,
}

fn main() {
    // Invoked as `cargo target-gc ...`, argv[1] is the subcommand name.
    let argv: Vec<String> = std::env::args()
        .enumerate()
        .filter(|(i, a)| !(*i == 1 && a == "target-gc"))
        .map(|(_, a)| a)
        .collect();

    if let Err(e) = run(Cli::parse_from(argv)) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // One `cargo metadata`: it answers both where the target dir is (honouring
    // CARGO_TARGET_DIR, build.target-dir and workspace inheritance) and which
    // targets are this workspace's own, which is what tells our artifacts in
    // deps/ from a dependency's.
    let meta = cargo_metadata(&cli);
    let target_dir = resolve_target_dir(&cli, meta.as_ref())?;
    if !target_dir.is_dir() {
        bail!("no target dir at {}", target_dir.display());
    }

    let profiles = scan::find_profile_dirs(&target_dir);
    for p in &profiles {
        if scan::build_lock_held(p) {
            bail!(
                "a cargo build is running ({} is locked) — wait for it to finish",
                rel(p, &target_dir)
            );
        }
    }

    let keep_incremental = match cli.keep_incremental.as_str() {
        "all" => None,
        n => Some(
            n.parse::<usize>()
                .map_err(|_| anyhow::anyhow!("--keep-incremental takes a number or `all`"))?,
        ),
    };

    // Ask cargo what is live -- but only ever with a command you named. A
    // target dir does not record which build filled it, so there is no default
    // that is not a guess, and a guess that misses (other features, a `-p`
    // narrowing, another front end) is a cold compile of units this tree never
    // had. Without --build, the run is a report plus whatever needs no live set.
    let live = if cli.no_build {
        let stored = marks::load(&target_dir);
        if stored.is_empty() {
            if !cli.json {
                eprintln!(
                    "note: --no-build and no live set on record, so unit variants are left \
                     alone; run without --no-build to collect them"
                );
            }
            None
        } else {
            if !cli.json {
                eprintln!(
                    "note: --no-build — reusing the live set from the last run ({} units); \
                     anything built since is at risk",
                    stored.len()
                );
            }
            Some(stored)
        }
    } else {
        let cmds: Vec<Vec<String>> = if cli.build.is_empty() {
            let ws = workspace_targets(meta.as_ref());
            let ws: Vec<(&str, bool)> = ws.iter().map(|(n, l)| (n.as_str(), *l)).collect();
            default_commands(&profiles, &ws, cli.json)
        } else {
            cli.build.iter().map(|c| c.split_whitespace().map(String::from).collect()).collect()
        };
        Some(marks::mark(&target_dir, &cmds, cli.allow_cold, cli.json)?)
    };

    let lockpath = lockfile::find_lockfile(&target_dir);
    let locked = lockpath.as_deref().and_then(lockfile::read_lockfile);
    if locked.is_none() && !cli.json {
        eprintln!(
            "note: no readable Cargo.lock above this target dir — skipping the stale \
             dependency-version check (expected under a shared CARGO_TARGET_DIR)"
        );
    }

    let opts = Options {
        live: live.as_ref(),
        keep_incremental,
        locked: locked.as_ref(),
    };

    // Measuring is a full walk of target/, which on a large tree is seconds of
    // silence right after the build. Say what is happening.
    if !cli.json {
        eprintln!("  scan: measuring {}", target_dir.display());
    }

    let mut claims = Claims::default();
    let mut cats: Vec<Category> = Vec::new();
    for p in &profiles {
        cats.extend(collect::collect(p, &target_dir, &opts, &mut claims));
        if let Some(live) = opts.live {
            // A profile with no marks at all is about to be collected whole.
            // That is legitimate — a stale release/ tree in a workspace you only
            // build in debug is exactly what you want gone — but it is never
            // what you want to discover afterwards, so say it before the report.
            let units = scan::scan_units(p);
            if !units.deps.is_empty() && !units.deps.keys().any(|h| live.contains(h)) {
                eprintln!(
                    "note: no mark covers any unit in {}/ — the whole profile will be \
                     collected. If you use it, mark that build first.",
                    rel(p, &target_dir)
                );
            }
        }
    }
    if let Some(scratch) = collect::collect_scratch(&target_dir, &mut claims) {
        cats.push(scratch);
    }

    // Fold same-key categories from debug/, release/ and cross-compiled trees
    // into one line. Claims guarantee no path lands in two of them, so the
    // hardlink accounts merge exactly.
    let mut order: Vec<&'static str> = Vec::new();
    let mut merged: HashMap<&'static str, Category> = HashMap::new();
    for c in cats {
        match merged.get_mut(c.key) {
            Some(existing) => existing.merge(c),
            None => {
                order.push(c.key);
                merged.insert(c.key, c);
            }
        }
    }
    let mut cats: Vec<Category> = order.into_iter().filter_map(|k| merged.remove(k)).collect();

    let mut budget_note = None;
    if let Some(budget) = &cli.budget {
        let want = parse_size(budget)?;
        let mut acct = Reclaim::default();
        walk_stat(&target_dir, &mut acct, &|_| false);
        let size = acct.reclaim() + acct.shared();
        if size <= want {
            budget_note = Some(format!(
                "target/ is {}, already under {budget} — nothing to do.",
                human(size)
            ));
            cats.clear();
        } else {
            let need = size - want;
            cats.sort_by_key(report::sort_key);
            let mut got = 0u64;
            let mut kept = Vec::new();
            for c in cats {
                if got >= need {
                    break;
                }
                got += c.acct.reclaim();
                kept.push(c);
            }
            let mut more: Vec<&str> = Vec::new();
            if cli.no_build {
                more.push("a run without --no-build");
            }
            if keep_incremental.is_some_and(|k| k > 0) {
                more.push("--keep-incremental 0");
            }
            let short = if got >= need {
                String::new()
            } else if more.is_empty() {
                format!("— short by {}; only `cargo clean` goes further", human(need - got))
            } else {
                format!("— short by {}; add {}", human(need - got), more.join(" and "))
            };
            budget_note = Some(format!(
                "target/ is {}; freeing {} of the {} needed for {budget} {short}",
                human(size),
                human(got),
                human(need)
            ));
            cats = kept;
        }
    }

    // The report comes before the deletion, not after it: --apply is asking to
    // destroy work, and the only way to judge it is to have read the table.
    let mut apply = cli.apply && !cats.is_empty();
    if cli.json {
        if apply && !cli.yes {
            bail!("--apply --json cannot prompt; pass --yes to confirm the deletion");
        }
    } else {
        report::report(&cats, cli.apply, budget_note.as_deref());
        if apply && !cli.yes {
            apply = confirm(&cats)?;
        }
    }

    let freed = delete(&cats, &target_dir, apply)?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report::json_summary(&cats, &target_dir, apply))?);
    } else if apply && freed > 0 {
        println!("\nfreed {}", human(freed));
    }
    Ok(())
}

/// The workspace's own compilable targets, as (name, is_lib) -- the key that
/// tells one of *our* artifacts in `deps/` from a dependency's.
///
/// `cargo metadata --no-deps` lists exactly the workspace members, which is the
/// set a bare `cargo build` here would select.
fn workspace_targets(meta: Option<&serde_json::Value>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let Some(pkgs) = meta.and_then(|m| m.get("packages")).and_then(|p| p.as_array()) else {
        return out;
    };
    for p in pkgs {
        for t in p.get("targets").and_then(|t| t.as_array()).into_iter().flatten() {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else { continue };
            let kinds: Vec<&str> = t
                .get("kind")
                .and_then(|k| k.as_array())
                .map(|a| a.iter().filter_map(|k| k.as_str()).collect())
                .unwrap_or_default();
            if kinds.iter().any(|k| {
                matches!(*k, "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
            }) {
                out.push((name.to_string(), true));
            }
            if kinds.iter().any(|k| *k == "bin") {
                out.push((name.to_string(), false));
            }
        }
    }
    out
}

/// The default mark commands: the compile modes and target kinds this tree can
/// be shown to have built, and nothing beyond them.
///
/// Every command here has units in `deps/` waiting for it, so on a settled tree
/// it is a freshness check. Widening it is what costs: `cargo build` on a tree
/// only ever checked, or `--all-targets` on one that never benched, is a cold
/// compile of a whole config paid to mark units that were never here.
///
/// Narrowing is safe in the direction that matters -- a mode or kind with no
/// units has nothing for a sweep to delete -- with one exception: it cannot
/// narrow past what the *features* were, which no artifact records. That is
/// what mark's cold check is for.
fn default_commands(profiles: &[PathBuf], ws: &[(&str, bool)], quiet: bool) -> Vec<Vec<String>> {
    let read: Vec<Vec<&'static str>> = profiles.iter().filter_map(|p| scan::built_kinds(p)).collect();
    let mut kinds: Vec<&'static str> = Vec::new();
    for k in read.iter().flatten() {
        if !kinds.contains(k) {
            kinds.push(*k);
        }
    }
    if read.is_empty() {
        // Nothing could be read, so nothing can be ruled out.
        kinds = vec!["--all-targets"];
    } else if !quiet {
        let skipped: Vec<&str> = ["--tests", "--examples", "--benches"]
            .into_iter()
            .filter(|f| !kinds.contains(f))
            .collect();
        if !skipped.is_empty() {
            eprintln!(
                "note: this tree has never built {}, so the default marks skip them",
                skipped.join(" ")
            );
        }
    }

    let mut modes = scan::Modes::default();
    for p in profiles {
        let m = scan::built_modes(p, ws);
        modes.build |= m.build;
        modes.check |= m.check;
    }
    // Neither seen means no unit of ours was found at all -- an unreadable or
    // foreign-shaped tree, where concluding "never built" would skip a build
    // whose units are here and get them swept.
    if !modes.build && !modes.check {
        modes.build = true;
    }

    // One command per selection rather than one command carrying them all,
    // because any target-selection flag *replaces* cargo's default selection:
    // `cargo build --tests` builds the test harnesses and not the lib and bins.
    // The bare run is what covers those, and it is the only spelling that does
    // so without `--lib`, which fails outright on a workspace member that has no
    // library target. Each extra run is a freshness check on a warm tree.
    let sel = |base: &str| -> Vec<Vec<String>> {
        let mut out = vec![vec!["cargo".into(), base.to_string()]];
        out.extend(kinds.iter().map(|f| vec!["cargo".into(), base.to_string(), f.to_string()]));
        out
    };
    let mut cmds = Vec::new();
    if modes.build {
        cmds.extend(sel("build"));
    }
    // Check units are separate units with their own hashes, so a build command
    // never resolves to them -- and on a tree an editor drives, they can be the
    // only units there are.
    if modes.check {
        cmds.extend(sel("check"));
    }
    if !quiet {
        match (modes.build, modes.check) {
            (true, true) => eprintln!("note: this tree has `cargo check` units, so they are marked too"),
            (false, true) => eprintln!(
                "note: this tree has only `cargo check` units — nothing here was ever built, \
                 so the default marks skip `cargo build`"
            ),
            _ => {}
        }
    }
    cmds
}

/// Ask before deleting. Off a terminal there is nobody to ask, and guessing
/// `yes` on a pipe is how a CI job silently cold-rebuilds, so that is an error
/// rather than a default.
fn confirm(cats: &[Category]) -> Result<bool> {
    use std::io::{BufRead, Write};

    let items: usize = cats.iter().map(|c| c.paths.len()).sum();
    let bytes: u64 = cats.iter().map(|c| c.acct.reclaim()).sum();
    if !stdin_is_tty() {
        bail!("--apply needs a terminal to confirm; pass --yes to delete without asking");
    }
    print!("\ndelete {items} path(s), freeing {}? [y/N] ", human(bytes));
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(true);
    }
    println!("nothing deleted.");
    Ok(false)
}

#[cfg(unix)]
fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdin_is_tty() -> bool {
    true
}

/// A profile dir named the way a person would say it: `debug`, or
/// `aarch64-apple-darwin/debug` when the target dir holds cross-compiled trees.
fn rel(p: &Path, target_dir: &Path) -> String {
    p.strip_prefix(target_dir).unwrap_or(p).to_string_lossy().into_owned()
}

/// Delete every claimed path, re-checking containment per path rather than
/// trusting it from the scan: a symlink or a race between scan and delete must
/// not let a recursive remove escape the target dir.
fn delete(cats: &[Category], target_dir: &Path, apply: bool) -> Result<u64> {
    let root = std::fs::canonicalize(target_dir)
        .with_context(|| format!("resolving {}", target_dir.display()))?;
    let mut freed = 0u64;
    for c in cats {
        for p in &c.paths {
            let Ok(real) = std::fs::canonicalize(p) else {
                continue; // already gone (a parent category took it first)
            };
            if real != root && !real.starts_with(&root) {
                eprintln!("  refusing to delete outside target/: {}", p.display());
                continue;
            }
            if !apply {
                continue;
            }
            let meta = std::fs::symlink_metadata(&real).ok();
            let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
            let is_link = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
            let res = if is_dir && !is_link {
                std::fs::remove_dir_all(&real)
            } else {
                std::fs::remove_file(&real)
            };
            if let Err(e) = res {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("  could not delete {}: {e}", p.display());
                }
            }
        }
        freed += c.acct.reclaim();
    }
    Ok(freed)
}

/// The target dir for any workspace: ask cargo, which honours `CARGO_TARGET_DIR`,
/// `build.target-dir` and workspace inheritance, and fall back to the plain
/// answers when cargo is unavailable.
fn cargo_metadata(cli: &Cli) -> Option<serde_json::Value> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    if let Some(m) = &cli.manifest_path {
        cmd.arg("--manifest-path").arg(m);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn resolve_target_dir(cli: &Cli, meta: Option<&serde_json::Value>) -> Result<PathBuf> {
    if let Some(d) = &cli.target_dir {
        return Ok(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()));
    }
    if let Some(d) = meta.and_then(|v| v.get("target_directory")).and_then(|d| d.as_str()) {
        return Ok(PathBuf::from(d));
    }
    if cli.manifest_path.is_some() {
        bail!("`cargo metadata` failed for the given --manifest-path");
    }
    let fallback = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    Ok(std::fs::canonicalize(&fallback).unwrap_or(fallback))
}

fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let digits: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let n: f64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("could not parse size {s:?} (try 20G, 500M)"))?;
    let suffix = t[digits.len()..].trim().trim_end_matches(['b', 'B']);
    let mult: f64 = match suffix.to_ascii_uppercase().as_str() {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1024f64.powi(2),
        "G" => 1024f64.powi(3),
        "T" => 1024f64.powi(4),
        _ => bail!("could not parse size {s:?} (try 20G, 500M)"),
    };
    Ok((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::{default_commands, parse_size};
    use crate::scan::{stem, unit_hash};
    use std::fs;
    use std::path::PathBuf;

    /// A profile dir holding one `deps/` listing and an (empty) `.fingerprint/`,
    /// so `built_kinds` reads it rather than concluding nothing.
    fn fixture(tag: &str, deps: &[&str]) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("cargo-target-gc-defaults-{}-{tag}", std::process::id()))
            .join("debug");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("deps")).unwrap();
        fs::create_dir_all(root.join(".fingerprint")).unwrap();
        for f in deps {
            fs::write(root.join("deps").join(f), b"x").unwrap();
        }
        root
    }

    /// The tree an editor made: `cargo check` and nothing else. Defaulting to
    /// `cargo build` here compiles the whole workspace from cold to mark units
    /// that do not exist -- and every check unit that does exist stays unmarked
    /// while it does so.
    #[test]
    fn a_checked_only_tree_defaults_to_checking() {
        let root = fixture("checked", &["libwapp-1111111111111111.rmeta", "wapp-1111111111111111.d"]);
        let cmds = default_commands(&[root], &[("wapp", true)], true);
        assert_eq!(cmds, vec![vec!["cargo", "check"]]);
    }

    #[test]
    fn a_built_tree_defaults_to_building_the_kinds_it_has() {
        let root = fixture("built", &["libwapp-1111111111111111.rlib"]);
        fs::create_dir_all(root.join(".fingerprint/wapp-2222222222222222")).unwrap();
        fs::write(root.join(".fingerprint/wapp-2222222222222222/test-bin-wapp"), b"x").unwrap();
        let cmds = default_commands(&[root], &[("wapp", true)], true);
        assert_eq!(cmds, vec![vec!["cargo", "build"], vec!["cargo", "build", "--tests"]]);
        // never a kind this tree has no units for
        assert!(!format!("{cmds:?}").contains("--benches"));
    }

    /// A tree whose units cannot be attributed to this workspace at all: the
    /// safe answer is the plain build, since skipping it would sweep whatever
    /// is live here.
    #[test]
    fn an_unreadable_tree_still_builds() {
        let root = fixture("foreign", &["libserde-5555555555555555.rlib"]);
        assert_eq!(default_commands(&[root], &[("wapp", true)], true), vec![vec!["cargo", "build"]]);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20G").unwrap(), 20 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("500m").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_size("1.5GB").unwrap(), 1610612736);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn hashes() {
        assert_eq!(unit_hash(stem("libfoo-0123456789abcdef.rlib")), Some("0123456789abcdef"));
        assert_eq!(unit_hash(stem("foo-0123456789abcdef.d")), Some("0123456789abcdef"));
        assert_eq!(unit_hash("build_script_build-0123456789abcdef"), Some("0123456789abcdef"));
        // a version directory under build/, not a unit hash
        assert_eq!(unit_hash("foo-1.2.3"), None);
        assert_eq!(unit_hash("foo-0123456789ABCDEF"), None);
        assert_eq!(unit_hash("short-0123456789abcde"), None);
    }
}
