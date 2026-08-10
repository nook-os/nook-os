# Where a build worktree's `target/` comes from

MAIN-493's investigation, measured on 2026-08-09 in a build worktree of this
repo on the dev node. It exists because a worktree's `target/` reached 120 GB
and filled the machine — 518 MB free of 1.5 TB — and because the obvious next
move, making the builds share artifacts, turns out to be worth much less than
it looks.

## One clean cycle is 86 GB

The checks `test.sh` runs, from an empty `target/`, cumulative:

| after | `target/` |
| --- | --- |
| `cargo clippy --workspace --all-targets` | 8.4 GB |
| `cargo test --workspace --no-run` | 83 GB |
| `cargo build --workspace` | 86 GB |

and where those bytes sit at the end:

| | |
| --- | --- |
| `debug/deps` | 77 GB |
| `debug/incremental` | 8.9 GB |
| `debug/build` | 357 MB |
| `debug/.fingerprint` | 34 MB |

`deps` is almost entirely **integration-test binaries**: 148 executables, the
largest 732 MB (`nook_control`) and a long plateau at ~617 MB each. This repo
has 123 integration-test files, 115 of them nook-control's, and cargo links one
binary per file — each statically holding the workspace and its dependency
graph with debug info. That, and not duplication, is where the first 77 GB
goes. Folding those files into fewer binaries is MAIN-493's NG-3: rejected,
because single-test iteration and per-file failure isolation are worth more
than the disk.

Duplication is what turns 86 GB into 120 GB, and it does so **over time**
rather than within a cycle: at the end of this one, 148 executables covered 138
distinct target names — only the four workspace binaries had more than one copy.

## Why nothing is ever deleted

Cargo names an artifact `<name>-<metadata-hash>`. The hash covers the unit's
**configuration** — package, target kind, profile, mode, features, rustc — and
never its content. Same configuration, same filename, overwritten in place.
Different configuration, a **new** filename, and the old one is kept for as
long as the directory exists. There is no garbage collector, and `cargo clean`
is all-or-nothing.

So `target/` is (one artifact set) × (how many distinct configurations have ever
been built in it), and the second factor only ever grows.

## AC-5: which invocation produces which profile hash

Each fingerprint records a `profile` hash — cargo's hash of
`(profile, mode, extra args, lto, lint rustflags)`. The worktree measured above
held **176 distinct values over 1704 units, under a single `rustc` hash**.

Attributed by giving each invocation a target directory of its own and diffing
the sets. Workspace-wide:

| invocation | distinct profile hashes |
| --- | --- |
| `cargo check --workspace --all-targets` | 112 |
| `cargo build --workspace` | 95 |
| shared by both | 32 |

`112 ∪ 95 = 175`, every one of them among the 176. The 176th is
`1722584277633009122`, the hash `cargo test --no-run` adds on top of
`cargo build` for the test-harness units. **All 176 are accounted for, with
nothing left over.**

The relationships between the invocations, isolated on one crate (`-p
nook-node`, whose smaller graph makes the deltas legible):

| invocation | hashes | |
| --- | --- | --- |
| `cargo check` | 83 | |
| `cargo clippy --all-targets` | 84 | check's set **exactly**, plus `3316208278650011218` (142 units, all `test-*`) |
| `cargo build` | 70 | 24 shared with `check`; the rest disjoint |
| `cargo test --no-run` | 71 | build's set **exactly**, plus `1722584277633009122` (141 units) |

So there are four families, not 176:

- **Check** — `cargo check`, `cargo clippy`; emits `rmeta`.
- **Check, test targets** — what `--all-targets` adds: exactly one hash.
- **Build** — `cargo build`, `cargo test`; emits `rlib` and binaries.
- **Build, test harnesses** — what building `--test` targets adds: exactly one.

The 176 values are those four families spread across the dependency graph:
within one invocation every unit has exactly one hash, and the count reflects
how many distinct positions the graph has (LTO/bitcode decisions differ by where
a crate sits), not how many times anything was rebuilt. **The count is a red
herring; the families are the multiplier.**

The card's top five, attributed:

| hash | units | family |
| --- | --- | --- |
| `15657897354478470176` | 358 lib | Build |
| `2241668132362809309` | 305 lib | Check |
| `2225463790103693989` | 205 lib + build scripts | shared — build scripts and proc macros are Build mode under both |
| `3316208278650011218` | 142 test | Check, test targets |
| `1722584277633009122` | 141 test | Build, test harnesses |

### Where two differ only trivially

**Nowhere.** This is the answer AC-5 was hoping would be different:

- **Check and Build cannot be unified.** They share 32 hashes of 175 — the build
  scripts and proc macros, compiled rather than checked under both. `rmeta` is
  not an `rlib`; a checked crate cannot be linked. Nothing here is a flag away
  from collapsing.
- **`clippy` and `check` are already the same set** — identical profile hashes
  and an identical `rustc` hash — so there is no divergence left to remove.
  `--all-targets` is the only difference, and it buys the test targets' lints.
- **The two test legs add no profile hash at all.** The Postgres leg and the
  SQLite leg (MAIN-270) are the same cargo invocation with a different
  `DATABASE_URL`. That variable reaches the fingerprint through sqlx's macros as
  a `local` entry, so the second leg **rebuilds in place** — same filenames,
  overwritten. It costs time, not disk.
- **Host versus container is the one real multiplier, and it is not a profile
  difference.** `./test.sh --host` and the container leg run different rustc
  installations, so every unit gets a different `rustc` hash, a different
  `-C metadata`, and therefore a **complete second artifact set** with no
  sharing at all. That is the two size classes the card measured for
  `nook_control` (~515 MB and ~678 MB). Removing it means dropping one of the
  two legs, which is a policy decision rather than a cheap unification.

There is no free artifact set to remove at the source. Throwing the output away
once the run that made it has concluded is the whole of the answer.

## What the reclaim does

`crates/nook-node/src/loop_job.rs` deletes the directories a repo declares in
`.nook.toml`'s `[worktree] reclaim` when a build run concludes, keeping the
worktree, its branch and its git state. Measured on the worktree this
investigation ran in: **86 GB → 240 MB in 19.6 s**, with the branch, the
commits, the tracked files and the seeded `.env` all untouched.
