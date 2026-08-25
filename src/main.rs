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

  Name every config you actually use, or the ones you leave out get deleted and
  cold-rebuild the next time you switch to them:

    cargo target-gc --apply --build 'cargo build' --build 'cargo clippy --all-targets'

WHAT IT DELETES
  unit variants     every unit no named build resolves to
  incremental/      all but the newest --keep-incremental dirs per crate; rustc
                    never removes these itself, so they grow without bound
  free leftovers    orphaned units, dead fingerprints, abandoned rustc sessions,
                    .dSYM, doc/tmp/package, and units built from dependency
                    versions Cargo.lock no longer resolves to

EXAMPLES
  cargo target-gc                       report what would go, delete nothing
  cargo target-gc --apply               delete it
  cargo target-gc --no-build            report without running any build
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

    /// A build whose units to keep, repeatable [default: cargo build --all-targets]
    #[arg(long, value_name = "CMD")]
    build: Vec<String>,

    /// Do not run any build: report and collect only what needs no live set
    #[arg(long, conflicts_with = "build")]
    no_build: bool,

    /// Incremental cache dirs to keep per crate, or `all` to keep every one
    #[arg(long, value_name = "N|all", default_value = "2")]
    keep_incremental: String,

    /// Collect cheapest-first only until target/ fits in SIZE (e.g. 20G)
    #[arg(long, value_name = "SIZE")]
    budget: Option<String>,

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
    let target_dir = resolve_target_dir(&cli)?;
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

    // Ask cargo what is live. A warm tree makes this a freshness check; a cold
    // one compiles, which is the same work the next build would have done.
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
            vec![vec![
                "cargo".into(),
                "build".into(),
                "--all-targets".into(),
            ]]
        } else {
            cli.build.iter().map(|c| c.split_whitespace().map(String::from).collect()).collect()
        };
        Some(marks::mark(&target_dir, &cmds, cli.json)?)
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

    let freed = delete(&cats, &target_dir, cli.apply)?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report::json_summary(&cats, &target_dir, cli.apply))?);
    } else {
        report::report(&cats, cli.apply, budget_note.as_deref());
        if cli.apply && freed > 0 {
            println!("\nfreed {}", human(freed));
        }
    }
    Ok(())
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
fn resolve_target_dir(cli: &Cli) -> Result<PathBuf> {
    if let Some(d) = &cli.target_dir {
        return Ok(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()));
    }
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    if let Some(m) = &cli.manifest_path {
        cmd.arg("--manifest-path").arg(m);
    }
    if let Ok(out) = cmd.output() {
        if out.status.success() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                if let Some(d) = v.get("target_directory").and_then(|d| d.as_str()) {
                    return Ok(PathBuf::from(d));
                }
            }
        }
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
    use super::parse_size;
    use crate::scan::{stem, unit_hash};

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
