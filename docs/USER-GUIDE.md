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
mmm [OPTIONS] <DIRECTORIES>...
```

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
5. **Plans renames** — each unique file is assigned a target path: `<output>/YYYY/MM/DD/YYYY-MM-DD-HHMMSS[-location].ext`.
6. **Reports** — without `--commit`, prints the full plan and duplicate list, then exits. Nothing is created, moved or deleted.
7. **Moves duplicates** — with `--commit`, duplicate files are moved to `<output>/duplicates/000/`, `001/`, etc. Each group directory includes a `manifest.txt` recording the BLAKE3 hash and original file path.
8. **Organises** — with `--commit`, unique files are renamed and moved into the date-based hierarchy, pausing every `--chunk-size` files for confirmation.

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
- **Atomic moves on the same volume.** Uses `rename()` which is an atomic filesystem operation.
- **Cross-volume moves use copy-verify-delete.** The file is copied to a temp file on the target volume, the temp file's size is verified against the source, then it is atomically renamed to the final name. Only after verification succeeds is the source deleted.
- **Filename collisions are resolved.** If the target filename already exists, a numeric suffix (`-1`, `-2`, etc.) is appended.
- **You can stop at any chunk.** Between chunks, the tool asks whether to continue. Answering `n` stops immediately; files already moved stay moved, nothing else is touched.

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
```
