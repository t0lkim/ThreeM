---
type: analysis
title: Mutation Testing Report
created: 2026-08-09
tags:
  - testing
  - mutation
  - quality
related:
  - '[[coverage-report]]'
  - '[[hashing-baseline]]'
  - '[[journal-format]]'
---

# Mutation Testing Report

Measured with [`cargo-mutants`](https://github.com/sourcefrog/cargo-mutants) 27.1.0
over `code/`, on macOS 15 (aarch64), rustc 1.92.0, scoped to the four modules
that decide what moves and what is recorded: `organiser.rs`, `journal.rs`,
`hasher.rs`, `metadata.rs`.

Reproduce with:

```sh
cd code
cargo install cargo-mutants --locked
cargo mutants -j 4 \
  --file src/organiser.rs --file src/journal.rs \
  --file src/hasher.rs --file src/metadata.rs
```

Roughly an hour at `-j 4` on an 8-core M1 Pro: every mutant rebuilds the crate
and runs the whole suite.

## Why this exists, given the coverage report

[`coverage-report.md`](coverage-report.md) ends on the reason. `cargo llvm-cov`
reports no **branch** data for this crate, so a line counted as covered may have
taken only one of its two outcomes — and a line that ran is not a line whose
result anything looked at. Mutation testing asks the stronger question directly:
change the code so it is wrong, and see whether a test notices.

It found that 19 changes to these four modules could be made without any of 603
tests failing. Eleven of them were real gaps and are now closed; the other eight
are recorded below with the reason each is not a gap. Two of the eleven are
changes that would have moved somebody's photographs to the wrong place.

## Figures

| | Baseline | After |
|---|---|---|
| Mutants generated | 313 | 313 |
| Caught | 246 | **257** |
| **Missed** | **19** | **8** |
| Unviable (did not compile) | 47 | 47 |
| Timeout | 1 | 1 |
| Mutation score (caught ÷ viable) | 92.83% | **96.98%** |

Tests: 603 → 609. The whole suite still runs in about 28 seconds.

The "After" column is the baseline sweep with the eighteen re-examined mutants'
outcomes substituted in. Rather than pay another hour for an identical full
sweep, the verification run was scoped by regex to every function that had a
surviving mutant plus every function touched by the new tests — 53 mutants,
11 minutes — and the unaffected 260 were not re-run. The one surviving mutant
outside that regex (`STREAM_BUFFER_BYTES`, below) was re-examined on its own.

## What was actually wrong

Eleven surviving mutants, in the order they matter.

### A file lost between the two hashing reads was not counted

`skipped += full.skipped` in `find_duplicates` could be changed to `-=` and no
test failed. That line is how a file dropped by **phase 3** reaches the count the
operator is shown; phase 2's skips were tested, phase 3's had never happened in a
test at all. A file excluded from duplicate detection is excluded from the plan
too — it is not moved, not reported as moved, and if the count is wrong the
operator reads a clean summary over a library that was only partly processed.

Phase 3 skips a file that phase 2 could read and phase 3 could not: a file
deleted, truncated or locked *between the two reads*. That race cannot be
produced against a real filesystem, so the read is now injected —
`find_duplicates_with` takes the phase-3 hash function, and `find_duplicates`
passes `full_hash`. This follows the injected `copy` parameter on
`copy_verify_delete` rather than the thread-local used for the manifest: the
phase-3 read happens on the pool's worker threads, so a thread-local armed by the
test would never be seen by the code under test.

The new test makes both phases skip — phase 2 refuses two files whose length no
longer matches the scan, phase 3 is told one file has gone — and asserts the
total is their sum. A single skip in one phase would be counted the same by any
arithmetic at all.

### A camera that spells its datetime as text was silently undated

Both `entry_to_wall_clock` and `entry_to_reading` could have their
`EntryValue::Text` arm deleted with no test failing. `nom-exif` hands most files
over as an already-parsed `EntryValue::Time`, so the text arm fires only for the
cameras that write the tag their own way — which is exactly why nothing in the
fixture tree reached it, and exactly why losing it would be invisible until
somebody with one of those cameras found their photographs filed under the date
they copied them off the card. **This is a mutant that moves files to the wrong
place.** Both arms are now pinned by unit tests, including the video side's
requirement that an offset in the string comes back as `Reading::Zoned` and not
as an instant.

### The year check could be answered by a constant

`reading_year` could return `1` or `0` for every reading. The function exists to
catch the negative year `chrono` will parse out of a corrupt EXIF string before
it reaches `naming` — a constant would let every one of them through, and the
first sign would be a directory called `-44-03-15` at the top of somebody's photo
library whose name every command-line tool reads as a flag. **The second mutant
that moves files to the wrong place.** Now pinned across all three `Reading`
variants.

### "Your file has no date" and "we could not read your file's date" were the same test

`date_entry_is_unreadable` could return `true` unconditionally, and its `&&`
could become `||`, without failing anything. The distinction it draws is the
subject of a long doc comment and of `DateSource`'s five variants: a photograph
that records no date is a fact about the photograph, and a photograph whose
recorded date we could not read is a limitation of this tool.

The gap was a missing fixture, not a missing assertion. `jpeg_without_exif` never
reaches the question — with no `APP1` segment the parser returns first — and
`jpeg_with_unreadable_date` is already on the `true` side of it. Nothing in the
tree was a JPEG whose EXIF *parses* and contains no datetime entry, which is the
only file that can prove the question is being asked correctly. `MediaTree::
jpeg_with_exif_but_no_date` builds one, with a control test that puts the fixture
to `nom-exif` directly — the block yields entries, and none of them is a datetime
— because a fixture whose EXIF silently failed to parse would classify as
`Filesystem` too, by returning before the question is ever asked, and would have
made the assertion vacuous.

### The default thread count could be 1

`default_hash_threads` could return `1` unconditionally. The existing test
asserted only `<= DEFAULT_HASH_THREAD_CEILING`, which a constant of one
satisfies — a test named after the ceiling that passes while every machine
hashes single-threaded. It now asserts the rule the doc comment states,
`min(cores, ceiling)`.

### `is_filesystem` had no caller at all

Both mutants of `DateSource::is_filesystem` survived because nothing calls it:
`reporter.rs` tallies the three filesystem variants by matching them directly. It
is public API on a public enum, documented as the counterpart to `is_recorded`
(which *is* used), and its contract includes a genuine trap — `DateSource::None`
is not a filesystem fallback, and counting it as one would inflate the figure the
run reports. It is now pinned by a test over all five variants rather than
deleted; that it has no in-crate caller is recorded here as the finding it is.

### An error message could quote nothing at all

`elide` could stop keeping any prefix of a corrupt journal line and the existing
test still passed: it asserted the `… (500 bytes)` suffix and that the whole line
was not quoted, both of which a completely empty elision satisfies. An operator
cannot find the offending line in a journal from a byte count. The test now
asserts the first 160 bytes survive.

## Surviving mutants, and why each is accepted

Eight, none of which can change where a file goes or what is recorded about it.

| Site | Mutation | Why it is accepted |
|---|---|---|
| `hasher.rs` `STREAM_BUFFER_BYTES` | `128 * 1024` → `128 + 1024` | A read-buffer length. Every digest and every copy is byte-identical at any buffer size; only throughput moves, and no test asserts throughput. |
| `hasher.rs` `candidate_groups += 1` | `+=` → `*=` | The counter is only ever read by a `debug!` field. Cosmetic by the task's own definition. |
| `hasher.rs` duplicate-group tie-break | delete `(Some(x), Some(y))` arm | Unreachable by construction: the groups were the keys of a `HashMap`, so no two share a digest and `then_with` never runs. Kept because it states the total order the sort would need if that stopped being true; commented at the site. |
| `hasher.rs` `if skipped > 0` | `>` → `<` | Suppresses a `warn!` line. The count itself is reported by `reporter` and is asserted — see the phase-3 test above. |
| `hasher.rs` `hash_reader` | `== 0` → `!= 0` | **Timeout, not a survivor.** The loop never terminates, so no assertion can run. The suite detects it by hanging, which is detection by the least useful signal available; there is no assertion that would improve on it. |
| `metadata.rs` `"CreateDate" \| "DateTimeOriginal"` | delete arm | Dead against `nom-exif` 1.x, measured rather than assumed: a probe over every video fixture the harness can build (`mp4`, `mov`, Apple `creationdate`) shows the parser normalises every date onto `com.apple.quicktime.creationdate`, and it refuses outright any file that is not a container it supports — so a mislabelled JPEG or HEIC cannot reach the arm either. Kept and commented: the key names are the parser's, and a version that started reporting `CreateDate` verbatim would otherwise send every affected video to its filesystem timestamp in silence. |
| `organiser.rs` `has_location` | `&&` → `\|\|` | Unreachable: the EXIF and ISO 6709 extractors both set the two coordinates together or set neither, so no metadata this can be handed has one without the other. Commented at the site. |
| `organiser.rs` `ChunkController::chunk_started` and `should_continue` | replace default body | Equivalent mutants. The default bodies are `let _ = (args);` and `let _ = (args); true` — discarding the arguments has no effect, so the mutations are the same programs. |

## What this does not cover

- **Only four modules.** `naming.rs`, `config.rs`, `settings.rs`, `undo.rs`,
  `scanner.rs`, `sidecar.rs`, `xmp.rs`, `reporter.rs` and `timezone.rs` have not
  been mutated. `undo.rs` and `sidecar.rs` are the notable absences: `undo` puts
  files back and `sidecar` moves them, so both are destructive surfaces by the
  standard this project uses. They were out of the task's stated scope and are a
  real gap, not a judgement that they are safe.
- **No CI gate.** Unlike coverage, mutation testing is not wired into
  `.github/workflows/ci.yml`. An hour per run at `-j 4` is too slow for a pull
  request, and `--in-diff` mode (mutating only changed lines) has not been
  evaluated. Nothing currently stops a new surviving mutant from appearing.
- **`--baseline` is the same suite.** A mutant is "caught" if the suite fails,
  which includes failing for the wrong reason. Every kill above was confirmed by
  reading the test that does the killing, but that is a manual check.
- **The figures are macOS.** `#[cfg(unix)]` tests run; nothing Windows-specific
  was mutated or exercised.
