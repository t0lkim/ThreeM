---
type: analysis
title: Test Coverage Report
created: 2026-08-08
tags:
  - testing
  - coverage
  - quality
related:
  - '[[hashing-baseline]]'
  - '[[journal-format]]'
  - '[[adr-003-atomic-move-semantics]]'
  - '[[adr-004-journal-design]]'
---

# Test Coverage Report

Measured with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) 0.8.7 over
`code/`, on macOS 15 (aarch64), rustc 1.92.0. The figures below are from
`cargo llvm-cov` — the default target set, which runs the library's unit tests
and every integration suite under `code/tests/`, including the ones that drive
the real `mmm` binary. Benchmarks are excluded; they measure throughput, not
behaviour.

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

## Figures

603 tests. Line coverage, whole crate: **91.48%**.

| Module | Regions | Lines | Functions | Bar |
|---|---|---|---|---|
| `organiser.rs` | 93.92% | **92.76%** | 96.37% | CI floor 92.0% |
| `journal.rs` | 95.62% | **96.88%** | 94.37% | CI floor 96.0% |
| `sidecar.rs` | 96.96% | **98.21%** | 93.48% | reported |
| `main.rs` | 86.85% | **90.05%** | 94.74% | reported |
| `naming.rs` | 94.69% | 97.07% | 100.00% | reported |
| `config.rs` | 93.97% | 96.38% | 99.03% | reported |
| `undo.rs` | 93.11% | 92.79% | 95.95% | reported |
| `settings.rs` | 90.99% | 94.33% | 97.35% | reported |
| `settings_report.rs` | 93.25% | 94.87% | 94.55% | reported |
| `xmp.rs` | 90.18% | 93.60% | 91.89% | reported |
| `scanner.rs` | 93.99% | 92.75% | 97.83% | reported |
| `geocoder.rs` | 93.33% | 91.43% | 80.00% | reported |
| `hasher.rs` | 92.44% | 89.46% | 89.12% | reported |
| `metadata.rs` | 86.73% | 87.97% | 88.89% | reported |
| `timezone.rs` | 87.69% | 86.15% | 87.88% | reported |
| `reporter.rs` | 89.05% | 87.56% | 100.00% | reported |
| `bin/mmm_dedup_verifier.rs` | 0.00% | 0.00% | 0.00% | **not covered at all** |
| **TOTAL** | **93.92%** → see below | **91.48%** | 94.09% | |

The `TOTAL` regions figure is 91.11%; the per-row regions column is per-file.

### Movement since this task began

| Module | Lines before | Lines after |
|---|---|---|
| `organiser.rs` | 86.20% | 92.76% |
| `journal.rs` | 94.44% | 96.88% |
| `sidecar.rs` | 95.88% | 98.21% |
| `main.rs` | 84.95% | 90.05% |
| whole crate | 89.82% | 91.48% |

## What was added

Twenty-six tests, all of them on branches that only run once something has
already gone wrong.

**`organiser.rs`** — sidecar move failure and sidecar journal failure; a
journal that dies after the photograph has moved (so the move is still
counted); a journal that cannot record a *failed* move; the duplicate pass
stopping on a journal failure, and on a duplicate's sidecar's journal failure;
a manifest that stops being writable mid-group, on the first line, and on a
sidecar line; every collision candidate taken; the link-is-impossible-so-copy
decision; `reserve_and_rename`'s fatal open and its placeholder cleanup after a
failed rename; `promote_into_place`'s fatal, collision, claim-and-rename and
unlink-source branches; a copy that reports success without writing anything; a
source that cannot be removed after a verified copy; both `Display` impls; the
`ChunkController` defaults.

**`journal.rs`** — an unreadable journal directory (an error, not an empty
list); a blank line inside a journal; a long corrupt line elided in the error,
and a short one quoted whole.

**`sidecar.rs`** — a media path and a sidecar path with no parent directory.

**`main.rs`** (through the real binary) — `undo --run` naming a run that was
never recorded, and one that was; `undo` in a library with no runs; `undo` of a
journal with no moves writing no journal of its own; chunking with
`--no-prompt`; the duplicate pass failing and the run still closing its
journal.

Two test-only failure injections were added, both following the pattern the
codebase already established with `Sink::Failing` and the injected `copy`
parameter on `copy_verify_delete` — the failure is introduced at the seam where
a real one would appear, because the real one is not reproducible:

- `MoveRecorder::failing_after(n)` — the journal accepts `n` writes and refuses
  every one after. This is what makes "the file moved and then the journal
  died" reachable, which is the single state `mmm undo` exists to survive.
- `MANIFEST_APPENDS_ACCEPTED` — a thread-local that makes a duplicate group's
  `manifest.txt` stop accepting appends after `n` lines. Thread-local rather
  than a `static`, so one test arming it cannot change what another executes.

## Uncovered lines, and why

Every uncovered line in the four bar-carrying modules is listed. Lines inside
`#[cfg(test)] mod tests` are excluded from this accounting throughout: they are
assertion *failure messages*, which by construction only render when a test
fails.

### `organiser.rs` — one line

| Line | What | Why |
|---|---|---|
| 595 | `Sink::Off => Ok(())` in `MoveRecorder::append` | Unreachable by construction. `intend` returns before appending when the sink is off, and `commit`/`failed` return before appending when there is no sequence number — which, with the sink off, there never is. Kept as the total match it has to be rather than an `unreachable!()`, because a panic here would be a panic in the middle of moving somebody's photograph. Commented at the site. |

### `journal.rs` — none

Every uncovered line is an assertion message inside the test module.

### `sidecar.rs` — one line

| Line | What | Why |
|---|---|---|
| 272 | the implicit `None` arm of `if let Some(parent) = parents.first()` | Unreachable: a key only exists in the map once something has been pushed under it, and the arm above has already diverted the `len() > 1` case. The alternative to stepping over it is an `unwrap` in the code path that pairs somebody's raw files with their edits. Commented at the site. |

### `main.rs` — five sites, all injection-only or outside the move paths

| Lines | What | Why |
|---|---|---|
| 106–108 | the `error!` inside `finish_journal` | Requires an already-open journal file descriptor to fail on its last write. Not reproducible portably; the equivalent stopping behaviour is proven in `organiser.rs` through `MoveRecorder::failing_after`. This branch only logs — it changes nothing about what moved. |
| 203–207 | the bail after a partial `undo` stopped by a journal failure | Same: needs the undo journal's descriptor to fail mid-run. `undo::execute_restore`'s handling of `journal_failed` is covered at unit level. |
| 522–524 | a `plan_move` that fails | `extract_metadata` falls back to filesystem metadata on every parse failure, so the only way to reach this is a file that vanishes between the scan and the plan. Not reachable through the binary. |
| 623, 679–683 | the progress bar and the bail after a run stopped by a journal failure | Same injection problem as 106–108. `process_moves` setting `journal_failed` is covered directly. |
| 267–269, 291–333, 377–408, 460–463 | verbosity mapping, `config` subcommand arms, scan-message pluralisation | Not move-related. Outside this task's bar; they are reported, not gated. |

### Not covered at all: `bin/mmm_dedup_verifier.rs`

The second binary — which re-reads a duplicate group's `manifest.txt` and
verifies each relocated file against the recorded digest — has **no tests**.
It is read-only by design and cannot move or delete anything, which is why it
is not in this task's scope, but 0% is 0%: nothing proves it parses the
manifest format `organiser.rs` writes, and nothing would catch the two drifting
apart. This is a real gap and it is recorded here rather than left for someone
to discover.

## Honest limits of this report

- **Measured on macOS only.** The `#[cfg(unix)]` tests run on both CI
  platforms, but the figures above are from one machine. The CI floors carry a
  one-point margin for that reason, stated in the workflow.
- **Line coverage, not branch coverage.** `cargo llvm-cov` reports no branch
  data for this crate (the branches column is empty throughout). A line with
  two outcomes can be fully "covered" having taken only one of them. Region
  coverage is the closer proxy and is reported alongside.
- **Coverage is not correctness.** Every figure here says a line ran, not that
  it did the right thing. What it did is the subject of the mutation-testing
  task that follows.

## Related

- [[hashing-baseline]] — throughput measurements for the dedup cascade
- [[journal-format]] — the record these tests assert the shape of
- [[adr-003-atomic-move-semantics]] — the move rules the failure paths implement
- [[adr-004-journal-design]] — why a move is recorded before it happens
