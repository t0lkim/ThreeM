---
type: analysis
title: Mutation Testing Report
created: 2026-08-10
tags:
  - testing
  - mutation
  - quality
related:
  - '[[coverage-report]]'
  - '[[hashing-baseline]]'
  - '[[fuzzing]]'
  - '[[journal-format]]'
---

# Mutation Testing Report

Measured with [`cargo-mutants`](https://github.com/sourcefrog/cargo-mutants) 27.1.0,
rustc 1.92.0, macOS 26.5.2 on an 8-core arm64 machine. **440 mutants, 2 hours at
`-j 4`.** This supersedes the 2026-08-09 report measured at v0.2.0; that run
covered four modules and 313 mutants, and its figures are quoted below only for
comparison.

Two things changed since it. The scope now includes `undo.rs` and `sidecar.rs` —
the two destructive modules the previous run named as a gap and did not measure —
and the crate has taken two releases of feature work, so the four original
modules generate 343 mutants where they generated 313.

Reproduce with:

```sh
cd code
cargo install cargo-mutants --locked
cargo mutants -j 4 \
  --file src/organiser.rs --file src/journal.rs \
  --file src/hasher.rs --file src/metadata.rs \
  --file src/undo.rs --file src/sidecar.rs
```

Configuration lives in [`code/.cargo/mutants.toml`](../../code/.cargo/mutants.toml),
which is read automatically and does two things this run depends on. It excludes
`src/fixtures.rs` and `src/generate.rs`, because a mutated byte in a JPEG
quantisation table produces a fixture that is differently shaped rather than
wrong and nothing asserts on the shape of a fixture — dozens of unkillable
survivors would hide the real ones. And it passes `no_default_features`, which
switches off `tests/docs.rs` and `tests/release.rs`: `cargo-mutants` copies the
package into a temp directory, so those two read repository-root files that do
not exist during a run, and they took the *baseline* down with them. Between
those targets landing and that line, mutation testing could not be run at all.
Neither target executes a line of any module measured here, so the exclusion
costs no detection.

## Why this exists, given the coverage report

[`coverage-report.md`](coverage-report.md) ends on the reason. `cargo llvm-cov`
reports no **branch** data for this crate, so a line counted as covered may have
taken only one of its two outcomes — and a line that ran is not a line whose
result anything looked at. Mutation testing asks the stronger question directly:
change the code so it is wrong, and see whether a test notices.

## Figures

Baseline sweep — the suite as it stood at 665 tests, before anything in this
report was written:

| Module | Mutants | Caught | Missed | Unviable | Timeout | Score |
|---|---|---|---|---|---|---|
| `hasher.rs` | 62 | 39 | 4 | 18 | 1 | 88.64% |
| `journal.rs` | 47 | 42 | **0** | 5 | 0 | **100.00%** |
| `metadata.rs` | 102 | 83 | 3 | 16 | 0 | 96.51% |
| `organiser.rs` | 132 | 115 | 5 | 12 | 0 | 95.83% |
| `sidecar.rs` | 20 | 17 | 1 | 2 | 0 | 94.44% |
| `undo.rs` | 77 | 55 | **14** | 8 | 0 | **79.71%** |
| **Total** | **440** | **351** | **27** | **61** | **1** | **92.61%** |

Score is caught ÷ viable, where viable excludes the mutants that did not compile
and counts the one timeout against us.

After the three tests this run produced:

| | Baseline | After |
|---|---|---|
| Caught | 351 | **356** |
| **Missed** | **27** | **22** |
| Mutation score | 92.61% | **93.93%** |
| `organiser.rs` | 95.83% | **97.50%** |
| `undo.rs` | 79.71% | **84.06%** |

Tests: 665 → **668**. Full gate green afterwards — `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`.

**The "After" column is not a second full sweep.** Re-running 440 mutants to
re-confirm 427 unchanged outcomes is two more hours for no information, so the
five mutants these tests target were re-run scoped by regex — 13 mutants, 5
minutes, `12 caught, 1 unviable, 0 missed` — and the rest are carried across
from the baseline. This is the same shortcut the v0.2.0 report took, and it is
stated for the same reason: the number is a substitution, not a measurement of
the whole.

### Against the v0.2.0 run

Comparable only over the four modules both runs covered:

| | v0.2.0 (2026-08-09) | This run | After |
|---|---|---|---|
| Mutants | 313 | 343 | 343 |
| Caught | 257 | 279 | 281 |
| Missed | 8 | 12 | 10 |
| Score | 96.98% | 95.55% | **96.23%** |

The four modules grew by 30 mutants across two releases, and the score fell
1.43 points before this run's tests and 0.75 after. **The v0.2.0 96.98% was
itself a substituted figure**, not a full sweep, so this comparison is direction
rather than arithmetic.

## The eight accepted survivors, re-checked one at a time

The v0.2.0 report accepted eight survivors and one timeout with a stated reason
for each. The instruction for this run was to check that each is still the same
survivor with the same reason, rather than re-accept a count. **All eight are,
at the same site, with the same mutation, for the same reason** — and the
timeout is still the same timeout:

| v0.2.0 site | Today | Still the same reason? |
|---|---|---|
| `hasher.rs` `STREAM_BUFFER_BYTES` `128 * 1024` → `+` | `hasher.rs:25:40` | Yes — a read-buffer length; every digest is byte-identical at any buffer size. |
| `hasher.rs` `candidate_groups += 1` → `*=` | `hasher.rs:407:30` | Yes — the counter is read only by a `debug!` field. |
| `hasher.rs` duplicate-group tie-break, delete arm | `hasher.rs:556:17` | Yes — unreachable by construction; commented at the site. |
| `hasher.rs` `if skipped > 0` → `<` | `hasher.rs:564:16` | Yes — suppresses a `warn!`; the count itself is asserted. |
| `metadata.rs` `"CreateDate" \| "DateTimeOriginal"`, delete arm | `metadata.rs:631:13` | Yes — dead against `nom-exif` 1.x, and the pin is still `nom-exif = "1"`. |
| `organiser.rs` `has_location` `&&` → `\|\|` | `organiser.rs:150:47` | Yes — the coordinate-pair invariant still holds; commented at the site. |
| `organiser.rs` `ChunkController::chunk_started` | `organiser.rs:1599:9` | Yes — equivalent mutant, default body discards its arguments. |
| `organiser.rs` `ChunkController::should_continue` | `organiser.rs:1608:9` | Yes — equivalent mutant. |
| `hasher.rs` `hash_reader` `== 0` → `!=` (timeout) | `hasher.rs:841:23` | Yes — still a non-terminating loop, still detected by the suite hanging. |

Nothing on that list quietly became a different mutant, and nothing on it was
re-accepted on the strength of the count matching.

## What this run found

### Two mutants in the collision ledger, and a preview test that could not see them

`DestinationLedger::seed_from_disk` could be replaced with `()`, or have its `!`
deleted, and all 665 tests passed. That function is what makes the plan account
for names **already on disk**, and the ledger's own doc comment calls a run into
an already-organised library "the ordinary case for this tool".

Without it the ledger starts empty, so it predicts the unsuffixed name as free
while `move_no_clobber` — still the arbiter — produces the suffixed one. The
preview then names a path the run does not create, and does so specifically
about a file already sitting in the output tree, which reads as *that one is
about to be overwritten*. That is the exact defect the ledger was added in 0.3.0
to fix.

It survived because **every existing preview test organises into an empty
directory**, including the one written for the 0.3.0 fix. That test builds its
collision out of files from a single run, so it is satisfied by a ledger that
knows only about its own claims. `the_preview_accounts_for_names_already_in_the_output_tree`
organises twice into the same output directory and asserts the second preview
names the suffixed path, that the commit produces exactly what the preview
named, and — by provenance marker rather than by filename — which of the two
files ended up where. Both mutants were injected by hand and both fail it on the
intended assertion, printing the unsuffixed name against the suffixed one.

### `undo.rs` is the least-verified module in the crate

Its first mutation run at all, and it scores **79.71%** against 88–100% for
everything else. Fourteen survivors, three of which are closed here.

`Verification::is_intact` could return `true` for everything or `false` for
everything with nothing failing, because **nothing in the crate calls it** —
`execute_restore` matches on the variants directly. This is the same shape as
`DateSource::is_filesystem` in the previous run, and it is resolved the same
way: it is public API on a public enum and its contract is the one sentence undo
rests on, so it is pinned by a test over all four variants rather than deleted,
and its lack of an in-crate caller is recorded here as the finding.

More seriously, `verify_step`'s `NotFound` guard could be widened to match every
error, so that a file this process cannot inspect returns `Missing` instead of
`Unverifiable`. Those two verdicts are counted differently — `skipped_missing`
against `failed` — and printed differently, and the difference is the whole of
`Unverifiable`'s doc comment: *the check itself could not be made, so nothing is
known either way*. `Missing` tells somebody mid-recovery that the file the run
left there is gone, which is a statement about their photograph. A file sitting
safely inside a directory this process cannot search is not that. The `NotFound`
path was tested; the path where the question cannot be asked at all was not.
`a_file_that_cannot_be_inspected_is_not_reported_as_missing` locks a directory
to `0o000` and asserts the verdict, skipping with a printed reason if the
process turns out to ignore permission bits.

**Every one of the three tests was verified in the failing direction** — five
injections, five catches by the intended assertion, each source restored and
`diff`ed byte-identical afterwards.

## Surviving mutants, and why each is accepted

Twenty-two, plus the one timeout. None can change where a file goes.

### The four original modules — ten, plus the timeout

Nine are the v0.2.0 list above, unchanged. Two are new since that run, both
arriving with 0.3.0 feature work:

| Site | Mutation | Why it is accepted |
|---|---|---|
| `metadata.rs:280` keeping a location whose date fell back | `&&` → `\|\|` | Unreachable, and equivalent besides. `get_gps_info` yields a record holding both coordinates or no record at all, and `parse_iso6709` returns a pair or `None`, so nothing reaching here has one without the other — the same invariant `has_location` rests on. Both fields are assigned together below the guard, so even a half-located file would be refused downstream. Now commented at the site. |
| `metadata.rs:887` the non-finite coordinate guard | `\|\|` → `&&` | Equivalent. The range check on the next line already refuses every non-finite value: `RangeInclusive::contains` is false for `NaN` and for both infinities — **measured, not reasoned**. The guard exists for the message rather than the verdict, so a coordinate that is not a number and one that is off the planet are distinguishable at `-vv`. Now commented at the site. |

### `undo.rs` — eleven

| Site | Mutation | Why it is accepted |
|---|---|---|
| `521`, `592`, `608`, `612` — four counters in `execute_restore` | `+=` → `-=` and `*=` (eight mutants) | **Accepted for now, not argued to be safe.** Every one is in an error branch — a file that could not be checked, a restore that failed, a journal that could not be written — so reaching them needs failure injection the undo suite does not yet have. The main `restored += 1` on the success path *is* caught. See the gap below. |
| `680` — `prune_empty_dirs`'s `NotFound` guard | guard → `true`, guard → `false`, `==` → `!=` (three mutants) | Equivalent, by an invariant worth writing down: **a directory that still holds an entry makes every one of its ancestors non-empty too**, so `remove_dir` refuses all of them. Continuing to climb past a refusal can therefore only waste syscalls, never remove something that should have stayed; and stopping early cannot strand an empty ancestor, because whichever pass empties that ancestor climbs to it from below. |

### `sidecar.rs` — one

| Site | Mutation | Why it is accepted |
|---|---|---|
| `185` `SidecarIndex::empty` | → `Default::default()` | Equivalent in the strictest sense: the body **is** `Self::default()`. There is no program here to tell apart. |

## What this does not cover

- **The undo counters are a real gap and this run does not close it.** Eight
  survivors across four `+=` sites mean the undo summary's `failed` and
  `restored` figures are unasserted on every error path. A user who runs
  `mmm undo` reads those numbers to decide whether their library came back.
  Closing it needs the failure-injection seams the organiser already has
  (`Sink::Failing`, `MoveRecorder::failing_after`) extended to the restore loop —
  a change to `undo.rs`'s test surface, not an afternoon's assertions, which is
  why it is named here as scheduled work rather than done badly in passing.
- **Nine modules have still never been mutated:** `naming.rs`, `config.rs`,
  `settings.rs`, `settings_report.rs`, `scanner.rs`, `xmp.rs`, `reporter.rs`,
  `timezone.rs`, `geocoder.rs`. `undo.rs` and `sidecar.rs` have come off this
  list; nothing else has.
- **`fixtures.rs` and `generate.rs` are excluded by configuration** and
  therefore unmeasured. The argument for the exclusion is in the config file and
  repeated above; the stronger check on that code is
  `tests/generated_library.rs`, which asserts the generator's own predictions
  against the real binary's behaviour.
- **Still no CI gate.** Two hours a run is too slow for a pull request, and
  `--in-diff` mode (mutating only changed lines) has still not been evaluated.
  Nothing stops a new surviving mutant from appearing — which is exactly what
  happened between the two runs, twice.
- **`--baseline` is the same suite.** A mutant is "caught" if the suite fails,
  which includes failing for the wrong reason. Every kill claimed above was
  confirmed by injecting the mutation by hand and reading which assertion fired,
  but that is a manual check over five mutants, not over 351.
- **The figures are macOS.** `#[cfg(unix)]` tests run — including the new
  permission-denied one, which is skipped rather than failed if the process
  ignores permission bits. Nothing Windows-specific was mutated or exercised.
