---
type: analysis
title: Test Coverage Report
created: 2026-08-10
tags:
  - testing
  - coverage
  - quality
related:
  - '[[mutation-testing]]'
  - '[[fuzzing]]'
  - '[[hashing-baseline]]'
  - '[[journal-format]]'
  - '[[adr-003-atomic-move-semantics]]'
  - '[[adr-004-journal-design]]'
---

# Test Coverage Report

Measured with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) 0.8.7
over `code/`, on macOS 26.5.2 (arm64), rustc 1.92.0. The figures are from
`cargo llvm-cov` — the default target set, which runs the library's unit tests
and every integration suite under `code/tests/`, including the ones that drive
the real binaries. Benchmarks are excluded; they measure throughput, not
behaviour.

This is a full re-measurement, not a patch of the previous numbers. The
generator moved out of `tests/` and into the library between the two runs, so
every figure in the 2026-08-08 edition was measured against a different set of
source files and none of them was carried forward.

Reproduce with:

```sh
cd code
cargo llvm-cov --summary-only                  # the table below
cargo llvm-cov report --show-missing-lines     # the uncovered line numbers
```

## Why only some modules have a bar

ThreeM moves other people's photo libraries. The modules that perform or record
a move — `organiser.rs`, `journal.rs`, `sidecar.rs`, and the move-related paths
of `main.rs` — are the ones where an unexercised branch means a destructive
path shipped without ever having run. Those four are the subject of this
report, and two of them carry a CI floor. Every other module is measured and
reported here, but a drop in `settings_report.rs` is a documentation problem;
a drop in `organiser.rs` is a data-loss problem.

Nothing was added to that list this time, and the reasoning for that decision is
in [Does the generator get a floor?](#does-the-generator-get-a-floor) below.

## Figures

**665 tests.** Line coverage, whole crate: **92.89%**. Regions 92.61%,
functions 93.97%.

| Module | Regions | Lines | Functions | Bar |
|---|---|---|---|---|
| `organiser.rs` | 93.63% | **92.74%** | 96.14% | CI floor 92.0% |
| `journal.rs` | 95.38% | **96.71%** | 94.37% | CI floor 96.0% |
| `sidecar.rs` | 97.61% | **98.57%** | 95.65% | reported |
| `main.rs` | 87.14% | **90.37%** | 94.74% | reported |
| `fuzz.rs` | 100.00% | 100.00% | 100.00% | reported |
| `fixtures.rs` | 96.61% | 97.16% | 87.21% | reported — see below |
| `naming.rs` | 94.55% | 97.07% | 100.00% | reported |
| `bin/mmm_dedup_verifier.rs` | 89.49% | 96.70% | 60.00% | reported |
| `config.rs` | 93.97% | 96.38% | 99.03% | reported |
| `generate.rs` | 96.87% | 95.83% | 97.01% | reported — see below |
| `settings_report.rs` | 93.25% | 94.87% | 94.55% | reported |
| `settings.rs` | 90.57% | 93.61% | 97.37% | reported |
| `undo.rs` | 93.11% | 92.79% | 95.95% | reported |
| `scanner.rs` | 93.77% | 92.75% | 97.83% | reported |
| `xmp.rs` | 88.51% | 91.71% | 92.11% | reported |
| `geocoder.rs` | 91.67% | 91.43% | 80.00% | reported |
| `metadata.rs` | 88.60% | 88.68% | 92.86% | reported |
| `hasher.rs` | 91.83% | 88.43% | 88.16% | reported |
| `reporter.rs` | 88.11% | 86.45% | 100.00% | reported |
| `timezone.rs` | 87.69% | 86.15% | 87.88% | reported |
| `bin/mmm_fixtures.rs` | 70.32% | **74.16%** | 45.45% | reported — lowest in the crate |
| **TOTAL** | **92.61%** | **92.89%** | 93.97% | |

Only files under `code/src/` are instrumented; nothing under `code/tests/`
appears in the report, at either measurement.

## Movement since the 2026-08-08 report

| Module | Then (603 tests) | Now (665 tests) | Δ |
|---|---|---|---|
| `organiser.rs` | 92.76% | 92.74% | −0.02 |
| `journal.rs` | 96.88% | 96.71% | −0.17 |
| `sidecar.rs` | 98.21% | 98.57% | +0.36 |
| `main.rs` | 90.05% | 90.37% | +0.32 |
| `naming.rs` | 97.07% | 97.07% | — |
| `config.rs` | 96.38% | 96.38% | — |
| `undo.rs` | 92.79% | 92.79% | — |
| `scanner.rs` | 92.75% | 92.75% | — |
| `geocoder.rs` | 91.43% | 91.43% | — |
| `timezone.rs` | 86.15% | 86.15% | — |
| `settings_report.rs` | 94.87% | 94.87% | — |
| `settings.rs` | 94.33% | 93.61% | −0.72 |
| `xmp.rs` | 93.60% | 91.71% | −1.89 |
| `metadata.rs` | 87.97% | 88.68% | +0.71 |
| `hasher.rs` | 89.46% | 88.43% | −1.03 |
| `reporter.rs` | 87.56% | 86.45% | −1.11 |
| `bin/mmm_dedup_verifier.rs` | **0.00%** | **96.70%** | **+96.70** |
| `fixtures.rs` | — | 97.16% | new |
| `generate.rs` | — | 95.83% | new |
| `bin/mmm_fixtures.rs` | — | 74.16% | new |
| `fuzz.rs` | — | 100.00% | new to the table |
| **whole crate** | **91.48%** | **92.89%** | **+1.41** |

**The whole-crate rise is not the generator flattering the total.** Excluding
`fixtures.rs` and `generate.rs` entirely, the remaining files come to
**92.61%** — so the crate as it stood improved by 1.13 points on its own merits
(overwhelmingly the verifier going from 0% to 96.70%), and the two new library
modules added a further 0.28. The generator is well covered, but it is not what
moved the number.

**`fuzz.rs` was never in the old table.** It existed at that commit; it was
simply omitted. It is 100% covered and is 32 counted lines.

### The comparison is looser than it looks

The published 2026-08-08 figures were measured at **603 tests**, before the
mutation-testing task that followed added six more, and the report was never
re-measured afterwards. So a Δ in this table spans the fixtures move *and* the
mutation tests *and* two releases of feature work. Treat the small movements as
direction, not as attribution.

## Do the floors still hold?

Yes, and by more than the movement:

| Module | Floor | Measured | Margin |
|---|---|---|---|
| `organiser.rs` | 92.0% | 92.74% | +0.74 |
| `journal.rs` | 96.0% | 96.71% | +0.71 |

Checked with the same `jq` expression the `coverage` job in
`.github/workflows/ci.yml` runs, against this run's `coverage.json`, rather than
by reading the table above.

**Both figures moved, so per the task both were chased rather than waved
through.** Neither moved because of the fixtures relocation — that changed
`tests/`, and per-file coverage of `src/organiser.rs` cannot see it. Both moved
because the files themselves grew:

- **`organiser.rs` gained ~490 net lines** across four commits since the
  reported measurement (`6c9d68f`, `cdb1bc7`, `47e4a85`, `cd5b482`). Two new
  uncovered clusters came with them, both confirmed by `git blame` rather than
  inferred:
  - **line 391** (`47e4a85`, the dry-run fix) — the collision ledger giving up
    after ten thousand candidates claim one name and predicting the unsuffixed
    destination. `execute_move` refuses at the same point and reports it
    properly; this is the preview agreeing with it.
  - **lines 1691–1696** (`cdb1bc7`, the verifier fix) — the `error!` emitted
    when a retained original's `manifest.txt` cannot be reopened to record where
    it landed. Deliberately non-fatal: the photograph has already moved and is
    in the journal, and halting here would trade a real library for a report.
- **`journal.rs` changed 8 lines** in the same span. It has **no uncovered
  non-test lines at all** — every line in its missing list (551, 648, 685, 829,
  835, 943, 955, 1022, 1026, 1033) is an assertion message inside
  `#[cfg(test)] mod tests`, which begins at line 503. The −0.17 is entirely a
  denominator effect from those 8 lines.

The old report's single uncovered `organiser.rs` line — 595, `Sink::Off =>
Ok(())` — is **still uncovered and still the same line**, now renumbered to
**758**. It has not been fixed or forgotten; the file simply grew above it.

**Neither floor is being raised.** 92.0 and 96.0 still sit roughly three
quarters of a point under the measured figures, which is the margin they were
designed with and the margin they still have. Raising them to chase +0.7 would
buy nothing and would make the next platform difference a CI failure.

### Two unchanged files whose figures moved anyway

`hasher.rs` (−1.03) and `sidecar.rs` (+0.36) are **byte-identical** to the
commit the old figures were measured at — `git diff` reports no change to
either. Their coverage moved because what the surrounding suite incidentally
executes moved: tests were added, and 1,350 lines were relocated out of
`tests/common/mod.rs`.

**This was not run to ground.** The old report recorded per-line detail only for
the four bar-carrying modules, so attributing `hasher.rs`'s missing point would
need a coverage run at the old commit, and that was outside this task. It is
recorded here as an open thread rather than smoothed over: an unchanged file
losing a point of coverage is a small thing, but it is a thing nobody has
explained.

## Does the generator get a floor?

**No. `fixtures.rs` and `generate.rs` are reported, not gated.** Stating that
plainly, because the task is right that a figure with no floor is a figure and
not a gate, and calling 97.16% a "bar" when nothing enforces it would be the
sort of overclaim this report exists to avoid.

Three reasons, in the order they matter:

1. **It fails the test the floors were built around.** The bar exists for one
   stated reason: a drop means a destructive branch stopped being exercised.
   `fixtures.rs` and `generate.rs` write files into a directory the user asked
   to be filled with disposable data. They never move, rename, overwrite or
   delete anything of the user's — `mmm-fixtures` refuses a non-empty directory
   without `--force` precisely so it cannot. The worst outcome of a regression
   here is a bad demo library.
2. **The coverage is incidental, so a floor would gate on the wrong thing.**
   Nothing asserts these modules' own branches. They are at 97.16% and 95.83%
   because every integration suite in the project uses them as scaffolding —
   which means deleting an unrelated organise test moves the figure. A floor
   fed by that is a tripwire on the shape of other people's test files, and the
   first time it fires it will be for a reason that has nothing to do with the
   generator.
3. **A far stronger gate already exists.** `tests/generated_library.rs`
   generates a library, organises it with the real binary, and asserts every
   file the generator predicted landed where it was predicted to. That is a
   behavioural claim about the *document users are handed*, and it is worth
   more than any line percentage — it is what caught the defect recorded below,
   which no coverage floor would have noticed at any threshold.

`fixtures.rs`'s nine uncovered lines are the `Default for MediaTree` impl
(100–102, nothing calls it), the panic arm of a fixture write (474), the
"journal directory unreadable" fallback (602), two environment guards that only
return early when the OS declines to make a directory read-only (751–752, 773),
and one assertion message (1038). `generate.rs`'s eleven non-test uncovered
lines are the `Stress` profile's summary string (90, no test builds 600 files),
`Rng::below`'s `n == 0` arm (156), and eight `_ => String::new()` fallbacks in
the `EXPECTED.md` section renderers (801, 817, 832, 849, 863, 883–885) — each
unreachable by construction, since the closure only ever runs on entries its own
predicate already selected.

## `bin/mmm_dedup_verifier.rs`: 0% → 96.70%

The gap the last report recorded as real, and open, is closed. The verifier had
no tests of any kind; it now has **96.70% line coverage**, and exactly two
uncovered lines — 75 and 76, the `1 => "info"` and `_ => "debug"` arms of the
`--verbose` mapping.

Two qualifications, because the headline number is better than the situation:

- **Functions is 60.00%** — four of ten uncovered, against two uncovered lines.
  `llvm-cov` counts closures as functions, so the two numbers are counting
  different things; the line figure is the one that describes the binary's
  behaviour here.
- **Coverage is not an argument that this binary is correct.** A test suite can
  execute every line of a tool that reaches the wrong verdict, and this binary
  is the case history for it: it once confirmed zero groups against a stale
  manifest, printed an all-clear and exited 0. **That was fixed in 0.3.0** —
  the organise pass appends `# Original moved to:` and the verifier prefers it,
  and a run that confirms nothing now exits 1. *(Corrected 2026-08-10: this
  paragraph originally said the defect was still open, citing the v0.2.0
  readiness report, which describes a release two versions back. The
  reasoning was right and the example was two releases stale.)*

## `bin/mmm_fixtures.rs`: 74.16%, and why that is not the same kind of number

The lowest figure in the crate, and the one worth the least alarm. **Its failure
mode is a bad demo library, not a lost photograph** — it writes synthetic files
into a directory the user nominated for exactly that, and refuses a non-empty
one without `--force`. A regression here wastes somebody's afternoon. A
regression in `organiser.rs` costs them a photograph. Holding the two to one
standard would be a category error, and inflating this number to look tidy would
consume effort the destructive paths have a better claim to.

Its eighteen uncovered lines are four clusters:

| Lines | What | Why |
|---|---|---|
| 74, 77–79, 81 | `--list-profiles` | No test invokes it. |
| 124, 125, 129 | `prepare_directory` creating a missing directory, and refusing a path that exists and is not a directory | Tests hand it an existing temp directory. |
| 157–161 | `fresh_seed()` | Every test passes `--seed`, which is the point of `--seed`. |
| 183–187 | the trailer printed for `--profile awkward` | **Named as a gap, not a curiosity.** This is the message telling a user that the malformed files are deliberate. Nothing runs `mmm-fixtures --profile awkward` — the awkward-profile test drives `mmm` over a library built in-process — so the one sentence standing between a correct result and a wrongly-filed bug report is never executed by anything. |

## A defect found while taking this measurement

The measurement did not complete on the first attempt: `cargo llvm-cov` aborted
because `tests/generated_library.rs::every_definite_expectation_is_met` failed,
claiming `EXPECTED.md` predicted `2026-08-10/` for a file that landed in
`2026-08-09/`.

**The generator was writing `EXPECTED.md` in two timezones at once.** Dated
fixtures carry an explicit `+00:00` and were predicted at their wall clock as
written — true only under `--timezone UTC`. Filesystem-dated fallbacks were
predicted by reading the file's mtime in `DateTime<Local>` — the machine's zone.
On a machine already running UTC the two agree and everything passes, which is
why CI's UTC leg never saw it. East of Greenwich they disagree for the eight
hours after local midnight. It was 05:36 in Singapore.

Two consequences, and the second is worse than the flake:

1. The suite fails for eight hours a day on CI's `Asia/Singapore` leg — a
   release gate that is red a third of the time and green if you retry later.
2. **`EXPECTED.md` told the reader to organise the library with no `--timezone`
   at all**, and so did the "Try it:" block `mmm-fixtures` prints, and so did
   the examples in `README.md` and `docs/USER-GUIDE.md`. A reader in any non-UTC
   zone who followed those instructions got a correct run and a document
   declaring every dated file misfiled — against a section that says in terms
   "if one of them lands anywhere else, that is a defect".

Fixed by putting the whole document in one frame: the filesystem fallback is now
read in UTC, and every surface that tells a reader how to organise the generated
library says `--timezone UTC` and says why. Guarded by two new assertions —
a unit test pinning the fallback to UTC (with instants chosen to land on a
different calendar day under the `Asia/Singapore` and `America/New_York` legs of
the CI matrix, so a revert to local time fails there rather than nowhere), and a
check that the binary's printed commands and the document it writes both carry
the flag. Both verified in the failing direction.

The figures in this report were taken after that fix, on a suite of 665 tests
passing with `fmt`, `clippy --all-targets -D warnings` and
`test --all-targets` clean.

## Honest limits of this report

- **Measured on macOS only.** The `#[cfg(unix)]` tests run on both CI
  platforms, but these figures are from one machine. The CI floors carry a
  one-point margin for that reason, stated in the workflow.
- **Line coverage, not branch coverage.** `cargo llvm-cov` still reports no
  branch data for this crate — the branches column is empty throughout. A line
  with two outcomes can be fully "covered" having taken only one of them.
  Region coverage is the closer proxy and is reported alongside.
- **Coverage is not correctness.** Every figure says a line ran, not that it did
  the right thing. The defect above is the case in point: `generate.rs` measured
  95.83% while emitting a document that contradicted itself, and the line that
  did it was covered.
- **One measurement, one commit.** These are not tracked over time and nothing
  alerts on a drift that stays above the floors.
- **The `hasher.rs` movement is unexplained**, as recorded above.

## Related

- [[mutation-testing]] — whether the covered lines are actually asserted on
- [[fuzzing]] — the parsers that read bytes somebody else wrote
- [[hashing-baseline]] — throughput measurements for the dedup cascade
- [[journal-format]] — the record these tests assert the shape of
- [[adr-003-atomic-move-semantics]] — the move rules the failure paths implement
- [[adr-004-journal-design]] — why a move is recorded before it happens
