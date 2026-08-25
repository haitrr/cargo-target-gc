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
use clap::{Parser, Subcommand};

use collect::{Category, Claims, Options};
use fsutil::{human, walk_stat, Reclaim};

const AFTER_HELP: &str = "\
TIERS
  A  free                        orphaned units, dead fingerprints, abandoned and
                                 superseded rustc sessions, .dSYM, doc/tmp/package,
                                 and units built from dependency versions Cargo.lock
                                 no longer resolves to. This is what --apply deletes.
  B  recompile unmarked configs  unit variants from feature/profile/RUSTFLAGS configs
                                 you no longer build. Needs `mark` first, then --sweep.
  C  first-edit penalty          incremental/ caches. Opt in with --incremental.

WHY NOT SWEEP BY MTIME
  Cargo does not touch a unit's files when it comes back Fresh, so mtime records
  when a unit was last COMPILED, not when it was last USED — and the units you
  rebuild least are exactly your stable ones. A cutoff sweep deletes the live set
  and keeps the garbage. That is why tier B asks cargo instead of guessing.

EXAMPLES
  cargo target-gc                              report only, changes nothing
  cargo target-gc --apply                      delete the free tier
  cargo target-gc --incremental all --apply    also drop incremental caches
  cargo target-gc mark -- cargo build          record what today's build uses
  cargo target-gc mark -- cargo test --no-run  ...marks accumulate across runs
  cargo target-gc --sweep --apply              delete every unmarked unit variant
  cargo target-gc --budget 20G --apply         free cheapest-first until it fits
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

    /// Also collect unit variants no mark covers (tier B); requires a prior `mark`
    #[arg(long)]
    sweep: bool,

    /// Also collect rustc incremental caches (tier C) [default when bare: stale:14]
    #[arg(long, value_name = "all|stale:DAYS", num_args = 0..=1, default_missing_value = "stale:14")]
    incremental: Option<String>,

    /// Collect cheapest-first only until target/ fits in SIZE (e.g. 20G)
    #[arg(long, value_name = "SIZE")]
    budget: Option<String>,

    /// Machine-readable summary
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record the unit hashes a real build resolves to (the tier B oracle)
    Mark {
        /// Build command to run; defaults to `cargo build`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "CMD")]
        command: Vec<String>,
    },
    /// Show what the current mark file covers
    Marks,
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

    match &cli.cmd {
        Some(Cmd::Marks) => {
            let live = marks::load(&target_dir);
            println!(
                "{} live unit hashes in {}",
                live.len(),
                marks::mark_path(&target_dir).display()
            );
            if live.is_empty() {
                println!("run `cargo target-gc mark -- cargo build` before --sweep");
            }
            return Ok(());
        }
        Some(Cmd::Mark { command }) => {
            let cmd = if command.is_empty() {
                vec!["cargo".to_string(), "build".to_string()]
            } else {
                command.clone()
            };
            let live = marks::mark(&target_dir, &[cmd], cli.json)?;
            eprintln!(
                "marked {} live unit hashes in {}",
                live.len(),
                marks::mark_path(&target_dir).display()
            );
            // Marking alone changes nothing unless a collection was also asked for.
            if !cli.sweep && !cli.apply && cli.budget.is_none() {
                return Ok(());
            }
        }
        None => {}
    }

    let live = if cli.sweep {
        let m = marks::load(&target_dir);
        if m.is_empty() {
            bail!("--sweep needs marks first: run `cargo target-gc mark -- <the build you use>`");
        }
        Some(m)
    } else {
        None
    };

    let incremental = match cli.incremental.as_deref() {
        None => None,
        Some("all") => Some(None),
        Some(s) => match s.strip_prefix("stale:").and_then(|d| d.parse::<f64>().ok()) {
            Some(days) => Some(Some(days)),
            None => bail!("--incremental takes `all` or `stale:DAYS` (got {s:?})"),
        },
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
        incremental,
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
            if !cli.sweep {
                more.push("--sweep");
            }
            if cli.incremental.as_deref() != Some("all") {
                more.push("--incremental all");
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
