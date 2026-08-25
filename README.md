# cargo-target-gc

Reclaim disk from a Rust workspace's `target/` **by rebuild cost**, instead of
`cargo clean`'s all-or-nothing.

`cargo clean` deletes the bulk of `target/` that is registry dependencies you
never edit, and you pay a full cold rebuild for them. What actually grows is the
part cargo never collects: stale unit variants in `deps/` from
feature/profile/RUSTFLAGS configs you no longer build, abandoned rustc sessions
in `incremental/`, orphaned fingerprints, artifacts from dependency versions
your `Cargo.lock` has moved past.

This walks `target/`, sorts what it finds by what a rebuild would cost, and lets
you delete by that cost. Nothing under `target/` is source — every category here
only ever costs time.

## Install

```sh
cargo install --path .
```

Then there are two commands:

```sh
cargo target-gc            # report what would go, delete nothing
cargo target-gc --apply    # delete it
```

Both run your build first (a freshness check on a warm tree), record the units
it resolves to, and treat everything else as garbage. Name every config you
actually use — anything you leave out is deleted and cold-rebuilds when you next
switch to it:

```sh
cargo target-gc --apply \
  --build 'cargo build' \
  --build 'cargo test --no-run' \
  --build 'cargo clippy --all-targets'
```

The default is `cargo build --all-targets`, which covers lib, bins, tests,
examples and benches for your default features, plus `cargo check --all-targets`
when the tree already contains check units. That second one matters: a check unit
emits metadata only and is a *different unit* with its own hash, so a build
command never resolves to one — without it, `cargo check` runs cold after a
collection. It is only added when such units already exist, so the default never
compiles metadata nobody asked for.

Anything driven by another compiler front-end is invisible to both and needs
naming explicitly — `--build 'cargo clippy --all-targets'`, and whatever your
editor runs if it shares this target dir. Note that on a tree where tests were
never built, the first run compiles them.

Other flags: `--no-build` reports without running anything (and then only
collects what needs no live set), `--keep-incremental N|all`, `--budget 20G`,
`--json`, `--target-dir`, `--manifest-path`.

It works in any workspace: the target dir comes from `cargo metadata`, so
`CARGO_TARGET_DIR`, `build.target-dir` and a shared target dir are all honoured.
Cross-compiled trees (`target/<triple>/<profile>`) are collected alongside the
host ones.

## What it deletes

| what | cost of deleting |
| --- | --- |
| unit variants no named build resolves to | recompiling a config you did not name |
| `incremental/` beyond the newest N dirs per crate (default 2) | one non-incremental compile of that crate, when you next edit it |
| orphaned units, fingerprints with no artifacts, abandoned and superseded rustc sessions, `.dSYM`, `target/{doc,tmp,package}` | free |
| units built from dependency versions `Cargo.lock` no longer resolves to | free, unless you check out a branch that pins them again |

### Why it has to run a build

There is no timestamp under `target/` that separates live from dead. Cargo does
**not** touch a unit's files or its `.fingerprint/*/invoked.timestamp` when the
unit comes back Fresh, so mtime records when a unit was last *compiled*, not when
it was last *used* — and the units you rebuild least are exactly your stable
ones. On a scratch workspace built four ways, the three units a plain `cargo
build` resolves to were the three *oldest*: a one-day cutoff would have deleted
the live set and kept the garbage. atime is no better: it updates only on the
first read after each write, on APFS and under Linux `relatime` alike.

Reconstructing reachability statically does not work either. A unit's recorded
config omits its package version, and the dependency fingerprints it records do
not resolve to units on disk — measured on a real tree, 100% of 63,354 dep edges
were unresolvable, and grouping "revisions" by config alone marked 577 units
dead that a plain `cargo build` was using. So the tool asks cargo, which is the
only component that actually knows.

### Why `incremental/` grows without bound

rustc garbage-collects sessions *inside* a crate dir, but never removes a crate
dir. The dir is keyed by `-C metadata`, so every new config or revision of a
crate starts a fresh one and orphans the old one forever — 959 dirs for 55
crates on the tree this was measured against, 48.8G, of which keeping the newest
2 per crate retains 5.9G. Dropping one cannot cascade: cargo never consults
incremental state for freshness, so the cost lands only when you next edit that
crate, and only as one non-incremental compile of it.

## Units that cannot be proven dead

Two kinds of unit are exempt from collection because the hash join that this
tool rests on does not reach them:

- **`cdylib` / `staticlib` crates.** Cargo omits `-C extra-filename` for them so
  the file name stays predictable for whatever links the library, so a crate with
  `crate-type = ["cdylib", "rlib"]` lands in `deps/` as `libfoo.rlib` and
  `libfoo.dylib` — no hash anywhere — while its fingerprint dir is still
  `.fingerprint/foo-<hash>/`. Nothing joins the two, so the fingerprint is left
  alone. Deleting it forces a recompile of that crate *and everything downstream
  of it*, which is a whole-workspace cascade when the cdylib crate is a core one.
- **Examples.** Cargo writes example units into `examples/`, not `deps/`, so that
  directory is indexed as an artifact dir too.

## Notes on artifact resolution

A bin or test unit is reported by cargo under its *uplifted* name
(`target/debug/myapp`, not `deps/myapp-<hash>`), so its hash has to be recovered
before it can be marked. Two routes are used, because neither alone is reliable:
the shared inode when cargo uplifted by hardlink, and the base name plus
extension (with size and mtime breaking ties) when it uplifted by **copy** —
which is what actually happens on macOS/APFS with cargo 1.9x. Any artifact that
still cannot be mapped to a unit hash is reported as a warning, because an
live unit that goes unrecorded is exactly what gets deleted.

## The lockfile check

The one static classification that is also *sound*: the lockfile is the complete
resolution for the workspace, so a unit compiled from a version it no longer
contains cannot be reached by any feature set, profile or RUSTFLAGS. The unit's
source revision is read out of its dep-info (`deps/<name>-<hash>.d`), which names
registry and git sources by absolute path. Workspace crates write *relative*
paths, match neither pattern, and are never classified this way. The one case
where this deletes something you wanted is checking out a branch that pins an old
dependency again — then that dependency recompiles.

## Why the numbers are smaller than `du`

Every figure printed is *reclaim*, not apparent size. A file with more than one
link frees nothing when you unlink one of its names, and target dirs are full of
those: rustc hardlinks unchanged object files into the next incremental session,
cargo uplifts binaries into the profile root, and some setups seed `deps/` across
checkouts. Each inode is counted once and only as reclaimable when every one of
its links is inside the set being deleted; the rest is reported as `shared`,
which is the honest answer to "why did deleting 11G free 300M".

## Safety

- Nothing is deleted without `--apply`.
- The tool refuses to run while a cargo build holds `target/<profile>/.cargo-lock`.
- A build command that exits non-zero aborts the run rather than deleting against
  a partial live set.
- Containment is re-checked per path immediately before deletion, so a symlink or
  a race between scan and delete cannot let a recursive remove escape `target/`.
- Each path is claimed by exactly one category, so nothing is double-counted in
  the report or deleted twice.

## Origin

Ported to Rust from an internal Python script, and generalized to work in any
workspace.
