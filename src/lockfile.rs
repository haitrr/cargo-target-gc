//! The one static check here that is also *sound*: a unit built from a
//! dependency version `Cargo.lock` no longer resolves to.
//!
//! The lockfile is the complete resolution for the workspace, so no feature
//! set, profile or RUSTFLAGS can reach such a unit -- it is garbage with
//! certainty, at the cost of reading one dep-info file. This is the growth that
//! compounds: every dependency bump strands a full set of artifacts.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use regex::Regex;
use std::sync::OnceLock;

fn re(src: &str) -> Regex {
    Regex::new(src).expect("static regex")
}

/// A registry source path: `<..>/registry/src/<index>/<name>-<version>/<..>`.
/// The version is anchored on the first digit-dot run so a crate whose own name
/// ends in something numeric (`base64-0.22.1`, `sha2-0.10.9`) still splits right.
fn registry_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"/registry/src/[^/]+/([A-Za-z0-9_.-]+?)-(\d+\.\d+\.\d+[^/]*)/"))
}

/// A git checkout: `<..>/git/checkouts/<repo>-<repohash>/<shortrev>/<..>`. The
/// path segment is an abbreviated rev that prefixes the 40-hex rev the lockfile
/// records after the `#`. Matching on the rev and not the name is deliberate --
/// the directory carries the *repo* name, which need not equal the package name.
fn git_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"/git/checkouts/[^/]+/([0-9a-f]{7,40})/"))
}

fn lock_pkg_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r#"(?m)^name = "([^"]+)"\nversion = "([^"]+)""#))
}

fn lock_gitrev_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r#"(?m)^source = "git\+[^"]*#([0-9a-f]{40})""#))
}

pub struct Locked {
    pub pkgs: HashSet<(String, String)>,
    pub revs: HashSet<String>,
}

/// Nearest `Cargo.lock` at or above `start_dir`.
///
/// Walking up *from the target dir* rather than from the cwd is what keeps this
/// honest under a shared `CARGO_TARGET_DIR`: the lockfile found is always one
/// whose tree contains this target dir, so it cannot be some other project's. A
/// shared target dir outside any workspace finds nothing and the check is
/// skipped, which is the conservative answer -- soundness depends on the
/// lockfile being the COMPLETE resolution for everything built into this dir.
pub fn find_lockfile(start_dir: &Path) -> Option<PathBuf> {
    let mut d = start_dir;
    loop {
        let lock = d.join("Cargo.lock");
        if lock.is_file() {
            return Some(lock);
        }
        d = d.parent()?;
    }
}

pub fn read_lockfile(path: &Path) -> Option<Locked> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(Locked {
        pkgs: lock_pkg_re()
            .captures_iter(&text)
            .map(|c| (c[1].to_string(), c[2].to_string()))
            .collect(),
        revs: lock_gitrev_re()
            .captures_iter(&text)
            .map(|c| c[1].to_string())
            .collect(),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Registry { name: String, version: String },
    Git { short_rev: String },
}

/// What upstream source a unit was compiled from, read out of its dep-info.
///
/// `deps/<name>-<hash>.d` lists the unit's source files by absolute path, and
/// that path identifies the exact upstream revision. `None` is the important
/// case and must stay conservative: workspace crates write RELATIVE source
/// paths, so they match neither pattern and are never classified by version.
/// Their sources are not versioned and this check has no opinion on them.
pub fn unit_source(dep_info_paths: &[PathBuf]) -> Option<Source> {
    for dp in dep_info_paths {
        let Ok(mut f) = File::open(dp) else { continue };
        let mut buf = vec![0u8; 65536];
        let Ok(n) = f.read(&mut buf) else { continue };
        buf.truncate(n);
        let head = String::from_utf8_lossy(&buf);
        if let Some(c) = registry_re().captures(&head) {
            return Some(Source::Registry {
                name: c[1].to_string(),
                version: c[2].to_string(),
            });
        }
        if let Some(c) = git_re().captures(&head) {
            return Some(Source::Git {
                short_rev: c[1].to_string(),
            });
        }
    }
    None
}

impl Locked {
    /// True when nothing in the current resolution can reach this source.
    pub fn is_stale(&self, src: &Source) -> bool {
        match src {
            Source::Registry { name, version } => {
                !self.pkgs.contains(&(name.clone(), version.clone()))
            }
            Source::Git { short_rev } => !self.revs.iter().any(|r| r.starts_with(short_rev)),
        }
    }
}
