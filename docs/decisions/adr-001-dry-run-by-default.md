---
type: decision
title: Dry-run by default
created: 2026-08-08
tags:
  - safety
  - cli
  - breaking-change
related:
  - '[[CHANGELOG]]'
  - '[[USER-GUIDE]]'
---

# ADR-001: Dry-run by default

**Status:** Accepted
**Date:** 2026-08-08

## Problem

`mmm` moves files. It walks one or more directories, renames every media file it finds into a `YYYY/MM/DD/` hierarchy, and relocates every file it judges to be a duplicate into a `duplicates/` tree. When no `--output` is given, the output directory defaults to the *first input directory* — so the most natural invocation a person will type, `mmm ~/Photos`, rewrites the layout of `~/Photos` in place.

Until this change that invocation was destructive by default. Previewing required remembering to add `--dry-run`. The failure mode is therefore:

- **Asymmetric.** Forgetting `--dry-run` reorganises a photo library. Forgetting `--commit` prints a list. One of those is recoverable by pressing up-arrow.
- **Silent at the point of mistake.** There was no confirmation before the first move. The chunk prompt fires *between* batches, and `--no-prompt` removes it entirely.
- **Irreversible in practice.** Moves are `rename()` on the same volume and copy-verify-delete across volumes. There is no undo log, no manifest of the organise pass, and no journal to replay backwards. A wrong run is undone by hand, over thousands of files, from memory.
- **Aimed at exactly the wrong data.** The intended input is a personal photo and video library — years of irreplaceable files, usually someone else's when the tool is recommended to a friend or family member.

A tool whose accidental invocation is unrecoverable and whose deliberate invocation is one extra word has its defaults inverted.

## Decision

**Previewing is the default. Moving files requires `--commit`.**

- `--dry-run` is removed as live state and replaced by `--commit` (`default false`). Any run that does not pass `--commit` scans, plans, prints and exits without touching a file.
- `Config::is_dry_run()` returns `!self.commit`. The existing early-return branch in `main` is unchanged; only the condition inverts. This deliberately keeps the change to the *posture*, not to the pipeline.
- A posture banner is printed **before the scan**, not after it — `DRY RUN — no files will be modified. Re-run with --commit to apply.` or `COMMIT MODE — files will be moved.` Learning which mode you were in from the aftermath is not a safety feature. Both strings live as `reporter::{DRY_RUN_BANNER, COMMIT_BANNER}` so tests assert against the constant rather than a copied literal.
- `--help` states the posture in three places: a `SAFE BY DEFAULT` paragraph in `long_about`, the `--commit` flag's own help text, and a `SAFETY` + `EXAMPLES` block in `after_help`. Two unit tests pin this so the posture cannot be quietly dropped from the CLI's own documentation.

## Deprecation path for `--dry-run`

`--dry-run` (and its short form `-d`) stays **accepted as a hidden no-op**, not removed.

- Removing it would make every existing script die on an unknown argument — including the read-only ones, which are precisely the invocations that were already doing the safe thing. Punishing careful users for a change made on their behalf is the wrong trade for one saved struct field.
- The short form is kept as well as the long. Hiding only `--dry-run` would still break `mmm ~/Photos -d`, which is the same class of script.
- The field is named `dry_run_deprecated` so the retired flag cannot be mistaken for live state at a call site.
- Passing it emits a one-line notice on **stderr** (`config::DRY_RUN_DEPRECATION_NOTICE`), while the banner goes to stdout — a script piping stdout into a planner is not corrupted by the warning. `deprecation_notice()` returns the string rather than printing it, so the decision is unit-testable; `main` owns the single `eprintln!`.
- **`--dry-run --commit` resolves to commit.** The combination is contradictory but reachable — someone bolts `--commit` onto an old invocation. The retired flag is a no-op, not a veto, so the explicit current flag wins. A silent no-op would be the worse outcome, which is what the stderr notice is for. Pinned by a test rather than left to be rediscovered.

No removal date is set. The flag costs one hidden field and one branch; it can be dropped at the next major version if it ever becomes a burden.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Keep destructive-by-default, print a loud warning first | A warning does not undo a `rename()`. It also trains people to ignore it, since it would fire on every legitimate run. |
| Interactive confirmation before the first move, no flag change | Not scriptable, and the tool already ships `--no-prompt` — which would immediately restore the destructive default for anyone automating it. |
| Hard-error on a bare invocation, demanding `--commit` or `--dry-run` explicitly | Breaks every existing script, including the safe ones, and gives the user an error where a useful preview would do. The preview *is* the correct default behaviour, so refusing to run is strictly worse. |
| Keep the old behaviour behind `--legacy` | Carries the destructive path forward indefinitely for no user benefit. There is nothing the old default does that `--commit` does not. |
| Remove `--dry-run` outright | Existing scripts fail on an unknown argument. See the deprecation path above. |

## Consequences

- **This is a breaking CLI change.** Anyone whose automation relies on a bare `mmm ~/Photos` to actually move files must add `--commit`. That is the point: the break is loud, immediate, and non-destructive, whereas the behaviour it replaces failed silently and destructively.
- Dedup is gated by `--commit` too — moving files into `duplicates/` is destructive in its own right, so a preview run reports the duplicate groups it found without creating a `duplicates/` directory in the user's library.
- The posture is covered by integration tests that drive the real binary via `assert_cmd`, not the library, because the `--commit` gate lives in `main`. A library-level test could call `execute_move` directly and prove nothing about whether `mmm ~/Photos` is safe. The preview tests deliberately pass no `-o`, exercising the most dangerous shape a real invocation takes, and assert the input tree is byte-identical afterwards by BLAKE3 hash rather than by path list.
- The safety net was verified by breaking the product: forcing `is_dry_run()` to `false` — restoring destructive-by-default — fails three integration tests, including the byte-identity assertion.
- `README.md`, `docs/USER-GUIDE.md` and `docs/TECHNICAL.md` were updated in the same change. A stale destructive example in the docs would defeat the flag change entirely, since the docs are where people copy commands from.

## Note on `adr-002`

An `adr-002-test-fixture-strategy` was provisionally planned to record a fallback fixture strategy — checked-in base JPEG bytes — in case the hand-built EXIF synthesiser could not be made to parse. It parsed on the first attempt across three independent parsers, so the fallback was never taken and **no ADR-002 exists**. The `related` front matter above points only at documents that are actually present.

The one finding from that work that *does* deserve a record is separate and still open: `nom-exif` resolves a naive EXIF timestamp against the machine's local timezone, which makes the organiser's output paths and filenames machine-dependent for any photo lacking `OffsetTimeOriginal`. The test fixtures pin this by emitting the tag; real user photos frequently will not. That is a product decision, not a test one, and is out of scope for this ADR.
