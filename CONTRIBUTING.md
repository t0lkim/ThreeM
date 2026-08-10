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
| `code/src/` | Library modules, `main.rs` (the `mmm` binary) and two more under `bin/`: `mmm_dedup_verifier.rs`, the independent verification channel, and `mmm_fixtures.rs`, the synthetic-library generator. |
| `code/src/fixtures.rs`, `code/src/generate.rs` | **Shipped library code, not test code.** They build the synthetic media and write the `EXPECTED.md` that goes with it. They began under `tests/` and moved into the library so `mmm-fixtures` could hand a user the same fixtures the suite runs against — so a change here is a change to a binary somebody installed, not to a test helper. |
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
cargo build --release          # binaries at code/target/release/{mmm,mmm-dedup-verifier,mmm-fixtures}
cargo install --path .         # all three binaries onto your PATH
```

The minimum supported Rust version is **1.87.0**, declared as `rust-version` in
`code/Cargo.toml` and enforced by its own CI job. It used to be a floor the
*tests* imposed — `u64::is_multiple_of`, stabilised in 1.87, was used only by
the fixtures under `tests/`, and the library and binaries themselves built on
1.86. That stopped being true when the fixtures moved into `src/fixtures.rs` to
ship as `mmm-fixtures`: the call is now in library code, so 1.87 binds the
library and every binary, not just the gate. The number did not move and the
`msrv` job measures it either way. Raising it is a change a user can notice, so
it goes in the changelog.

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
  benches and the two binaries under `src/bin/` are not compiled at all, so a
  clean `cargo clippy` proves considerably less than it appears to.
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
| `fuzz` | Each of the five targets for 60 seconds from the checked-in corpus. The job enumerates `cargo fuzz list`, so a new target is fuzzed on the next push with no workflow edit. It catches a regression seed crashing again and shallow new panics — it is not a campaign. |
| `msrv` | `cargo build --release` and `cargo test --all-targets` on the declared `rust-version`, so the number in `Cargo.toml` is a measurement rather than a hope. |

Mutation testing (`cargo mutants`) is **not** gated — the last whole-crate run
was 440 mutants and two hours at `-j 4`. Run it by hand when you change a
destructive module; the baseline and the accepted survivors are in
[`docs/research/mutation-testing.md`](docs/research/mutation-testing.md).

It is configured by [`code/.cargo/mutants.toml`](code/.cargo/mutants.toml), and
both settings in there matter to anyone running it. `src/fixtures.rs` and
`src/generate.rs` are excluded, because a mutated byte in a JPEG quantisation
table is a survivor that means nothing. `no_default_features` turns off the
`repository-tests` feature, which is the only thing that switches off
`tests/docs.rs` and `tests/release.rs`: `cargo-mutants` copies `code/` into a
temp directory, so those two targets find no repository above them and take the
*baseline* down rather than any mutant. Between them landing and that line,
mutation testing could not be run at all.

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

The five parsers that read bytes somebody else wrote have targets under
`code/fuzz/`. If you change `metadata::parse_wall_clock`,
`metadata::parse_iso6709`, `timezone::parse_offset`, `xmp::parse` or
`journal::parse_line`, run the matching target locally for a few minutes:

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
  what changed. When a version is cut, those entries are collected into a
  `### Breaking` section at the head of the release — ahead of Added and
  Changed, because it is the part a user upgrading has to read — and the now
  redundant prefix comes off. That heading is the one addition to the Keep a
  Changelog set; everything else keeps its name.
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
