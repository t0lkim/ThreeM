# mmm User Guide

## Overview

`mmm` scans one or more directories for images and videos, detects duplicates, renames files by date and location, and sorts them into one directory per day (`YYYY-MM-DD/`). Both the directory layout and the filenames are configurable — see [Configuration](#configuration). A companion tool, `mmm-dedup-verifier`, independently verifies that flagged duplicates are genuine before you delete them.

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
mmm config show                     # the settings this run resolved to, and why
mmm config path                     # where config files were looked for
mmm config init [--user|--project]  # write a starter config
mmm config validate [PATH]          # parse a config, run nothing
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
| `--threads <N>` | | cores, capped at 8 | How many files the duplicate scan may hash at once. Higher helps on NVMe; lower is kinder to a spinning disk or a network share. The plan is the same either way — only the speed changes. See [How many threads](#how-many-threads) |
| `--no-prompt[=<BOOL>]` | | off | Skip confirmation prompts between chunks. `--no-prompt=false` keeps them, which is how to answer a `no_prompt = true` written in a config file |
| `--timezone <TZ>` | | machine's zone | Which wall clock to read a file that recorded no offset against. A fixed offset (`+08:00`, `-05:30`) or an IANA name (`Asia/Singapore`). See [Timezones](#timezones) |
| `--require-exif[=<BOOL>]` | | off | Refuse to file anything under a date the file did not record itself — those go to `unsorted/` instead, keeping their own names. See [Refusing dates you do not trust](#refusing-dates-you-do-not-trust) |
| `--no-sidecars[=<BOOL>]` | | off | Leave `.xmp`, `.aae` and `.thm` sidecars where they are instead of moving them with their photograph. See [Sidecars](#sidecars) |
| `--config <PATH>` | | discovery | Read this config file instead of searching for one. A path that does not exist is an error |
| `--no-config` | | off | Ignore every config file. `MMM_` environment variables still apply |
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
4. **Extracts metadata** — creation date and GPS coordinates from EXIF (images) or container atoms (video), then from an `.xmp` sidecar for a file that recorded nothing usable itself. Falls back to the filesystem timestamp when there is nothing else, and says which of the three reasons applied.
5. **Plans renames** — each unique file is assigned a target path: `<output>/YYYY-MM-DD/YYYY-MM-DD-HHMMSS[-location].ext`, derived from the file's **local wall clock** (see [Timezones](#timezones)).
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

  [EXIF][tz:exif] ~/Photos/IMG_0001.jpg → ~/Organised/2024-03-15/2024-03-15-143022-London-GB.jpg
  [SIDECAR][tz:config] ~/Photos/IMG_0002.cr2 → ~/Organised/2024-03-15/2024-03-15-143530.cr2
    [sidecar] ~/Photos/IMG_0002.cr2.xmp → ~/Organised/2024-03-15/2024-03-15-143530.cr2.xmp
  [FS][tz:system] ~/Photos/screenshot.png → ~/Organised/2026-01-02/2026-01-02-091500.png
  [FS: UNSUPPORTED][tz:system] ~/Photos/DSC_0009.nef → ~/Organised/2026-01-02/2026-01-02-091500.nef
  [NO DATE] ~/Photos/unknown.bmp → ~/Organised/unsorted/unknown.bmp
```

The first tag says where the **date** came from:

| Tag | Meaning |
|---|---|
| `[EXIF]` | The file's own embedded metadata. |
| `[SIDECAR]` | An `.xmp` beside the file — see [Sidecars](#sidecars). |
| `[FS]` | The filesystem timestamp: the file records no date. |
| `[FS: UNREADABLE]` | The filesystem timestamp, because the file's metadata is there and will not parse — a truncated write, a corrupted card. |
| `[FS: UNSUPPORTED]` | The filesystem timestamp, because the format is not one `mmm` can read a date out of. Which formats those are is in [`docs/reference/format-support.md`](reference/format-support.md). |
| `[NO DATE]` | No usable date at all; the file goes to `unsorted/`. |

The second tag says which **wall clock** the date was read against — `[tz:exif]`,
`[tz:sidecar]`, `[tz:config]`, `[tz:system]` or `[tz:utc]`. See
[Timezones](#timezones).

An indented `[sidecar]` line is a companion file following its parent. The
closing summary counts every one of these categories, on a committing run as well
as a preview.

### Supported Formats

Thirty-two extensions are **scanned** — 21 image, 11 video:

**Images:** JPEG, PNG, HEIC/HEIF, TIFF, RAW (CR2, CR3, NEF, ARW, DNG, ORF, RW2, RAF, SRW, PEF), WebP, AVIF, BMP

**Video:** MOV, MP4, M4V, AVI, MKV, WMV, FLV, WebM, 3GP, MTS, M2TS

A date can be **read out of** four container families: JPEG, the HEIF family
(HEIC/HEIF/AVIF), QuickTime and MP4. Everything else — every TIFF-based RAW
included — falls back to the filesystem timestamp unless there is an `.xmp`
beside it, and the run says so per file rather than passing it off as an ordinary
date. Which formats are verified, which are not, and what happens in each case is
the whole subject of
[`docs/reference/format-support.md`](reference/format-support.md).

### Output Structure

```
~/Organised/
├── .mmm/
│   └── journal/
│       └── 20260808-005652-z2a3m1.jsonl
├── 2024-01-15/
│   ├── 2024-01-15-143022-London-GB.jpg
│   └── 2024-01-15-143025-London-GB.jpg
├── 2024-01-20/
│   └── 2024-01-20-091500.mp4
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

Every name in that tree is a default, not a fixture: `date_directory_format`, `filename_format`, `duplicates_dir` and `unsorted_dir` change it, and `extensions` decides which files are scanned at all. See [Configuration](#configuration).

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

## How many threads

Finding duplicates means reading files, and `mmm` reads several at once. How
many is `--threads`, or `hash_threads` in a config file. **It changes the speed
and nothing else** — a run at `--threads 1` produces exactly the same duplicate
groups, keeps exactly the same original in each, and plans exactly the same
moves as a run at the default.

The run says what it chose:

```
Analysing for duplicates (8 threads)...
```

### What to set it to

The number is really a *queue depth* for your storage device, not a use of your
CPU — the hashing itself is fast, and most of the time is spent waiting on
reads. So the right answer follows the disk, not the processor:

| Where the library lives | Suggested | Why |
|---|---|---|
| **Internal SSD / NVMe** | leave it alone | The default already saturates a modern SSD. Raising it past 8 buys little and is unlikely to be measurable. |
| **External SSD over USB** | leave it alone, or `--threads 4` | Usually fine at the default; drop it if the enclosure is the bottleneck. |
| **Spinning hard disk** | `--threads 1` or `2` | A platter drive has **one head**. Every extra concurrent reader turns one sequential read into a seek storm, and the run gets *slower*, not faster — often dramatically so on a library of large videos. |
| **Network share (SMB, NFS)** | `--threads 1` to `4` | Each thread is a separate round trip. A few can hide the latency; many will simply queue, and some servers throttle a client that opens too many files at once. |
| **A machine you are still using** | `--threads 2` | Leaves cores and IO bandwidth for whatever else you are doing. The run takes longer and stays out of the way. |

```bash
# An archive on a USB hard drive — one file at a time
mmm --threads 1 /Volumes/Archive --commit

# Pin it for a machine whose library is always on the NAS
echo 'hash_threads = 2' >> ~/.config/mmm/config.toml
```

### The default, and its limits

Without a setting, `mmm` uses **as many threads as the machine has cores, up to
a maximum of 8**. The cap exists because more concurrent readers stop helping
long before core count does: a 64-core workstation firing 64 simultaneous reads
at a photo library is not eight times better than eight, and on half the storage
it might be pointed at, it is worse.

Two limits worth knowing:

- **This bounds the duplicate scan only.** Scanning for files, reading metadata
  and the move phase itself are unchanged and still work through one file at a
  time, so `--threads` will not make those faster.
- **There is no published figure for what a given count buys on a given
  device.** The project's benchmarks run against a page-cache-warm corpus, which
  measures hashing rather than disk — see
  [`docs/research/hashing-baseline.md`](research/hashing-baseline.md). If your
  library is on unusual storage, the honest way to pick a number is to time a
  preview run (`mmm --threads N ~/Photos`, which moves nothing) at two or three
  values.

`--threads 0` is refused rather than treated as "decide for me": zero means "no
bound at all" to the underlying thread pool, which is the opposite of what the
setting is for. Omit the flag to get the default.

---

## Timezones

**A photograph is filed under the time its camera displayed.** A frame taken at
23:30 in Singapore lands in `2024-03-15/` and is named `2024-03-15-233000.jpg` —
on any machine, in any zone, whatever you pass on the command line. That is the
whole of what most people need to know, and it is a change: builds before this
one read an EXIF timestamp as UTC, so the same frame landed in `2024-03-16/`
under a filename stamped `153000` for anyone east of Greenwich.

An EXIF `DateTimeOriginal` is a wall clock with no zone in it. `mmm` still works
out the real instant behind it, because comparing two photographs taken in
different places needs one, but the *instant* is not what names the directory.

### When the file did not say

Most files record no offset. `--timezone` decides what to assume for those:

```bash
# A library shot in Singapore, being organised on a laptop in Portugal
mmm ~/Photos --timezone Asia/Singapore

# A fixed offset works too — note that west-of-Greenwich offsets start with a hyphen
mmm ~/Photos --timezone -05:00
```

Set it once instead, in a config file:

```toml
default_timezone = "Asia/Singapore"
```

Resolution order, first answer wins:

1. **The file's own offset** — an EXIF `OffsetTimeOriginal` tag, an offset in an
   `.xmp` sidecar, or the offset an iPhone writes into a `.mov`. Always believed,
   over everything below it.
2. **`--timezone` / `default_timezone`.**
3. **The machine's own timezone.**
4. **UTC**, only where the machine's zone has no answer — a local time inside a
   daylight-saving gap, which never occurred.

### What `--timezone` does not do

**It does not move a photograph to a different day.** A wall clock is filed under
its own digits whatever the zone; the setting decides the instant recorded
alongside, which is what the `[tz:…]` tag reports. It *does* move files whose
timestamp is a genuine instant rather than a wall clock — a filesystem-dated
file, or a video with only a container clock — because those have to be converted
to somebody's local time before they can name a directory, and converting them to
UTC's would be the original bug again.

Every line of a preview carries its `[tz:…]` tag, and the preview summary breaks
the run down by how each zone was decided, so a run that fell back to the
machine's timezone for three thousand files tells you once rather than leaving it
to be inferred.

The reasoning, the alternatives and the known gaps are in
[`docs/decisions/adr-006-timezone-handling.md`](decisions/adr-006-timezone-handling.md).

---

## Sidecars

An `.xmp` is bound to its photograph by **filename and nothing else** — there is
no identifier inside it naming the RAW it describes. So a run that renamed
`IMG_1234.CR2` and left `IMG_1234.xmp` behind would silently detach every edit
its owner had made. `mmm` moves them together.

```
~/Photos/IMG_1234.CR2      →  ~/Organised/2024-03-15/2024-03-15-143022.cr2
~/Photos/IMG_1234.cr2.xmp  →  ~/Organised/2024-03-15/2024-03-15-143022.cr2.xmp
```

- **Both naming conventions are recognised, case-insensitively** — `IMG_1234.xmp`
  (Adobe's, matched on the stem) and `IMG_1234.CR2.xmp` (darktable's, matched on
  the whole filename).
- **The convention is preserved, not normalised.** A sidecar written the
  darktable way lands as `<new stem>.cr2.xmp`, because the tool that wrote it is
  the tool that will next go looking for it.
- **`.aae` and `.thm` count too** — Apple's edit records and camera video
  thumbnails. The list is `extensions.sidecar`, default `["xmp", "aae", "thm"]`.
- A sidecar follows its parent wherever it **actually** landed, collision suffix
  included, and follows a duplicate into `duplicates/NNN/`.
- It is never treated as a media file: not counted in the scan totals, not
  deduplicated, not dated on its own.
- Each move is journalled separately, so `mmm undo` puts both back.

### Sidecars left in place

Two cases are reported rather than guessed at, under a `Sidecars left in place`
heading and counted as `Sidecars orphaned:` in the summary:

| Reason | Example |
|---|---|
| **No parent** | `IMG_9999.xmp` with no media file of that name beside it — including one whose parent was excluded by `skip_patterns` or a narrowed extension list. |
| **More than one parent** | `IMG_1234.xmp` beside both `IMG_1234.jpg` and `IMG_1234.cr2` — an ordinary RAW+JPEG shoot. Nothing in the file breaks the tie, and attaching somebody's edits to the wrong photograph is worse than refusing. |

Neither is swept into `unsorted/`, which means "no usable date" and would say the
wrong thing.

### A sidecar can supply the date

If a photograph records no usable date of its own, `mmm` reads one out of its
`.xmp` — `exif:DateTimeOriginal`, then `photoshop:DateCreated`, then
`xmp:CreateDate` — and tags the file `[SIDECAR]`.

**This is the only way a TIFF-based RAW gets filed under the date it was taken.**
No RAW container is one `mmm` can read, so a `.cr2` library is otherwise
organised entirely by modification time while the answer sits in the text file
beside every frame. A sidecar never overrides a date the photograph itself
recorded, and a malformed one is a warning and a skip, never a failed run.

### Switching it off

```bash
mmm ~/Photos --no-sidecars      # sidecars are not collected, moved, dated or counted
```

`sidecars = false` in a config file does the same standing; `--no-sidecars=false`
answers it for one run.

---

## Refusing dates you do not trust

A filesystem timestamp is when a file was last written. On a library that has
been copied between disks, restored from a backup or synced through a cloud
service, that is the date of the **copy** — so filing by it produces a tree that
looks organised and is not.

`--require-exif` refuses to do it:

```bash
mmm ~/Photos --require-exif --commit
```

Any file whose date did not come from the file itself or its sidecar goes to
`unsorted/` instead of being filed under a date nobody recorded. **It keeps its
own filename there**, unlike the undated files in `unsorted/`, which are all
`unknown.<ext>` — a refused file has a perfectly good name and a date you merely
declined to trust, and the name is the last handle on it.

It can be set in a config file (`require_exif = true`), unlike `--commit`,
because it can only ever make a run more careful. `--require-exif=false` answers a
configured `true` for one run.

### The warning that points at it

Every run's summary counts each way a date was established:

```
  Date from EXIF: 812
  Date from XMP sidecar: 0
  Date from filesystem: 4
  Date from filesystem — metadata unreadable: 1
  Date from filesystem — format not supported: 296
  No date (unsorted): 0

  WARNING: 27% of dated files (301 of 1113) took their date from the filesystem
  rather than from the file's own metadata.
  A filesystem timestamp is when the file was last written, which on a library
  that has been copied between disks is the date of the copy — not of the
  photograph. Pass --require-exif to send those to unsorted/ instead of filing
  them under a date nobody recorded.
```

The threshold is `filesystem_date_warning_percent`, 20% of dated files by
default. `0` warns about every one; `100` never warns.

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

## Configuration

Flags you type every run can be written down instead. `mmm` reads a TOML config file, and settings resolve through four layers — **user config, then project config, then `MMM_` environment variables, then the command line** — each overriding the one before it, key by key.

Start with a commented template:

```bash
mmm config init            # ~/.config/mmm/config.toml (~/Library/Application Support/mmm/ on macOS)
mmm config init --project  # mmm.toml in the working directory
```

Then uncomment what you want:

```toml
output_dir = "/Volumes/Photos/Organised"
chunk_size = 25
date_directory_format = "%Y/%m/%d"                       # nested tree instead of one dir per day
filename_format = "{original_stem}-{date}-{time}.{ext}"  # keep the original name
skip_patterns = ["*.tmp", ".thumbnails", "raw/**"]

[extensions]
image = ["jpg", "heic", "dng"]                           # replaces the built-in list
```

A project `mmm.toml` is found by walking up from the working directory, so a library can carry its own layout. The nearest one wins.

### Answering "why did it do that?"

```bash
mmm config show      # every resolved setting, each naming the layer that decided it
mmm config path      # every location searched, and whether it was there
mmm config validate  # parse the config, report problems, run nothing
```

```
$ mmm config show
chunk_size = 25  # from: project config (/Volumes/Photos/mmm.toml)
date_directory_format = "%Y-%m-%d"  # from: built-in defaults
```

The output is itself a valid config file, so `mmm config show > mmm.toml` pins a run's settings.

### What a config file may not do

`commit`, `no_journal` and `i_know_what_im_doing` cannot be set in a file or in the environment, and writing one is an error rather than a silent no-op. Moving files stays opt-in at the command line: a run must not become destructive because of a file written months ago, or one that arrived with a copied directory.

### Failing loudly

A config that cannot be read never falls back to the defaults. A mistyped key, a wrong type, malformed TOML, an invalid format string or glob, an unrecognised `MMM_` variable, and a `--config` path that does not exist all stop the run with the file, the line and the column:

```
$ mmm ~/Photos
Error: /Volumes/Photos/mmm.toml:3:1: unknown field `chunck_size`, expected one of `output_dir`,
`chunk_size`, `no_prompt`, `verbose`, `journal_dir`, …
```

This applies to every subcommand, `mmm undo` included — `journal_dir` decides where undo *reads* journals, so proceeding on the defaults would search the wrong place. `--no-config` is the way past a broken file; `--config <PATH>` reads one named file instead of searching.

The full key table, the environment variable names and a worked precedence example are in [`docs/reference/configuration.md`](reference/configuration.md); the reasoning behind the layer order in [`docs/decisions/adr-005-configuration-precedence.md`](decisions/adr-005-configuration-precedence.md).

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
