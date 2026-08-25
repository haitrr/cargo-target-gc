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

Then it is a cargo subcommand:

```sh
cargo target-gc                              # report only, changes nothing
cargo target-gc --apply                      # delete the free tier (A)
cargo target-gc --incremental all --apply    # also drop incremental caches (C)
cargo target-gc mark -- cargo build          # record what today's build uses
cargo target-gc mark -- cargo test --no-run  # marks accumulate across runs
cargo target-gc --sweep --apply              # delete every unmarked variant (B)
cargo target-gc --budget 20G --apply         # free cheapest-first until it fits
cargo target-gc --json                       # machine-readable summary
```

It works in any workspace: the target dir comes from `cargo metadata`, so
`CARGO_TARGET_DIR`, `build.target-dir` and a shared target dir are all honoured,
and `--target-dir` / `--manifest-path` override it. Cross-compiled trees
(`target/<triple>/<profile>`) are collected alongside the host ones.

## The three tiers

| tier | cost of deleting | what it is |
| --- | --- | --- |
| **A** | free | artifacts with no fingerprint and fingerprints with no artifacts; rustc sessions a killed build left unfinalized; superseded incremental sessions; `.dSYM` bundles; `target/{doc,tmp,package}`; and units built from dependency versions `Cargo.lock` no longer resolves to |
| **B** | recompiling a config you did not mark | unit variants from feature/profile/RUSTFLAGS configs you no longer build — the thing that actually accumulates |
| **C** | the first-edit penalty, per crate, once | `incremental/` caches, pure speed cache; correctness never depends on it |

Tier A is what plain `--apply` deletes. Tier B needs `mark` first, then
`--sweep`. Tier C is opt-in with `--incremental` (`all`, or `stale:DAYS`,
default `stale:14`).

The one static classification that is also *sound* is the lockfile check: the
lockfile is the complete resolution for the workspace, so a unit compiled from a
version it no longer contains cannot be reached by any feature set, profile or
RUSTFLAGS. The unit's source revision is read out of its dep-info
(`deps/<name>-<hash>.d`), which names registry and git sources by absolute path.
Workspace crates write *relative* paths, match neither pattern, and are never
classified this way. The one case where tier A deletes something you wanted is
checking out a branch that pins an old dependency again — then that dependency
recompiles.

## Why not sweep by mtime, the way `cargo-sweep` does

Because mtime here is not merely imprecise, it is inverted. Cargo does **not**
touch a unit's files or its `.fingerprint/*/invoked.timestamp` when the unit
comes back Fresh, so a unit's mtime records when it was last *compiled*, not when
it was last *used* — and the units you rebuild least are exactly your stable
ones. On a scratch workspace built four ways, the three units a plain `cargo
build` resolves to were the three *oldest*: a one-day cutoff would have deleted
the live set and kept the garbage.

So tier B asks cargo instead of guessing. `mark` runs your real build command
with `--message-format=json` and records the unit hashes it resolves to;
`--sweep` deletes every hash no mark covers. Mark once per config you actually
use — marks are unioned across runs and stored in `target/.gc-live.json`.

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
- Containment is re-checked per path immediately before deletion, so a symlink or
  a race between scan and delete cannot let a recursive remove escape `target/`.
- Each path is claimed by exactly one category, so nothing is double-counted in
  the report or deleted twice.

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
unmarked live unit is exactly what `--sweep` would delete.

## Origin

Ported to Rust from an internal Python script, and generalized to work in any
workspace.
