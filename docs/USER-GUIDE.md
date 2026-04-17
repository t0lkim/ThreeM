# media-organiser User Guide

## Overview

`media-organiser` scans one or more directories for images and videos, detects duplicates, renames files by date and location, and sorts them into a `Year/Month/Day` directory hierarchy. A companion tool, `dedup-verifier`, independently verifies that flagged duplicates are genuine before you delete them.

Both binaries are installed at `~/bin/`.

---

## Quick Start

Preview what would happen (no files are modified):

```bash
media-organiser ~/Photos --dry-run
```

Organise files from multiple sources into a single output directory:

```bash
media-organiser ~/Photos ~/Camera/DCIM -o ~/Organised
```

After organising, verify the duplicates directory:

```bash
dedup-verifier ~/Organised/duplicates/
```

---

## media-organiser

### Usage

```
media-organiser [OPTIONS] <DIRECTORIES>...
```

### Arguments

| Argument | Description |
|---|---|
| `<DIRECTORIES>...` | One or more directories to scan (recursive) |

### Options

| Flag | Short | Default | Description |
|---|---|---|---|
| `--output <DIR>` | `-o` | First input directory | Where organised files and the `duplicates/` directory are written |
| `--dry-run` | `-d` | off | Show what would happen without moving any files |
| `--chunk-size <N>` | `-c` | 100 | Number of files to process before pausing for confirmation |
| `--no-prompt` | | off | Skip confirmation prompts between chunks |
| `--verbose` | `-v` | warn | Increase log verbosity (`-v` info, `-vv` debug, `-vvv` trace) |
| `--help` | `-h` | | Print help |
| `--version` | `-V` | | Print version |

### What It Does

1. **Scans** all input directories recursively for media files.
2. **Deduplicates** using a three-phase hash cascade (see Technical Documentation).
3. **Extracts metadata** — creation date and GPS coordinates from EXIF (images) or container atoms (video). Falls back to filesystem creation date when metadata is absent.
4. **Plans renames** — each unique file is assigned a target path: `<output>/YYYY/MM/DD/YYYY-MM-DD-HHMMSS[-location].ext`.
5. **Reports** — in dry-run mode, prints the full plan and duplicate list, then exits.
6. **Moves duplicates** — in live mode, duplicate files are moved to `<output>/duplicates/000/`, `001/`, etc. Each group directory includes a `manifest.txt` recording the BLAKE3 hash and original file path.
7. **Organises** — unique files are renamed and moved into the date-based hierarchy, pausing every `--chunk-size` files for confirmation.

### Dry Run Output

A dry run produces two reports:

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

- **Dry run modifies nothing.** Not a single file is created, moved, or deleted.
- **Originals are never deleted during dedup.** The first file in each group is kept; only copies are moved to `duplicates/`.
- **Atomic moves on the same volume.** Uses `rename()` which is an atomic filesystem operation.
- **Cross-volume moves use copy-verify-delete.** The file is copied to a temp file on the target volume, the temp file's size is verified against the source, then it is atomically renamed to the final name. Only after verification succeeds is the source deleted.
- **Filename collisions are resolved.** If the target filename already exists, a numeric suffix (`-1`, `-2`, etc.) is appended.
- **You can stop at any chunk.** Between chunks, the tool asks whether to continue. Answering `n` stops immediately; files already moved stay moved, nothing else is touched.

---

## dedup-verifier

### Usage

```
dedup-verifier [OPTIONS] <DUPLICATES_DIR>
```

### Arguments

| Argument | Description |
|---|---|
| `<DUPLICATES_DIR>` | Path to the `duplicates/` directory created by `media-organiser` |

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
# 1. Dry run to review the plan
media-organiser ~/Photos --dry-run

# 2. Run for real
media-organiser ~/Photos -o ~/Organised

# 3. Verify duplicates independently
dedup-verifier ~/Organised/duplicates/

# 4. If all [OK], safe to delete duplicates
rm -rf ~/Organised/duplicates/

# 5. If any [MISMATCH], investigate before deleting
```
