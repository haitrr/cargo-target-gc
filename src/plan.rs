//! Ask cargo which units a command resolves to -- without compiling anything.
//!
//! The unit hash in `deps/libfoo-<hash>.rlib` is a SipHash over the package id,
//! the resolved features, the profile, the compile mode, the LTO decision, the
//! full `rustc -vV`, RUSTFLAGS, and every dependency unit's hash. None of that
//! is recoverable from the tree, and no CLI on stable prints it: `--build-plan`
//! used to, and was removed in cargo 1.93; `--unit-graph` is nightly and omits
//! the hashes. So the only way to know what a build WOULD resolve to used to be
//! to run it -- which on a config this tree has never held is a cold build of
//! the whole workspace, paid to mark units that were never here.
//!
//! Cargo is a library, though. `create_bcx` resolves the unit graph and
//! `prepare_units` lays out the file names, neither of which invokes rustc on
//! your code, and `CompilationFiles::metadata` then hands over the hash cargo
//! itself would use. That turns marking from "run the build and watch" into a
//! lookup.
//!
//! Two seams to know about. `cargo::core::compiler::lto` is private, so its
//! whole-graph pass is re-implemented here -- `BuildRunner` panics on a missing
//! entry, and a wrong LTO value is a wrong hash. And cargo's library API is
//! explicitly unstable ("may make major changes"), so everything here is
//! fallible by design: any failure returns an error, and the caller falls back
//! to running the command for real.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use cargo::core::compiler::{BuildContext, BuildRunner, CompileMode, CrateType, Lto, Unit, UnitInterner, UserIntent};
use cargo::core::resolver::features::CliFeatures;
use cargo::core::{profiles, Workspace};
use cargo::ops::{create_bcx, CompileFilter, CompileOptions, Packages};
use cargo::util::interning::InternedString;
use cargo::GlobalContext;

/// The parts of a cargo command line that change which units it resolves to.
///
/// Anything not listed here is either irrelevant to the unit graph (`--jobs`,
/// `--message-format`) or unsupported, and unsupported is an error rather than
/// a silent omission: a flag we ignore is a unit graph we get wrong, and a
/// wrong live set deletes units that are in use.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub intent: Intent,
    pub lib_only: bool,
    pub bins: Vec<String>,
    pub all_bins: bool,
    pub tests: Vec<String>,
    pub all_tests: bool,
    pub examples: Vec<String>,
    pub all_examples: bool,
    pub benches: Vec<String>,
    pub all_benches: bool,
    pub all_targets: bool,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub packages: Vec<String>,
    pub workspace: bool,
    pub exclude: Vec<String>,
    pub profile: Option<String>,
    pub targets: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum Intent {
    #[default]
    Build,
    Check,
    Test,
    Bench,
}

impl Intent {
    fn to_cargo(self, has_test_filter: bool) -> UserIntent {
        match self {
            Intent::Build => UserIntent::Build,
            // `cargo check --tests` type-checks in test mode, which is a
            // different unit again from a plain check of the same crate.
            Intent::Check => UserIntent::Check { test: has_test_filter },
            Intent::Test => UserIntent::Test,
            Intent::Bench => UserIntent::Bench,
        }
    }
}

/// Parse `["cargo", "build", "--tests"]`.
///
/// Front ends that are not cargo subcommands (`cargo clippy`, and anything
/// running its own rustc wrapper) are rejected: their units carry the wrapper
/// path in the hash, so planning them as a plain check would name the wrong
/// units entirely.
pub fn parse(argv: &[String]) -> Result<Spec> {
    let mut it = argv.iter().map(String::as_str);
    match it.next() {
        Some("cargo") => {}
        Some(other) => bail!("not a cargo command: {other}"),
        None => bail!("empty command"),
    }
    let intent = match it.next() {
        Some("build") | Some("b") => Intent::Build,
        Some("check") | Some("c") => Intent::Check,
        Some("test") | Some("t") => Intent::Test,
        Some("bench") => Intent::Bench,
        Some(other) => bail!("unsupported cargo subcommand: {other}"),
        None => bail!("no cargo subcommand"),
    };
    let mut spec = Spec { intent, ..Spec::default() };
    let mut rest: Vec<&str> = it.collect();
    rest.reverse();
    while let Some(arg) = rest.pop() {
        let value = |flag: &str, rest: &mut Vec<&str>| -> Result<String> {
            match arg.split_once('=') {
                Some((_, v)) => Ok(v.to_string()),
                None => rest.pop().map(String::from).ok_or_else(|| anyhow!("{flag} needs a value")),
            }
        };
        let head = arg.split_once('=').map(|(h, _)| h).unwrap_or(arg);
        match head {
            "--lib" => spec.lib_only = true,
            "--bins" => spec.all_bins = true,
            "--bin" => spec.bins.push(value("--bin", &mut rest)?),
            "--tests" => spec.all_tests = true,
            "--test" => spec.tests.push(value("--test", &mut rest)?),
            "--examples" => spec.all_examples = true,
            "--example" => spec.examples.push(value("--example", &mut rest)?),
            "--benches" => spec.all_benches = true,
            "--bench" => spec.benches.push(value("--bench", &mut rest)?),
            "--all-targets" => spec.all_targets = true,
            "--features" | "-F" => spec
                .features
                .extend(value("--features", &mut rest)?.split([' ', ',']).map(String::from)),
            "--all-features" => spec.all_features = true,
            "--no-default-features" => spec.no_default_features = true,
            "--package" | "-p" => spec.packages.push(value("--package", &mut rest)?),
            "--workspace" | "--all" => spec.workspace = true,
            "--exclude" => spec.exclude.push(value("--exclude", &mut rest)?),
            "--release" | "-r" => spec.profile = Some("release".into()),
            "--profile" => spec.profile = Some(value("--profile", &mut rest)?),
            "--target" => spec.targets.push(value("--target", &mut rest)?),
            // Not a unit-graph input: it changes what cargo prints, not what it
            // resolves. `--no-run` likewise -- `cargo test --no-run` builds the
            // same units `cargo test` does.
            "--message-format" | "--jobs" | "-j" | "--color" => {
                let _ = value(head, &mut rest);
            }
            "--no-run" | "--quiet" | "-q" | "--offline" | "--locked" | "--frozen" => {}
            other => bail!("unsupported flag: {other}"),
        }
    }
    Ok(spec)
}

/// The cargo this crate links, as a CLI version string.
///
/// The hash inputs are cargo's own and have changed before (1.85 folded
/// RUSTFLAGS into the filename hash), so a linked library older or newer than
/// the cargo on PATH is a reason to distrust every hash computed here.
pub fn linked_cargo_version() -> String {
    cargo::version().to_string()
}

/// Every unit hash the command resolves to, computed rather than observed.
pub fn unit_hashes(manifest: &Path, argv: &[String]) -> Result<HashSet<String>> {
    let spec = parse(argv)?;
    let gctx = GlobalContext::default().map_err(|e| anyhow!("cargo config: {e}"))?;
    let ws = Workspace::new(manifest, &gctx).map_err(|e| anyhow!("opening workspace: {e}"))?;

    let has_test_filter = spec.all_targets || spec.all_tests || !spec.tests.is_empty();
    let mut opts = CompileOptions::new(&gctx, spec.intent.to_cargo(has_test_filter))
        .map_err(|e| anyhow!("compile options: {e}"))?;
    opts.filter = CompileFilter::from_raw_arguments(
        spec.lib_only,
        spec.bins.clone(),
        spec.all_bins,
        spec.tests.clone(),
        spec.all_tests,
        spec.examples.clone(),
        spec.all_examples,
        spec.benches.clone(),
        spec.all_benches,
        spec.all_targets,
    );
    opts.cli_features = CliFeatures::from_command_line(
        &spec.features,
        spec.all_features,
        !spec.no_default_features,
    )
    .map_err(|e| anyhow!("features: {e}"))?;
    opts.spec = if spec.workspace {
        Packages::All(Vec::new())
    } else if !spec.exclude.is_empty() {
        Packages::OptOut(spec.exclude.clone())
    } else if !spec.packages.is_empty() {
        Packages::Packages(spec.packages.clone())
    } else {
        Packages::Default
    };
    if let Some(p) = &spec.profile {
        opts.build_config.requested_profile = InternedString::new(p);
    }
    if !spec.targets.is_empty() {
        opts.build_config.requested_kinds =
            cargo::core::compiler::CompileKind::from_requested_targets(&gctx, &spec.targets)
                .map_err(|e| anyhow!("--target: {e}"))?;
    }

    let interner = UnitInterner::new();
    let bcx = create_bcx(&ws, &opts, &interner, None).map_err(|e| anyhow!("resolving units: {e}"))?;
    let mut runner = BuildRunner::new(&bcx).map_err(|e| anyhow!("build runner: {e}"))?;
    // `lto::generate` is private, and `metadata` indexes this map without a
    // fallback, so it is filled here or the call panics.
    runner.lto = lto_map(&bcx)?;
    runner.prepare_units().map_err(|e| anyhow!("preparing units: {e}"))?;

    let mut out = HashSet::new();
    for unit in bcx.unit_graph.keys() {
        let meta = runner.files().metadata(unit);
        // `unit_id` is what lands in `-C extra-filename`, and so what the file
        // names in deps/ and the dir names in .fingerprint/ carry.
        out.insert(format!("{}", meta.unit_id()));
    }
    Ok(out)
}

/// A port of cargo's private `lto::generate`, which decides how much bitcode
/// each unit has to carry. It is part of the hash, so it cannot be guessed:
/// this mirrors cargo 0.98's pass exactly, and any divergence shows up as a
/// hash that matches nothing on disk.
fn lto_map(bcx: &BuildContext<'_, '_>) -> Result<std::collections::HashMap<Unit, Lto>> {
    let mut map = std::collections::HashMap::new();
    for unit in bcx.roots.iter() {
        let root_lto = match unit.profile.lto {
            profiles::Lto::Bool(false) => Lto::OnlyObject,
            profiles::Lto::Off => Lto::Off,
            _ => {
                let crate_types = unit.target.rustc_crate_types();
                if unit.target.for_host() {
                    Lto::OnlyObject
                } else if needs_object(&crate_types) {
                    lto_when_needs_object(&crate_types)
                } else {
                    Lto::OnlyBitcode
                }
            }
        };
        calculate(bcx, &mut map, unit, root_lto)?;
    }
    Ok(map)
}

fn needs_object(crate_types: &[CrateType]) -> bool {
    crate_types.iter().any(|k| k.can_lto() || k.is_dynamic())
}

fn lto_when_needs_object(crate_types: &[CrateType]) -> Lto {
    if crate_types.iter().all(|ct| *ct == CrateType::Dylib) {
        Lto::OnlyObject
    } else {
        Lto::ObjectAndBitcode
    }
}

fn calculate(
    bcx: &BuildContext<'_, '_>,
    map: &mut std::collections::HashMap<Unit, Lto>,
    unit: &Unit,
    parent_lto: Lto,
) -> Result<()> {
    let crate_types = match unit.mode {
        CompileMode::Test | CompileMode::Doctest => vec![CrateType::Bin],
        _ => unit.target.rustc_crate_types(),
    };
    let all_lto_types = crate_types.iter().all(CrateType::can_lto);
    let lto = if unit.target.for_host() {
        Lto::OnlyObject
    } else if all_lto_types {
        match unit.profile.lto {
            profiles::Lto::Named(s) => Lto::Run(Some(s)),
            profiles::Lto::Off => Lto::Off,
            profiles::Lto::Bool(true) => Lto::Run(None),
            profiles::Lto::Bool(false) => Lto::OnlyObject,
        }
    } else {
        match (parent_lto, needs_object(&crate_types)) {
            (Lto::Run(_), false) => Lto::OnlyBitcode,
            (Lto::Run(_), true) | (Lto::OnlyBitcode, true) => lto_when_needs_object(&crate_types),
            (Lto::Off, _) => Lto::Off,
            (_, false) | (Lto::OnlyObject, true) | (Lto::ObjectAndBitcode, true) => parent_lto,
        }
    };

    let merged_lto = match map.entry(unit.clone()) {
        std::collections::hash_map::Entry::Vacant(v) => *v.insert(lto),
        std::collections::hash_map::Entry::Occupied(mut v) => {
            let result = match (lto, v.get()) {
                (Lto::OnlyBitcode, Lto::OnlyBitcode) => Lto::OnlyBitcode,
                (Lto::OnlyObject, Lto::OnlyObject) => Lto::OnlyObject,
                (Lto::Run(s), _) | (_, &Lto::Run(s)) => Lto::Run(s),
                (Lto::Off, _) | (_, Lto::Off) => Lto::Off,
                (Lto::ObjectAndBitcode, _) | (_, Lto::ObjectAndBitcode) => Lto::ObjectAndBitcode,
                (Lto::OnlyObject, Lto::OnlyBitcode) | (Lto::OnlyBitcode, Lto::OnlyObject) => {
                    Lto::ObjectAndBitcode
                }
            };
            if result == *v.get() {
                return Ok(());
            }
            v.insert(result);
            result
        }
    };

    for dep in &bcx.unit_graph[unit] {
        calculate(bcx, map, &dep.unit, merged_lto)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> Spec {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn target_selection_and_features_are_read() {
        let s = spec("cargo build --tests --no-default-features -p oxy-server");
        assert_eq!(s.intent, Intent::Build);
        assert!(s.all_tests && s.no_default_features);
        assert_eq!(s.packages, vec!["oxy-server"]);

        let s = spec("cargo check --features a,b --release");
        assert_eq!(s.intent, Intent::Check);
        assert_eq!(s.features, vec!["a", "b"]);
        assert_eq!(s.profile.as_deref(), Some("release"));

        // `--features=a` and `--features a` are the same flag
        assert_eq!(spec("cargo build --features=a").features, vec!["a"]);
    }

    /// The claim this whole module rests on: the hashes computed here are the
    /// ones cargo writes. Built against a real build of a real crate, because
    /// the failure mode is silent -- a wrong hash matches nothing, and a live
    /// unit that matches nothing is one --apply deletes.
    #[test]
    fn computed_hashes_are_the_ones_cargo_writes() {
        use std::fs;
        let root = std::env::temp_dir()
            .join(format!("cargo-target-gc-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"planned\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() -> u8 { 7 }\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { println!(\"{}\", planned::f()) }\n").unwrap();

        let out = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(&root)
            .output()
            .expect("cargo build");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

        let planned = unit_hashes(&root.join("Cargo.toml"), &["cargo".into(), "build".into()]).unwrap();
        let mut on_disk: Vec<String> = Vec::new();
        for e in fs::read_dir(root.join("target/debug/deps")).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(h) = crate::scan::unit_hash(crate::scan::stem(&name)) {
                on_disk.push(h.to_string());
            }
        }
        assert!(!on_disk.is_empty(), "nothing built");
        for h in &on_disk {
            assert!(planned.contains(h), "cargo wrote {h}, which the plan does not name: {planned:?}");
        }
    }

    /// Flags that do not move the unit graph must not be mistaken for ones
    /// that do -- and anything unrecognised has to be an error, since a flag
    /// silently ignored is a live set that is silently wrong.
    #[test]
    fn irrelevant_flags_pass_and_unknown_ones_refuse() {
        assert_eq!(spec("cargo test --no-run --quiet").intent, Intent::Test);
        assert!(parse(&["cargo".into(), "build".into(), "--future-flag".into()]).is_err());
        assert!(parse(&["cargo".into(), "clippy".into()]).is_err());
        assert!(parse(&["just".into(), "build".into()]).is_err());
    }
}

