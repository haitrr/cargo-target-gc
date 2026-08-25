//! Filesystem metadata and the hardlink-aware byte accounting.
//!
//! `du` lies about a target dir that shares inodes with anything else: rustc
//! hardlinks unchanged object files into the next incremental session, cargo
//! uplifts binaries into the profile root, and some setups seed `deps/` across
//! checkouts. Deleting one name for an inode frees nothing until the last name
//! goes, so every number this tool prints is *reclaim*, not apparent size.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

#[derive(Clone, Copy)]
pub struct Meta {
    pub ino: u64,
    pub nlink: u64,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[cfg(unix)]
fn ino_nlink_size_mtime(md: &fs::Metadata) -> (u64, u64, u64, i64) {
    use std::os::unix::fs::MetadataExt;
    (md.ino(), md.nlink(), md.size(), md.mtime())
}

/// Off unix there is no portable link count, so every file is treated as
/// single-linked: reclaim is then just size, and `shared` stays zero.
#[cfg(not(unix))]
fn ino_nlink_size_mtime(md: &fs::Metadata) -> (u64, u64, u64, i64) {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (0, 1, md.len(), mtime)
}

/// `lstat`: never follows the final symlink, so a link is measured as a link.
pub fn lmeta(p: &Path) -> Option<Meta> {
    let md = fs::symlink_metadata(p).ok()?;
    let (ino, nlink, size, mtime) = ino_nlink_size_mtime(&md);
    Some(Meta {
        ino,
        nlink,
        size,
        mtime,
        is_dir: md.is_dir(),
        is_symlink: md.file_type().is_symlink(),
    })
}

/// Byte accounting that understands hardlinks.
///
/// Each inode is counted once, and only as reclaimable once every one of its
/// links lives inside the set about to be deleted. The rest is reported as
/// `shared`, which is the honest answer to "why did deleting 11G free 300M".
#[derive(Default)]
pub struct Reclaim {
    /// ino -> (size, nlink, links deleted here)
    seen: HashMap<u64, (u64, u64, u64)>,
    /// bytes from single-link files
    plain: u64,
}

impl Reclaim {
    pub fn add(&mut self, m: &Meta) {
        if m.nlink <= 1 {
            self.plain += m.size;
            return;
        }
        let e = self.seen.entry(m.ino).or_insert((m.size, m.nlink, 0));
        e.2 += 1;
    }

    pub fn merge(&mut self, other: &Reclaim) {
        self.plain += other.plain;
        for (ino, (size, nlink, n)) in &other.seen {
            let e = self.seen.entry(*ino).or_insert((*size, *nlink, 0));
            e.2 += *n;
        }
    }

    pub fn reclaim(&self) -> u64 {
        self.plain
            + self
                .seen
                .values()
                .filter(|(_, nlink, n)| n >= nlink)
                .map(|(size, _, _)| *size)
                .sum::<u64>()
    }

    pub fn shared(&self) -> u64 {
        self.seen
            .values()
            .filter(|(_, nlink, n)| n < nlink)
            .map(|(size, _, _)| *size)
            .sum()
    }

    pub fn shared_files(&self) -> usize {
        self.seen.values().filter(|(_, nlink, n)| n < nlink).count()
    }
}

/// Account for `path`: one entry if it is a file, every contained file if it is
/// a directory. `skip` vetoes paths already claimed by a cheaper category, so
/// nothing is counted (or reported) twice.
pub fn walk_stat(path: &Path, acct: &mut Reclaim, skip: &dyn Fn(&Path) -> bool) {
    let Some(m) = lmeta(path) else { return };
    if !m.is_dir || m.is_symlink {
        acct.add(&m);
        return;
    }
    for entry in WalkDir::new(path).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_dir() {
            continue;
        }
        if skip(entry.path()) {
            continue;
        }
        if let Some(fm) = lmeta(entry.path()) {
            acct.add(&fm);
        }
    }
}

/// Newest mtime among a directory's immediate children -- enough to date a
/// rustc session dir without walking gigabytes of session contents.
pub fn newest_child_mtime(path: &Path) -> i64 {
    let Ok(rd) = std::fs::read_dir(path) else { return 0 };
    rd.flatten()
        .filter_map(|e| lmeta(&e.path()).map(|m| m.mtime))
        .max()
        .unwrap_or(0)
}

pub fn human(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if v < 1024.0 || unit == "T" {
            return if unit == "B" {
                format!("{v:.0}B")
            } else {
                format!("{v:.1}{unit}")
            };
        }
        v /= 1024.0;
    }
    unreachable!()
}
