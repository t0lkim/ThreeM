---
type: reference
title: Journal Format
created: 2026-08-08
tags:
  - journal
  - undo
  - safety
  - format
related:
  - '[[adr-003-atomic-move-semantics]]'
  - '[[adr-004-journal-design]]'
---

# Journal format

The run journal is the record that makes `mmm undo` possible. Every committing run writes one, and it is written *ahead of* the filesystem: the line describing a move is on the disk before the move is attempted. A run that dies halfway — power loss, `Ctrl-C`, a full disk — leaves a journal that still says which files moved where, and which single file it was in the middle of.

This document specifies the on-disk format. The reasoning behind the choice of format is in [`adr-004-journal-design`](../decisions/adr-004-journal-design.md); the move primitives the journal records are in [`adr-003-atomic-move-semantics`](../decisions/adr-003-atomic-move-semantics.md).

## Location and naming

```
<output_dir>/.mmm/journal/<run_id>.jsonl
```

`--journal-dir <PATH>` overrides the directory; `--no-journal` disables journalling entirely, and is refused together with `--commit` unless `--i-know-what-im-doing` is also passed. The default sits *inside the output tree* so that a library copied to another disk arrives with the record of how it was built still attached.

`.mmm/` is excluded from scanning at any depth, so `mmm` never tries to organise its own metadata. A `.mmm` named explicitly on the command line is still scanned — the exclusion stops the walk wandering into metadata, it does not overrule an operator who pointed at it deliberately.

### Run ids

```
YYYYMMDD-HHMMSS-<six base36 characters>
```

for example `20260808-005652-z2a3m1`.

Lexical order over run ids is chronological order, which is why `mmm journal list` sorts by file name rather than by mtime — mtimes become the copy's the moment a library is copied to another disk. The random suffix exists because one second is not fine enough to separate two runs started together, and a collision would mean either a refused journal or, worse, two runs sharing one. It is six base36 characters drawn from the standard library's OS-seeded `RandomState` plus a process-local counter, so it costs no dependency.

## File shape

JSONL: one JSON object per line, `\n`-terminated, UTF-8.

- **Line 1** is the [run header](#run-header). A journal file that exists at all has one, because it is written before the handle is returned to the caller.
- **Lines 2..n** are [entries](#entries), one per line, in the order they happened.

Line-oriented because appending a line is the only write a crash can leave half-finished in a recoverable way — and because a journal has to stay readable by a person and by `jq` at three in the morning, when the tool itself is what is suspect.

### Schema version

`SCHEMA_VERSION` is **1**.

It is bumped whenever an existing field changes meaning or disappears. Adding a new *optional* field does not need a bump: the reader ignores unknown fields, so an older reader survives a newer writer's additions. A journal whose `schema_version` exceeds the reading build's is refused outright, naming the `mmm` version that wrote it — undoing a run by guessing at fields this build does not know is exactly the class of guess the journal exists to eliminate.

## Run header

Written as the first line of every journal.

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | `u32` | Format version of the lines that follow. See above. |
| `run_id` | `string` | Sortable run identifier, and the journal's own file stem. |
| `started_at` | RFC 3339 UTC | When the run began. |
| `mmm_version` | `string` | The build that wrote this journal, so a later undo can say which version's behaviour it is reversing. |
| `output_dir` | path | The output tree this run organised into. Undo's directory-pruning walk stops here. |
| `argv` | `[string]` | The command line, verbatim (lossy for non-UTF-8 arguments). |

`argv` and `output_dir` are in the header because "undo this run" is answerable only if the journal says which run it was. A journal found on a disk months later has to explain itself without the shell history that produced it.

```json
{"schema_version":1,"run_id":"20260808-005652-z2a3m1","started_at":"2026-08-08T00:56:52.294581Z","mmm_version":"0.1.0","output_dir":"…/out","argv":["mmm","…/in","-o","…/out","--commit","--no-prompt"]}
```

## Entries

Every entry is a flat JSON object with a `type` discriminator, so a line is readable without knowing the Rust type that produced it. Entries concerning a single file carry the `seq` of the intent they belong to; `seq` is allocated by the journal itself, so the organise pass and the duplicate pass draw from one counter and cannot collide.

### `move_intent`

Written and **synced before the move is attempted**.

| Field | Type | Meaning |
|---|---|---|
| `seq` | `u64` | Sequence number this move is known by. |
| `source` | path | Where the file is now. This is the path undo restores to. |
| `destination` | path | Where the run intends to put it. Not necessarily where it lands — see `move_committed`. |
| `source_size` | `u64` | Size stat-ed immediately before the move, not carried from the scan. Undo compares it against the file it finds. A failed stat records `0` and lets the move report the real cause. |
| `source_hash` | `string \| null` | BLAKE3 digest, when the run already had one. `null` for ordinary organise moves — the dedup cascade never fully hashes a unique file, and paying for a hash just to record it would be a cost on every file for a check size already mostly answers. |
| `kind` | `organise \| duplicate \| restore` | *Why* the file is moving. On the disk rather than inferred from the destination's shape, because undo treats the three differently and a duplicate goes back to a path the organiser never planned. |

### `move_committed`

Written immediately after a successful move.

| Field | Type | Meaning |
|---|---|---|
| `seq` | `u64` | The intent this settles. |
| `final_destination` | path | Where the file **actually** is. Collision resolution can land a planned `photo.jpg` at `photo-1.jpg`, and the name the file actually has is the only one undo can find it by. |
| `move_kind` | `renamed \| cross_volume` | `renamed` = same volume, link-and-unlink, no data copied. `cross_volume` = copied, digest-verified, promoted, then the source removed. See [`adr-003`](../decisions/adr-003-atomic-move-semantics.md). |

### `move_failed`

| Field | Type | Meaning |
|---|---|---|
| `seq` | `u64` | The intent this settles. |
| `reason` | `string` | The full error chain. |

A recorded failure is an **answer**: the source is still where it was, and undo must not list the file as possibly moved.

### `duplicate_moved`

The commit record for a duplicate relocation. One commit line per move, of the type that fits — writing a `move_committed` as well would be two lines claiming the same move, and a drift risk the moment they disagree.

| Field | Type | Meaning |
|---|---|---|
| `seq` | `u64` | The intent this settles. |
| `group` | `usize` | The `duplicates/NNN/` group the file went into. |
| `source` | path | Where the file came from. Carried again here, so a duplicate stays restorable even when its intent line was the one lost to truncation. |
| `destination` | path | Where it landed. |

**Undo treats `duplicate_moved` as a commit record alongside `move_committed`.**

### `run_completed`

The last line of a journal whose run reached an exit path — including the user-declined-chunk early stop. **Its absence means the run was interrupted**, and `mmm undo` says so before printing anything else.

| Field | Type | Meaning |
|---|---|---|
| `moved` | `usize` | Organise moves plus duplicate moves. |
| `failed` | `usize` | Attempted moves that did not happen. |
| `skipped` | `usize` | Files never attempted — stopped-before, plus unplannable. |
| `ended_at` | RFC 3339 UTC | When the run finished. |

## A complete journal

A three-file run: one duplicate relocated, two files organised, the second of which hit a name collision and landed with a `-1` suffix. Paths abbreviated.

```json
{"schema_version":1,"run_id":"20260808-005652-z2a3m1","started_at":"2026-08-08T00:56:52.294581Z","mmm_version":"0.1.0","output_dir":"…/out","argv":["mmm","…/in","-o","…/out","--commit","--no-prompt"]}
{"type":"move_intent","seq":0,"source":"…/in/a.png","destination":"…/out/duplicates/000/a.png","source_size":22,"source_hash":"3388ab0f3875332f05d292112d28c3864632a11eedcb35588464d311f0473d17","kind":"duplicate"}
{"type":"duplicate_moved","seq":0,"group":0,"source":"…/in/a.png","destination":"…/out/duplicates/000/a.png"}
{"type":"move_intent","seq":1,"source":"…/in/holiday/b.png","destination":"…/out/2026-08-08/2026-08-08-005651.png","source_size":21,"source_hash":null,"kind":"organise"}
{"type":"move_committed","seq":1,"final_destination":"…/out/2026-08-08/2026-08-08-005651.png","move_kind":"renamed"}
{"type":"move_intent","seq":2,"source":"…/in/holiday/a-copy.png","destination":"…/out/2026-08-08/2026-08-08-005651.png","source_size":22,"source_hash":null,"kind":"organise"}
{"type":"move_committed","seq":2,"final_destination":"…/out/2026-08-08/2026-08-08-005651-1.png","move_kind":"renamed"}
{"type":"run_completed","moved":3,"failed":0,"skipped":0,"ended_at":"2026-08-08T00:56:52.408554Z"}
```

Note `seq 2`: the intent names `…-005651.png` and the commit names `…-005651-1.png`. That divergence is the whole reason `final_destination` exists.

## Durability guarantee

**Every entry is on the disk before the operation it describes is attempted, and before `append` returns.**

- `Journal::append` writes the line and calls `File::sync_data()` before returning. Durability is per entry, not per run: buffering would make the journal exactly as lossy as the crash it exists to survive.
- The line and its `\n` go out in **one** `write_all`. Two calls could be interrupted between them, leaving a complete-looking entry with no terminator that the next append would then run into.
- The header is written and synced inside `Journal::create`, so a journal file that exists at all is a journal that can be read.
- The journal is created with `create_new`. A run-id collision is a refusal, never an overwrite — appending one run's entries to another run's journal would make both unusable.
- **A journal write failure stops the run; a move failure does not.** A run that cannot record what it is about to do must not do it. An outcome that cannot be recorded does not un-move the file, so the caller distinguishes the two cases and the run exits non-zero either way.

The cost is one `fsync` per move. It is accepted deliberately — see [`adr-004`](../decisions/adr-004-journal-design.md).

## Truncation recovery

A run killed mid-append leaves a partial final line. That is **expected, not corrupt**.

- A parse failure on the **final** line is discarded with a warning (`discarding a truncated final journal line`), and every complete entry before it is returned.
- A parse failure on **any other** line is a hard error naming the line number. Nothing truncates the middle of a file, so a bad line there is real corruption and is reported as such rather than silently skipped.
- The file is read as **bytes**, not as a string. A cut in the middle of a multi-byte character makes that one line unparseable; it must not make the whole journal unreadable.
- A truncated **header** is an error. There is no run to attribute the entries to.
- A journal with no lines at all is an error for the same reason.

### The interrupted-mid-rename case

A `move_intent` with no `move_committed`, `duplicate_moved` or `move_failed` for its `seq` is a move whose outcome nobody knows. `mmm undo` **does not guess**: it produces no restore step for it, reports it under `Possibly moved — verify manually`, and exits non-zero.

The reported destination is the one the run *planned*, and the report says so — the line that would have named where the file actually landed is precisely the line the interruption cost. Under collision resolution the file may sit at `…-3.jpg` while the report can only name `….jpg`, so the notice tells the operator to expect a numbered suffix rather than to conclude from an empty path that the move never happened.

**Any field added to this report must be one the intent line alone can supply.**

## Undo contract

`mmm undo` reads a journal and replays it backwards. What it guarantees:

1. **It plans from commit records, never from intents.** `final_destination` is where the file actually is; the intent's planned destination would move the wrong file or none.
2. **Reverse order is load-bearing.** A run that moved `a`→`x` then `b`→`a` must be undone from the far end, or the first file lands on top of the second.
3. **It is a preview by default**, like everything else. `--commit` is required to move anything, and a preview writes no journal at all.
4. **Each file is verified immediately before it is moved**, not once up front — the restores change what later steps find. Existence, then is-a-file, then size, then digest (only when one was recorded), stopping at the first answer. `symlink_metadata`, so a symlink left where the file was reads as a replacement rather than being followed.
5. **A modified, missing or unverifiable file is reported and skipped, never moved.** A skipped file writes no journal line at all, so the undo's own journal has nothing to reverse for it.
6. **A restore never overwrites.** It goes through the same no-clobber move primitive as everything else, so an occupied original path yields `first-1.jpg` beside its occupant. That is reported as a conflict.
7. **An undo is itself journalled**, as a new run with `kind: restore` intents committing as `move_committed` — so an undo is undoable with no third record type.
8. **Empty directories left behind are removed**, walking up from each vacated parent, stopping at the run's `output_dir` and never entering `.mmm/`. Only genuinely empty directories: "never remove a directory containing files `mmm` did not create" is structural, not a heuristic. A consequence, deliberate: `duplicates/NNN/manifest.txt` survives after its files go home — the manifest records a run that really happened, and deleting records is not undo's job.
9. **It exits non-zero if the library is not as it was** — any file not restored, any conflict, or any unresolved intent. A conflicted file counts: it was restored, but not to the path it came from, and a script that reads exit 0 as "safe to delete the library" must not be told that it is.

The outcome table names each case: `Restored`, `Conflicted`, `Skipped (missing)`, `Skipped (modified)`, `Could not restore`, `Not attempted`, `Possibly moved`.

## Reading a journal by hand

The format is deliberately `jq`-shaped.

```bash
# Every move a run committed, as source → final destination
jq -r 'select(.type=="move_committed") | "\(.seq) → \(.final_destination)"' \
  ~/Photos/.mmm/journal/20260808-005652-z2a3m1.jsonl

# Intents with no recorded outcome (the interrupted-mid-rename case)
jq -s '[.[] | select(.type=="move_intent") | .seq] -
       [.[] | select(.seq != null and .type != "move_intent") | .seq]' \
  ~/Photos/.mmm/journal/20260808-005652-z2a3m1.jsonl
```

`mmm journal list` and `mmm journal show <run_id>` render the same information without the JSON.
