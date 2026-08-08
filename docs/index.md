---
type: reference
title: ThreeM Documentation Index
created: 2026-08-09
tags:
  - documentation
  - index
  - navigation
related:
  - '[[USER-GUIDE]]'
  - '[[TECHNICAL]]'
  - '[[configuration]]'
  - '[[format-support]]'
  - '[[journal-format]]'
  - '[[adr-001-dry-run-by-default]]'
  - '[[adr-003-atomic-move-semantics]]'
  - '[[adr-004-journal-design]]'
  - '[[adr-005-configuration-precedence]]'
  - '[[adr-006-timezone-handling]]'
  - '[[adr-007-parallel-hashing]]'
  - '[[coverage-report]]'
  - '[[mutation-testing]]'
  - '[[fuzzing]]'
  - '[[hashing-baseline]]'
  - '[[CHANGELOG]]'
  - '[[CONTRIBUTING]]'
  - '[[SECURITY]]'
---

# ThreeM Documentation Index

Every document in the repository, and what question each one answers. Names in
double brackets are graph links; the path beside each is the file.

## Start here

| Document | Path | Answers |
|---|---|---|
| [[USER-GUIDE]] | [`USER-GUIDE.md`](USER-GUIDE.md) | How do I run it, what does each flag do, what does the output mean, how do I undo a run? |
| [[TECHNICAL]] | [`TECHNICAL.md`](TECHNICAL.md) | How is it built — the two-pass architecture, the dedup cascade, move safety, the module map? |
| [[CHANGELOG]] | [`../CHANGELOG.md`](../CHANGELOG.md) | What changed, and which changes will break an existing library or script? |

## Reference

Stable descriptions of formats and settings — the tables you look things up in.

| Document | Path | Answers |
|---|---|---|
| [[configuration]] | [`reference/configuration.md`](reference/configuration.md) | Every config key, every `MMM_` environment variable, and a worked precedence example. |
| [[format-support]] | [`reference/format-support.md`](reference/format-support.md) | Which of the 32 scanned extensions a date can actually be read out of, which are verified by fixture, and what happens to the rest. |
| [[journal-format]] | [`architecture/journal-format.md`](architecture/journal-format.md) | The on-disk shape of a run journal — the record `mmm undo` replays. |

## Decisions (ADRs)

Why the tool behaves the way it does, and what was rejected. There is **no
ADR-002**: it was drafted conditionally, its condition did not occur, and the
number was left unused rather than recycled.

| Document | Path | Decides |
|---|---|---|
| [[adr-001-dry-run-by-default]] | [`decisions/adr-001-dry-run-by-default.md`](decisions/adr-001-dry-run-by-default.md) | Previewing is the default; `--commit` is the opt-in. The first of the two v0.2.0 breaking changes. |
| [[adr-003-atomic-move-semantics]] | [`decisions/adr-003-atomic-move-semantics.md`](decisions/adr-003-atomic-move-semantics.md) | `link()` + `unlink()` over `rename()`, and copy-verify-delete across volumes. |
| [[adr-004-journal-design]] | [`decisions/adr-004-journal-design.md`](decisions/adr-004-journal-design.md) | Write-ahead JSONL, one line per intent and one per outcome, flushed before the file moves. |
| [[adr-005-configuration-precedence]] | [`decisions/adr-005-configuration-precedence.md`](decisions/adr-005-configuration-precedence.md) | Four layers, in which order, and why `commit` may never be one of them. |
| [[adr-006-timezone-handling]] | [`decisions/adr-006-timezone-handling.md`](decisions/adr-006-timezone-handling.md) | An EXIF timestamp is a local wall clock, not UTC. The second v0.2.0 breaking change. |
| [[adr-007-parallel-hashing]] | [`decisions/adr-007-parallel-hashing.md`](decisions/adr-007-parallel-hashing.md) | Dedup phases 2 and 3 run on a pool the run owns, capped at 8 threads. |

## Research and quality reports

Measurements, with their gaps stated. Each of these is a snapshot with a date on
it, not a promise about the current commit.

| Document | Path | Reports |
|---|---|---|
| [[coverage-report]] | [`research/coverage-report.md`](research/coverage-report.md) | Line and region coverage per module, the CI floors, and every branch left uncovered with its reason. |
| [[mutation-testing]] | [`research/mutation-testing.md`](research/mutation-testing.md) | What `cargo-mutants` broke that the suite did not notice, which tests were added, and which survivors were accepted. |
| [[fuzzing]] | [`research/fuzzing.md`](research/fuzzing.md) | The four fuzz targets over the parsers that read untrusted bytes, the defect they found, and what is still unfuzzed. |
| [[hashing-baseline]] | [`research/hashing-baseline.md`](research/hashing-baseline.md) | Serial and parallel cascade throughput, run-to-run variance, and where the speedup does and does not hold. |

## Elsewhere in the repository

- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — the layout, the four-command test gate, the CI jobs beyond it, and the rule that a change to a destructive path lands as a failing test first.
- [`../SECURITY.md`](../SECURITY.md) — how to report a vulnerability, what is in scope for a local tool that opens no sockets, and the hardening already in place.
- [`../.github/ISSUE_TEMPLATE/bug_report.md`](../.github/ISSUE_TEMPLATE/bug_report.md) — the four things that make a bug report reproducible.
- [`../code/fuzz/README.md`](../code/fuzz/README.md) — how to run the fuzz targets, and why the toolchain is pinned.
- The phased build plan this project was written against lives under
  `.maestro/playbooks/`, with each phase's outcome recorded under its tasks. It
  is **not in the repository** — `.gitignore` keeps the agent tooling out — so
  it is named here rather than linked, and a clone will not have it.
