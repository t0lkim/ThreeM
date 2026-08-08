# Contributing to ThreeM

`mmm` moves other people's photograph libraries. That single fact decides most
of what follows: the default is a preview, every committing run is journalled
before it acts, and a change that touches the code which moves, records or
restores files is held to a harder standard than the rest of the repository.

If you are here to report something rather than change it, jump to
[Reporting a bug](#reporting-a-bug) or [`SECURITY.md`](SECURITY.md).

## Layout

The repository root is the *product*; the crate lives one level down.

| Path | Holds |
|---|---|
| `code/` | **The crate root.** `Cargo.toml`, `src/`, `tests/`, `benches/`, `fuzz/`. Every `cargo` command runs from here. |
| `code/src/` | Library modules, `main.rs` (the `mmm` binary) and `bin/mmm_dedup_verifier.rs` (the second, independent one). |
| `code/tests/` | Integration suites, one file per surface, driving the real binary through `assert_cmd`. Shared fixtures in `tests/common/`. |
| `code/fuzz/` | A separate crate — its own `Cargo.toml`, `Cargo.lock` and pinned toolchain. See [`code/fuzz/README.md`](code/fuzz/README.md). |
| `docs/` | Everything a reader needs and no source. Start at [`docs/index.md`](docs/index.md), which links the lot. |
| `CHANGELOG.md`, `README.md`, this file | Root, because they are about the product rather than the build. |

The convention is worth stating plainly because it is the first thing that
catches people out: **`cargo test` at the repository root finds nothing.**
`cd code` first, or pass `--manifest-path code/Cargo.toml`. CI does the former,
on every step.

## Building

```sh
cd code
cargo build                    # debug
cargo build --release          # binaries at code/target/release/{mmm,mmm-dedup-verifier}
cargo install --path .         # both binaries onto your PATH
```

The minimum supported Rust version is **1.87.0**, declared as `rust-version` in
`code/Cargo.toml` and enforced by its own CI job. The library and the two
binaries compile on 1.86, but the test fixtures use `u64::is_multiple_of`,
stabilised in 1.87 — so 1.87 is the floor for anything that has to pass the
gate, and one number is better than two. Raising it is a change a user can
notice, so it goes in the changelog.

## The test gate

Four commands. CI runs exactly these, on both `ubuntu-latest` and
`macos-latest`, and a pull request that fails any of them does not merge.

```sh
cd code
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

Three things about them are not obvious:

- **`--all-targets` is not optional.** Without it the integration suites, the
  benches and the second binary are not compiled at all, so a clean `cargo
  clippy` proves considerably less than it appears to.
- **`-D warnings` makes the pedantic clippy group binding**, along with the
  `unwrap_used` and `expect_used` denials in `code/Cargo.toml`. Panicking
  helpers are banned outside `#[cfg(test)]` because a panic mid-run leaves a
  library half-organised. Test modules carry a local `allow`; files under
  `tests/` are separate crates and need their own crate-level attribute:

  ```rust
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  ```

- **`cargo build --release` is in the gate for a reason.** The release profile
  sets `lto = true` and `codegen-units = 1`, which compiles code paths the dev
  profile never does.

CI adds three jobs beyond that gate:

| Job | What it enforces |
|---|---|
| `coverage` | Line coverage floors on the two files that move and record photographs: `src/organiser.rs` ≥ 92.0%, `src/journal.rs` ≥ 96.0%. A drop means a destructive branch stopped being exercised. Raise the floors when the real figure rises; never lower them to make a build green. |
| `fuzz` | Each of the four targets for 60 seconds from the checked-in corpus. It catches a regression seed crashing again and shallow new panics — it is not a campaign. |
| `msrv` | `cargo build --release` and `cargo test --all-targets` on the declared `rust-version`, so the number in `Cargo.toml` is a measurement rather than a hope. |

Mutation testing (`cargo mutants`) is **not** gated — a full run over the four
destructive modules takes about an hour. Run it by hand when you change one of
them; the baseline and the accepted survivors are in
[`docs/research/mutation-testing.md`](docs/research/mutation-testing.md).

## Changing a destructive path requires a failing test first

These are the destructive paths:

- `code/src/organiser.rs` — planning and executing moves, collision resolution,
  copy-verify-delete across volumes.
- `code/src/journal.rs` — the write-ahead record `mmm undo` replays.
- `code/src/undo.rs` — replaying it backwards.
- `code/src/sidecar.rs` — sidecars travelling with the file they belong to.
- The move-related paths of `code/src/main.rs` — everything between the plan
  and the summary.

**A change to any of them lands as a failing test first, then the change.** Two
commits, in that order, or one commit whose message shows the test failing
before and passing after. This is not ceremony. A test written after the fix
passes against the fixed code by construction, and there is no evidence
anywhere that it would have failed against the broken code — which is the only
property that matters when the failure mode is somebody's photographs going
missing. Where a bug is being fixed, quote the failure output in the pull
request.

Two corollaries the codebase already follows:

- **Verify in the failing direction.** Back the fix out, watch the new test
  fail by the assertion you intended, put the fix back. A test that passes for
  the wrong reason is worse than no test.
- **Prefer an injected seam to a mock.** Failure injection here follows the
  existing pattern — `Sink::Failing`, `MoveRecorder::failing_after(n)`,
  `find_duplicates_with` — which drives the real code down its real error path
  rather than testing a stand-in.

For everything else — reporting, configuration, docs, the geocoder — an
ordinary test alongside the change is fine.

## Fuzzing

The four parsers that read bytes somebody else wrote have targets under
`code/fuzz/`. If you change `metadata::parse_wall_clock`,
`metadata::parse_iso6709`, `xmp::parse` or `journal::parse_line`, run the
matching target locally for a few minutes:

```sh
cd code
cargo +nightly-2026-05-01 fuzz run parse_iso6709 \
  fuzz/scratch/parse_iso6709 fuzz/corpus/parse_iso6709 -- -max_total_time=180
```

The toolchain is pinned, for a reason recorded in
[`code/fuzz/README.md`](code/fuzz/README.md). Any input that crashes a target
is checked into `code/fuzz/corpus/<target>/` as a regression seed in the same
pull request as the fix.

## Commit messages and the changelog

- **Imperative subject, leading with a verb and the concrete artifact** —
  `Extract mmm library target so integration tests are possible`. No tool or
  agent markers, no `Co-Authored-By` trailers, no "generated with" footers.
- **Nothing is pushed without `CHANGELOG.md` updated in the same push.** Every
  user-visible change goes under `## [Unreleased]` against the right Keep a
  Changelog heading — Added, Changed, Deprecated, Removed, Fixed, Security.
- **A breaking change says so.** Prefix the entry `**BREAKING — …**` and state
  what an existing user's library or scripts will *do* on upgrade, not merely
  what changed.
- **State the limits of a fix.** If it is partial, the entry names the cases
  still uncovered. An entry that overclaims is worse than no entry.
- Purely internal changes — repository hygiene, ignore rules, tooling — need no
  entry. The test is whether a user could notice.

## Releasing

Pushing a `v*` tag is the whole release. `.github/workflows/release.yml` takes
it from there, in this order, and each step has to pass before the next starts:

| Job | Does |
|---|---|
| `verify` | Refuses the tag if `v<x.y.z>` does not match `version` in `code/Cargo.toml`, or if `CHANGELOG.md` has no `## [x.y.z]` section. Both take seconds, so a wrong tag fails before twenty minutes of builds. |
| `gate` | `cargo fmt --check`, `clippy --all-targets -- -D warnings`, `test --all-targets` on Linux and macOS, against the tagged commit. **A failing test blocks the release.** |
| `build` | `cargo build --release --target …` for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin` and `x86_64-apple-darwin`, each archived as a `.tar.gz` with a `.sha256` beside it. |
| `publish` | Creates the GitHub Release with the changelog section as its body and the six files attached. The only job with a write token. |

Two of the pieces are scripts rather than inline YAML, so they can be run
locally against a real build instead of being debugged through tag pushes:

```sh
.github/scripts/changelog-section.sh 0.2.0        # the release body, to stdout
cd code && cargo build --release --target aarch64-apple-darwin && cd ..
.github/scripts/package-release.sh aarch64-apple-darwin 0.2.0 dist
```

Both are covered by [`code/tests/release.rs`](code/tests/release.rs), which
runs in the ordinary suite — the alternative is discovering the extractor is
broken during the one minute a year it executes.

The binaries are not stripped by the workflow; `[profile.release]` sets
`strip = true` and the packaging script *checks* that it took effect, because
deleting that one line would otherwise ship debug symbols with nothing to say
so.

## Reporting a bug

Open an issue using the bug report template, which asks for the four things
that make a report reproducible: `mmm --version`, your OS, the exact command
you ran, and whether a journal file exists for the run.

**If files went missing, do not run anything else against that library** —
`mmm journal list <output>` is read-only and will name the run; the journal is
the record that lets it be put back.

Security issues do not go in the issue tracker. See [`SECURITY.md`](SECURITY.md).
