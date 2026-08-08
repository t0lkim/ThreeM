---
type: decision
title: Journal design
created: 2026-08-08
tags:
  - journal
  - undo
  - safety
  - durability
related:
  - '[[adr-001-dry-run-by-default]]'
  - '[[adr-003-atomic-move-semantics]]'
  - '[[journal-format]]'
  - '[[CHANGELOG]]'
---

# ADR-004: Append-only JSONL with per-entry fsync

**Status:** Accepted
**Date:** 2026-08-08

## Problem

[`adr-001`](adr-001-dry-run-by-default.md) made previewing the default, and [`adr-003`](adr-003-atomic-move-semantics.md) made each individual move refuse to destroy anything it did not create. Neither reverses a run. A user who read the plan, agreed with it, ran `--commit`, and *then* realised the output tree was wrong had no way back: thousands of files renamed and redistributed across a date hierarchy, undoable only by hand and from memory. `adr-003` closes with that gap named explicitly.

The gap has a second half that a "just be careful" answer cannot reach at all. A run that is **interrupted** — `Ctrl-C`, a laptop lid, a full disk at file 4 000 of 9 000 — leaves the library in a state nobody has a description of. Some files moved, some did not, and the boundary between them exists only in the terminal scrollback, if that. The tool has to be able to say afterwards which files it moved and where, having been given no opportunity to say it at the time.

So the requirement is not "log what happened". It is:

- The record of a move must exist on the disk **before** the move, or an interruption between the two loses a file with no trace.
- The record must survive the process being killed without warning — no `Drop`, no flush, no closing write.
- The record must name where the file **actually** landed, not where it was planned to go, because collision resolution can put a planned `photo.jpg` at `photo-1.jpg`.
- A record that is itself half-written must still yield everything written before it.

## Decision

**An append-only JSONL file, one entry per line, `fsync`ed per entry, written before the operation it describes.**

One header line naming the run, then one line per intent and one per outcome, at `<output_dir>/.mmm/journal/<run_id>.jsonl`. The full specification is [`journal-format`](../architecture/journal-format.md); this ADR covers why that shape and not another.

### Why append-only

The journal is only trustworthy if a crash cannot leave it *wrong*. Append-only is the one write pattern with that property: an interruption can leave a line incomplete, but it cannot alter a line that is already there. Every alternative that updates a record in place — marking an intent "done", maintaining a count, rewriting a status field — has a window in which the file on disk says something untrue, and that window is precisely where the crashes this exists to survive land.

The consequence is that the file has no summary. "Which moves are unaccounted for?" is answered by reading the whole journal and pairing intents to outcomes by `seq`. That is a linear scan of a file with two lines per moved file, which for a 50 000-photo library is a few megabytes read once, by a command a user runs after something has already gone wrong. It is not on any hot path.

### Why one line per entry, JSON

Line-oriented, because **appending a line is the only write a crash can leave half-finished in a recoverable way**. A partial line is unparseable and is at the end; everything before it is complete by construction. There is no framing to resynchronise, no length prefix that might itself be truncated, no offset table to be stale.

JSON per line, because the journal has to be readable by a person and by `jq` at three in the morning when the tool itself is what is suspect. A user who is not sure `mmm undo` will do the right thing must be able to look at exactly what it will do, with tools they already have. An enum is tagged externally with a `type` field for the same reason: every line is a flat object that explains itself without reference to the Rust type that wrote it.

### Why fsync per entry

`File::sync_data()` before `append` returns.

Without it, the "record, then move" ordering is a statement about a buffer in this process rather than about the disk — and a buffer does not survive a `SIGKILL`, a panic, or a power cut, which is the entire threat model. A journal flushed at the end of a run is a journal that is guaranteed to be complete exactly when it is not needed.

The line and its `\n` go out in one `write_all` for the adjacent reason: two calls could be interrupted between them, leaving a complete-looking entry with no terminator that the next append would then run into, producing one corrupt line in the *middle* of the file — the one failure mode the format does not tolerate.

### Why a truncated tail is recovery, not corruption

The reader discards an unparseable **final** line with a warning and returns everything before it; an unparseable line anywhere else is a hard error naming the line number.

That asymmetry is not leniency, it is a claim about what can physically happen. Nothing truncates the middle of a file. A bad line at the end is the expected shape of an interrupted run; a bad line in the middle is real damage, and treating it as skippable would let `undo` act on a partial picture while believing it had the whole one.

The file is read as bytes rather than as a string for the same reason: a cut in the middle of a multi-byte character must cost one line, not the whole journal.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **SQLite (or any embedded database)** | It solves durability properly and would give indexed queries for free — but it brings a C dependency and a schema-migration story to a project whose entire persistence need is an ordered list of six record types that is written once and read once. Worse, it inverts the recovery property that matters: a database file interrupted mid-transaction is opaque to the operator, and the answer to "is my library recoverable?" becomes "run this other tool and hope". A text file's damaged state can be read, understood and repaired in an editor. For a tool whose users are trusting it with irreplaceable photographs, inspectability by the person holding the disk beats query performance nobody needs. |
| **A batch-written manifest at the end of the run** | Fails the requirement outright. The record has to exist before the move, and a manifest written at the end exists only for runs that reached the end — which excludes exactly the runs that need it. This is precisely the defect [`adr-003`](adr-003-atomic-move-semantics.md) already fixed once in the duplicate manifest: it accumulated in a `String` and was written after the group's last move, so an interrupted dedup pass left files relocated with no record of where they came from. Repeating that mistake at run scope was not on the table. |
| **Buffered writes, fsync every N entries or every N seconds** | Turns a hard guarantee into a probability, and the exposure is the last N moves — the ones nearest the interruption, which is to say the ones an operator is least sure about. It also makes the safety property depend on a tuning constant, and any such constant is eventually raised for performance by someone who does not know what it is protecting. |
| **Write intent and outcome as one line after the move** | One `fsync` per move instead of two, and it loses the entire point. A file killed between the rename and the write is a file that moved with nothing recording it. The two-line shape is what makes "possibly moved — verify manually" expressible at all. |
| **`fdatasync` the containing directory as well as the file** | Not done, and the gap is acknowledged. The journal file's *creation* is not synced into its parent directory, so a crash immediately after `Journal::create` could in principle leave a library with a moved file and no directory entry for the journal naming it. The window is one `create_dir_all` plus one `open` wide, before any move is attempted; closing it means a platform-specific directory sync for a case in which no file has yet moved. Recorded here rather than silently omitted. |
| **A single global journal in `~/.local/share`** | A journal describes one particular library. Parking it in a home directory means a library copied to another disk, or handed to somebody else, arrives with no record of how it was built — and one machine reinstall away from an undo that cannot find its run. In the output tree, the record travels with the thing it describes. |
| **Binary format (bincode, CBOR, protobuf)** | Smaller and faster to parse, neither of which is a constraint here — the file is written at the speed of `fsync` and read once by a human-facing command. It would cost the property the format was chosen for: an operator being able to read the record without the tool. |
| **Log to `tracing` and parse the log back** | Log lines are for humans and change freely; a record undo depends on is a data format with a schema version. Coupling the two means a cosmetic change to a log message breaks recovery. |

## Consequences

- **Two `fsync`s per moved file**, one for the intent and one for the outcome. Measured on macOS/APFS with a debug build over 300 tiny files — deliberately the worst case, since the per-file journal cost is fixed while the real per-file work (EXIF parsing, BLAKE3 hashing, copying bytes) is near zero for a 36-byte file: **~14.8–15.3 s with the journal against ~12.1–12.3 s with `--no-journal`**, i.e. roughly 9–10 ms per file, or +22 % on a run doing almost no other work. On a real photo library, where each file carries megabytes to hash and metadata to parse, the same fixed cost is a much smaller share. It is accepted without a flag to tune it: the durability *is* the feature, and a fast journal that does not survive a crash is a slow way of writing nothing.
- **`--no-journal` exists and is refused with `--commit`** unless `--i-know-what-im-doing` is also passed. The combination is legitimate on a scratch tree and catastrophic by accident, and the only difference between the two is whether the operator meant it — so the refusal names the flag that says so. A run that did use it gets a warning in the summary rather than silence, because "no journal" means two opposite things: a preview recorded nothing because it moved nothing, and `--no-journal` moved files that can never be put back.
- **A journal write failure stops the run.** A caller that cannot record what it is about to do must not do it. A failure recording an *outcome* does not un-move the file, so the two are distinguished and counted honestly, and the run exits non-zero either way.
- **The ordering lives in one function, not at every call site.** Threading the journal into each loop would put "record, then move" at every call site, and the one that forgets it produces a move nothing can reverse. Both passes go through a single `recorded_move`.
- **`.mmm/` had to become invisible to the scanner**, at any depth, or `mmm` would organise its own journals on the next run. A `.mmm` named explicitly on the command line is still scanned — the exclusion stops the walk wandering into metadata, it does not overrule a deliberate operator.
- **The schema is versioned, and a newer journal is refused rather than partially understood.** Adding an optional field does not need a bump, since unknown fields are ignored; a field whose meaning changes does. Undoing a run by guessing at fields the build does not know is the class of guess this whole subsystem exists to eliminate.
- **An undo is itself a journalled run**, with `restore` intents committing as ordinary `move_committed` records, so an undo is undoable with no third record type and no special case in the reader.
- **This does not make every run reversible.** `--no-journal` runs are not, a journal deleted by hand cannot be replayed, and a file modified after the run is reported and skipped rather than restored over. What it guarantees is that a run which *was* journalled can be described exactly afterwards — including a run that was killed halfway.
