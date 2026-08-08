# mmm User Guide

## Overview

`mmm` scans one or more directories for images and videos, detects duplicates, renames files by date and location, and sorts them into a `Year/Month/Day` directory hierarchy. A companion tool, `mmm-dedup-verifier`, independently verifies that flagged duplicates are genuine before you delete them.

Both binaries are installed at `~/bin/`.

> **`mmm` is safe by default.** Every run is a preview until you pass `--commit`. Without it, `mmm` scans, plans, prints and exits without touching a single file. Note that when no `--output` is given, output defaults to the *first input directory* — so `mmm ~/Photos --commit` reorganises `~/Photos` in place.

---

## Quick Start

Preview what would happen — this is the default, no files are modified:

```bash
mmm ~/Photos
```

Read the plan. If it looks right, re-run the same command with `--commit` to apply it:

```bash
mmm ~/Photos --commit
```

Organise files from multiple sources into a single output directory:

```bash
mmm ~/Photos ~/Camera/DCIM -o ~/Organised --commit
```

After organising, verify the duplicates directory:

```bash
mmm-dedup-verifier ~/Organised/duplicates/
```

---

## mmm

### Usage

```
mmm [OPTIONS] <DIRECTORIES>...      # organise (the default)
mmm undo [LIBRARY] [OPTIONS]        # put a recorded run back
mmm journal list [LIBRARY]          # what has been run against this library
mmm journal show <RUN_ID> [LIBRARY] # one run in full
```

`mmm ~/Photos` still means "organise `~/Photos`" — the subcommands are additions, not a change. Writing `mmm organise ~/Photos` says the same thing explicitly, which is how you organise a directory that happens to be named `undo` or `journal`.

### Arguments

| Argument | Description |
|---|---|
| `<DIRECTORIES>...` | One or more directories to scan (recursive) |

### Options

| Flag | Short | Default | Description |
|---|---|---|---|
| `--output <DIR>` | `-o` | First input directory | Where organised files and the `duplicates/` directory are written |
| `--commit` | | off | **Actually move files.** Without this, `mmm` only prints the plan and exits |
| `--chunk-size <N>` | `-c` | 100 | Number of files to process before pausing for confirmation |
| `--no-prompt` | | off | Skip confirmation prompts between chunks |
| `--journal-dir <PATH>` | | `<output>/.mmm/journal` | Write the run journal somewhere else — useful when the output tree is read-only, or when journals for several libraries are collected in one place |
| `--no-journal` | | off | **Unsafe.** Do not record this run, so it cannot be undone. Refused together with `--commit` unless `--i-know-what-im-doing` is also passed |
| `--i-know-what-im-doing` | | off | Acknowledge an unsafe flag combination (currently only `--no-journal --commit`) |
| `--verbose` | `-v` | warn | Increase log verbosity (`-v` info, `-vv` debug, `-vvv` trace) |
| `--help` | `-h` | | Print help |
| `--version` | `-V` | | Print version |

#### Deprecated

| Flag | Short | Description |
|---|---|---|
| `--dry-run` | `-d` | **No-op.** Previewing is now the default, so this flag does nothing. It stays accepted, hidden from `--help`, so scripts written against the old CLI keep running; passing it prints a deprecation notice to stderr. If combined with `--commit`, the explicit `--commit` wins and files are moved. |

### What It Does

1. **Announces the mode** — `DRY RUN — no files will be modified. Re-run with --commit to apply.` or `COMMIT MODE — files will be moved.`, printed before anything is scanned.
2. **Scans** all input directories recursively for media files.
3. **Deduplicates** using a three-phase hash cascade (see Technical Documentation).
4. **Extracts metadata** — creation date and GPS coordinates from EXIF (images) or container atoms (video). Falls back to filesystem creation date when metadata is absent.
5. **Plans renames** — each unique file is assigned a target path: `<output>/YYYY-MM-DD/YYYY-MM-DD-HHMMSS[-location].ext`.
6. **Reports** — without `--commit`, prints the full plan and duplicate list, then exits. Nothing is created, moved or deleted.
7. **Opens a journal** — with `--commit`, a record of the run is created at `<output>/.mmm/journal/<run_id>.jsonl` before anything moves. Its path is printed immediately and again in the closing summary. A preview writes no journal, because it moves nothing.
8. **Moves duplicates** — with `--commit`, duplicate files are moved to `<output>/duplicates/000/`, `001/`, etc. Each group directory includes a `manifest.txt` recording the BLAKE3 hash and original file path.
9. **Organises** — with `--commit`, unique files are renamed and moved into the date-based hierarchy, pausing every `--chunk-size` files for confirmation.

### Preview Output

A preview run (no `--commit`) produces two reports:

**Duplicate Groups** — lists every group of identical files with their BLAKE3 hash:

```
═══ Duplicate Groups ═══

Group 1 (3 files, 4521984 bytes each, hash: 7a3b1c4d5e6f7890…):
  → ~/Photos/IMG_0042.jpg
  → ~/Camera/DCIM/IMG_0042.jpg
  → ~/Photos/Copy of IMG_0042.jpg
```

**Planned Operations** — shows the source and destination for every unique file:

```
═══ Dry Run — Planned Operations ═══

  [EXIF] ~/Photos/IMG_0001.jpg → ~/Organised/2024/03/15/2024-03-15-143022-London-GB.jpg
  [FS]   ~/Photos/screenshot.png → ~/Organised/2026/01/02/2026-01-02-091500.png
  [NO DATE] ~/Photos/unknown.bmp → ~/Organised/unsorted/unknown.bmp
```

The `[EXIF]`, `[FS]`, and `[NO DATE]` tags tell you where the date came from.

### Supported Formats

**Images:** JPEG, PNG, HEIC/HEIF, TIFF, RAW (CR2, CR3, NEF, ARW, DNG, ORF, RW2, RAF, SRW, PEF), WebP, AVIF, BMP

**Video:** MOV, MP4, M4V, AVI, MKV, WMV, FLV, WebM, 3GP, MTS, M2TS

### Output Structure

```
~/Organised/
├── .mmm/
│   └── journal/
│       └── 20260808-005652-z2a3m1.jsonl
├── 2024/
│   ├── 01/
│   │   ├── 15/
│   │   │   ├── 2024-01-15-143022-London-GB.jpg
│   │   │   └── 2024-01-15-143025-London-GB.jpg
│   │   └── 20/
│   │       └── 2024-01-20-091500.mp4
│   └── 03/
│       └── ...
├── unsorted/
│   └── unknown.bmp
└── duplicates/
    ├── 000/
    │   ├── manifest.txt
    │   └── IMG_0042.jpg
    └── 001/
        ├── manifest.txt
        └── clip.mov
```

### Safety Guarantees

- **Safe by default.** Without `--commit`, not a single file is created, moved, or deleted — including the `duplicates/` directory, which is not created at all during a preview.
- **Originals are never deleted during dedup.** The first file in each group is kept; only copies are moved to `duplicates/`.
- **Same-volume moves never overwrite.** The move is a `link()` followed by an `unlink()`, and `link()` refuses any destination that is already occupied — including a dangling symlink, which an `exists()` check would report as free. A file already at the target name is never replaced.
- **Cross-volume moves use copy-verify-delete.** The file is streamed to a temp file on the target volume and hashed on the way through; the file that landed is then hashed and the two BLAKE3 digests compared. Only once they match is the source deleted. A copy that is the right length and the wrong bytes is caught, and the original is kept.
- **Filename collisions are resolved.** If the target filename already exists, a numeric suffix (`-1`, `-2`, etc.) is appended.
- **One unreadable file costs one file.** A directory that cannot be read, or a photo that cannot be opened, is skipped with a warning — the rest of the library is still organised. Nothing is skipped silently: the closing summary reports `Unreadable (scan):` and `Unhashable (dedup):` counts, and each skipped path is named in a warning.
- **A file that cannot be read is never moved.** If its contents could not be established, it stays exactly where you put it.
- **You can stop at any chunk.** Between chunks, the tool asks whether to continue. Answering `n` stops before the next chunk; files already moved stay moved, nothing else is touched. The run then finishes properly — it prints the same closing summary a completed run does, with a `Not processed:` line counting the files it never got to, so you always know what a stopped run managed.
- **Every committing run is recorded before it acts.** Each move is written to the journal and flushed to disk *before* the file is touched, so a run killed by `Ctrl-C`, a power cut or a full disk still leaves a record of exactly what it moved and where — and one entry naming the single file it was part-way through. See [Undoing a run](#undoing-a-run).

---

## Undoing a run

Every run that moves files writes a journal first. `mmm undo` reads it back and puts the files where they were.

```bash
# What has been run against this library?
mmm journal list ~/Photos

# Preview putting the most recent run back — nothing is modified
mmm undo ~/Photos

# Do it
mmm undo ~/Photos --commit
```

`undo` takes the library as a positional argument defaulting to the current directory, so `cd ~/Photos && mmm undo` works.

### Undo options

| Flag | Default | Description |
|---|---|---|
| `[LIBRARY]` | `.` | The organised library whose journals to read |
| `--run <RUN_ID>` | most recent | Undo a specific run, as named by `mmm journal list` |
| `--last` | implied | Undo the most recent recorded run. Inert on its own — it is what happens anyway — but lets a script say which run it means rather than rely on a default. Conflicts with `--run` |
| `--commit` | off | **Actually move the files back.** Without this, `undo` only prints the restore plan |
| `--journal-dir <PATH>` | `<LIBRARY>/.mmm/journal` | Read journals from here instead. The counterpart of `organise --journal-dir` |

### Listing and inspecting runs

```
$ mmm journal list ~/Photos

═══ Recorded Runs ═══

  20260808-005652-z2a3m1  2026-08-08 00:56:52.294581 UTC
    moved 3, failed 0, skipped 0
    3 files could be put back by `mmm undo --run 20260808-005652-z2a3m1`

Total: 1 run
```

Runs are listed newest first. `mmm journal show <RUN_ID> ~/Photos` renders one run in full: the command line that produced it, the `mmm` version, and every move it intended and committed.

### What undo does

```
$ mmm undo ~/Photos --commit

COMMIT MODE — files will be moved.

═══ Undo — Run 20260808-005652-z2a3m1 ═══
  Started:            2026-08-08 00:56:52.294581 UTC
  Library:            ~/Photos/out
  Journal:            ~/Photos/out/.mmm/journal/20260808-005652-z2a3m1.jsonl
  Files to restore:   3

  ~/Photos/out/2026-08-08/2026-08-08-005651-1.png → ~/Photos/in/holiday/a-copy.png
  ~/Photos/out/2026-08-08/2026-08-08-005651.png → ~/Photos/in/holiday/b.png
  ~/Photos/out/duplicates/000/a.png → ~/Photos/in/a.png

═══ Undo — Results ═══

  restored   ~/Photos/in/holiday/a-copy.png
  restored   ~/Photos/in/holiday/b.png
  restored   ~/Photos/in/a.png

═══ Undo Complete ═══
  Restored:           3
  Empty dirs removed: 1
═════════════════════
```

Moves are replayed in reverse. That ordering is not cosmetic: a run that moved `a` → `x` and then `b` → `a` has to be undone from the far end, or the first file lands on top of the second.

Duplicates relocated into `duplicates/NNN/` are restored alongside ordinary moves. The group's `manifest.txt` is deliberately left behind — it records a run that really happened, and deleting records is not undo's job.

### Undo refuses to guess

Before each file is moved back, `mmm` checks that the file at the recorded destination is still the file it put there — it exists, it is a regular file (a symlink left in its place counts as a replacement), its size matches, and its BLAKE3 hash matches when one was recorded. Anything else is reported and **skipped**, never moved.

The closing table names every outcome:

| Outcome | Meaning |
|---|---|
| `Restored` | Put back at its original path. |
| `Conflicted` | Something else now occupies the original path, so the file was restored *beside* it with a numeric suffix rather than overwriting. |
| `Skipped (missing)` | The file is no longer at the destination the run recorded. |
| `Skipped (modified)` | The file at that destination is not the one the run moved there. |
| `Could not restore` | The move itself failed, or the file could not be checked at all. |
| `Not attempted` | The run stopped before reaching it. |
| `Possibly moved` | See below. |

**`undo` exits non-zero if the library is not exactly as it was** — including when a file was restored but only under a conflict suffix. A script can therefore treat exit 0, and only exit 0, as "the undo was clean".

### Interrupted runs

If a run was killed mid-move, its journal ends with a move it recorded as *about to happen* and never recorded the outcome of. `mmm undo` will not guess whether that file moved:

```
─── Possibly moved — verify manually ───

The run recorded that it was about to move each of these and never recorded what happened
next, so each one is either still at its original path or already in the library. …

  [    3] ~/Photos/in/IMG_0042.jpg  →  ~/Photos/out/2024-03-15/2024-03-15-143022.jpg (planned)
```

Check both paths by hand. The destination shown is the one the run *planned* — the line that would have said where the file actually landed is exactly the line the interruption cost — so if a name collision occurred the file may be sitting beside it under a numbered suffix.

Everything the run *did* record is restored normally. A journal cut off mid-line loses only that line.

### Undo is itself undoable

A committing undo writes its own journal, so it shows up in `mmm journal list` like any other run and can be reversed the same way.

### Turning the journal off

`--no-journal` disables journalling. Combined with `--commit` it is refused unless `--i-know-what-im-doing` is also passed, because it means "move this library and keep no record of where anything came from" — reasonable on a scratch tree, catastrophic by accident. A run that used it says so in its summary rather than printing nothing.

The format is documented at [`docs/architecture/journal-format.md`](architecture/journal-format.md), and the reasoning behind it at [`docs/decisions/adr-004-journal-design.md`](decisions/adr-004-journal-design.md).

---

## mmm-dedup-verifier

### Usage

```
mmm-dedup-verifier [OPTIONS] <DUPLICATES_DIR>
```

### Arguments

| Argument | Description |
|---|---|
| `<DUPLICATES_DIR>` | Path to the `duplicates/` directory created by `mmm` |

### Options

| Flag | Description |
|---|---|
| `--check-originals` | Also verify that the original files still exist at their recorded paths |
| `-v, --verbose` | Increase verbosity |

### What It Does

1. Reads each numbered group directory (`000/`, `001/`, ...).
2. Parses the `manifest.txt` to find the recorded original file path.
3. Hashes the original file using BLAKE3 **keyed mode** (a deliberately different algorithm from the main binary — see Technical Documentation).
4. Hashes every duplicate file in the group directory using the same keyed mode.
5. Compares hashes. If all duplicates match the original, the group is `[OK]`. If any differ, it is `[MISMATCH]`. If the original file no longer exists, it is `[MISSING]`.
6. Prints a summary and exits with code 1 if any mismatches were found.

### Example Output

```
Verifying 3 duplicate groups using SHA-256...

═══ Verification Results (SHA-256) ═══

  [OK] Group 000: ~/Organised/2024/01/15/2024-01-15-143022.jpg (2 duplicates, hash: 7a3b1c4d5e6f7890...)
  [OK] Group 001: ~/Organised/2024/03/20/2024-03-20-101500.mp4 (1 duplicates, hash: abc123def456...)
  [MISMATCH] Group 002: ~/Organised/unsorted/unknown.bmp (1 duplicates, hash: 999888777666...)
    MISMATCH: ~/Organised/duplicates/002/unknown.bmp (hash: 111222333444...)

═══ Summary ═══
  Groups verified: 3
  Confirmed duplicates: 2
  Hash mismatches: 1
  Original missing: 0

WARNING: 1 groups have hash mismatches — review before deleting!
```

### Recommended Workflow

```bash
# 1. Preview to review the plan (default — nothing is modified)
mmm ~/Photos -o ~/Organised

# 2. Same command, plus --commit, to run it for real
mmm ~/Photos -o ~/Organised --commit

# 3. Verify duplicates independently
mmm-dedup-verifier ~/Organised/duplicates/

# 4. If all [OK], safe to delete duplicates
rm -rf ~/Organised/duplicates/

# 5. If any [MISMATCH], investigate before deleting

# 6. Changed your mind about the whole run? Preview putting it back, then do it
mmm undo ~/Organised
mmm undo ~/Organised --commit
```
