# mmm Technical Documentation

## Architecture

The system uses a **two-pass architecture**:

- **Phase A (Scan):** Walk directories, discover media files, build the dedup table, extract metadata, plan all moves. This phase is entirely read-only. In `--dry-run` mode, execution stops here.
- **Phase B (Process):** Move duplicates to the `duplicates/` directory, then rename and move unique files into the date hierarchy. Chunked with user confirmation between batches.

```
┌─────────────────────────────────────────────────────────┐
│                    Phase A: SCAN                        │
│                                                         │
│  1. scanner.rs    → Walk dirs, filter by extension      │
│  2. hasher.rs     → Three-phase dedup cascade           │
│  3. metadata.rs   → EXIF/video metadata extraction      │
│  4. geocoder.rs   → Reverse geocode GPS coordinates     │
│  5. organiser.rs  → Plan target paths for each file     │
│  6. reporter.rs   → Print dry-run report (if --dry-run) │
└─────────────────────────────────────────────────────────┘
                          │
                    (--dry-run stops here)
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                   Phase B: PROCESS                      │
│                                                         │
│  7. organiser.rs  → Move duplicates to duplicates/NNN/  │
│  8. organiser.rs  → Execute planned moves (chunked)     │
│  9. reporter.rs   → Print summary                       │
└─────────────────────────────────────────────────────────┘
```

---

## Module Reference

| Module | Responsibility |
|---|---|
| `config.rs` | CLI argument parsing via clap derive API |
| `scanner.rs` | Recursive directory traversal, extension filtering |
| `hasher.rs` | Three-phase dedup cascade, BLAKE3 hashing |
| `metadata.rs` | EXIF extraction (images), container metadata (video), filesystem fallback |
| `geocoder.rs` | Offline reverse geocoding via GeoNames k-d tree |
| `organiser.rs` | Target path computation, atomic file moves, duplicate movement |
| `reporter.rs` | Dry-run output, duplicate listing, summary reports, chunk prompts |
| `error.rs` | Typed error definitions (thiserror) |
| `main.rs` | Orchestration, progress bars, chunked execution loop |
| `bin/dedup_verifier.rs` | Independent verification binary |

---

## Deduplication: Three-Phase Cascade

The deduplication strategy is designed to minimise I/O. Most files in a typical photo library are unique, so the goal is to prove uniqueness as cheaply as possible and only pay the cost of full-file hashing for the tiny subset that survives the cheap filters.

### Phase 1: Group by File Size

**Cost:** Zero I/O (filesystem metadata only, already collected during scan).

Files are grouped by byte size. Any file whose size is unique across the entire input set is immediately classified as unique and skipped for all further hashing.

**Typical elimination rate:** 70-90% of files. Two different photos almost never have the exact same byte count.

### Phase 2: Partial BLAKE3 Hash

**Cost:** 128KB read per file (first 64KB + last 64KB).

For files that share a size with at least one other file, a partial hash is computed. The hasher reads:

- The **first 64KB** of the file (captures headers, EXIF differences, encoding parameters)
- The **last 64KB** of the file (captures content/trailer differences)

These two chunks are fed into a single BLAKE3 hasher to produce a partial hash. Files within a size group that have different partial hashes are classified as unique.

**Why first + last:** Two photos with the same file size almost never have identical header and trailer bytes. This is especially effective for media files where headers contain unique EXIF data and trailers contain format-specific padding or checksums.

**Edge case:** If a file is smaller than 128KB total, only the first chunk is read (the entire file fits in one read).

### Phase 3: Full BLAKE3 Hash

**Cost:** Full file read (streaming, 128KB buffer).

Only files that matched on both size AND partial hash reach this phase. A streaming full-file BLAKE3 hash is computed and compared. Files with matching full hashes are confirmed as true duplicates (cryptographic certainty).

**Typical volume:** Less than 1% of input files reach this phase.

### Cascade Summary

```
All files
  │
  ├── Phase 1: Group by size ──────── unique sizes → UNIQUE (skip)
  │     │
  │     └── size matches
  │           │
  │           ├── Phase 2: Partial hash ── unique partials → UNIQUE (skip)
  │           │     │
  │           │     └── partial matches
  │           │           │
  │           │           └── Phase 3: Full hash ── unique fulls → UNIQUE
  │           │                 │
  │           │                 └── full matches → DUPLICATE GROUP
  │           │
  │           └── ...
  └── ...
```

### Implementation Details

- Hash algorithm: **BLAKE3** (standard mode, unkeyed)
- Partial hash read: first 64KB + last 64KB via `File::read_exact` and `File::seek(SeekFrom::End)`
- Full hash read: streaming 128KB buffer via `File::read` loop
- Hash output: 256-bit hex string (64 characters)
- Data structure: `HashMap<u64, Vec<ScannedFile>>` for size groups, `HashMap<String, Vec<&ScannedFile>>` for hash groups

---

## Verification: mmm vs mmm-dedup-verifier

The two binaries use **deliberately different hashing approaches** so that a bug in one cannot produce a false positive in both. This is the same principle used in safety-critical systems: independent verification channels.

### Comparison Table

| Property | mmm | mmm-dedup-verifier |
|---|---|---|
| **Purpose** | Detect duplicates, organise files | Verify that flagged duplicates are genuine |
| **Hash algorithm** | BLAKE3 standard mode (unkeyed) | BLAKE3 keyed mode |
| **Hash key** | None | `mmm-dedup-verifier-independent-key!!` (32-byte fixed key) |
| **Hashing strategy** | Three-phase cascade (size → partial → full) | Always full-file hash, no cascade |
| **Read buffer size** | 128KB | 256KB |
| **Partial hashing** | Yes (64KB head + 64KB tail in Phase 2) | No — always hashes the entire file |
| **Hash output** | Standard BLAKE3 digest | Keyed BLAKE3 digest (different value for identical input) |
| **Input** | Raw media directories | The `duplicates/` directory and manifest files |
| **Compares against** | Other files in the input set | The recorded original file path from the manifest |

### Why the Hashes Are Different

Even though both binaries use the BLAKE3 crate, they produce **different hash values for the same file**:

1. **Keyed vs unkeyed mode.** BLAKE3's keyed mode (`Hasher::new_keyed(key)`) uses a 32-byte key to derive a different internal state. The same input bytes produce a completely different output hash. This means a collision in unkeyed mode (astronomically unlikely but theoretically possible) would not be a collision in keyed mode.

2. **No shortcut path.** The main binary's three-phase cascade might classify two files as duplicates after only reading 128KB of each (Phase 2). The verifier always reads the entire file. If the cascade's partial hash produced a false match (two files identical in the first and last 64KB but different in the middle), the verifier would catch it.

3. **Different buffer sizes.** The main binary reads in 128KB chunks; the verifier reads in 256KB chunks. While this doesn't affect the final hash value (BLAKE3 is streaming and chunk-size-independent), it means the two binaries exercise different I/O paths.

### What the Verifier Proves

When `mmm-dedup-verifier` reports `[OK]` for a group, it confirms:

1. The original file still exists at the path recorded in `manifest.txt`.
2. Every file in the group directory produces the **same keyed BLAKE3 hash** as the original.
3. Since the keyed hash is computed over the entire file (no partial hashing shortcut), this is a full-content comparison with cryptographic strength.

When the verifier reports `[MISMATCH]`, it means one of:

- The file was corrupted during the move operation.
- The main binary's cascade produced a false positive (a file that matched on size + partial hash but differs in full content). This would indicate a bug in the partial hashing logic.
- The file was modified after being moved to the duplicates directory.

### Manifest File Format

Each group directory contains a `manifest.txt`:

```
# Duplicate group 000
# BLAKE3 hash: 7a3b1c4d5e6f7890abcdef1234567890abcdef1234567890abcdef1234567890
# File size: 4521984 bytes
# Original kept at: ~/Organised/2024/01/15/2024-01-15-143022.jpg

~/Photos/IMG_0042.jpg
~/Camera/DCIM/IMG_0042.jpg
```

- Lines starting with `#` are metadata (hash, size, original path).
- Non-comment lines are the source paths of the duplicate files that were moved into this group directory.
- The verifier parses the `# Original kept at:` line to locate the original for hash comparison.

---

## Metadata Extraction

### Priority Chain

```
Image files:
  1. EXIF metadata via nom-exif (DateTimeOriginal → CreateDate)
  2. Filesystem creation date (macOS btime via .created())
  3. Filesystem modification date (.modified())
  4. No date → placed in unsorted/

Video files (MOV/MP4/3GP/WebM/MKV):
  1. Container metadata via nom-exif parse_metadata()
     (CreateDate, DateTimeOriginal, com.apple.quicktime.creationdate)
  2. Filesystem creation date
  3. Filesystem modification date
  4. No date → placed in unsorted/
```

### GPS and Location

GPS coordinates are extracted from:

- **Images:** EXIF GPSLatitude/GPSLongitude tags with LatitudeRef/LongitudeRef for hemisphere
- **Videos:** `com.apple.quicktime.location.ISO6709` atom (Apple devices encode location as an ISO 6709 string like `+48.8577+002.295/`)

When GPS is available, the coordinates are reverse-geocoded using the `reverse_geocoder` crate, which loads the GeoNames dataset (bundled with the crate — no network requests) into a k-d tree. Lookups return the nearest city and country code, which are sanitised for filename safety and appended to the filename.

### Date Parsing

The metadata module handles multiple date formats:

| Format | Source | Example |
|---|---|---|
| `YYYY:MM:DD HH:MM:SS` | EXIF standard | `2024:01:15 14:30:00` |
| `YYYY-MM-DDTHH:MM:SS` | ISO 8601 | `2024-01-15T14:30:00` |
| RFC 3339 with timezone | nom-exif Time variant | `2024-02-02T08:09:57+00:00` |
| `EntryValue::Time` | nom-exif parsed DateTime | (native chrono DateTime) |

---

## File Move Safety

### Same-Volume Moves

Uses `std::fs::rename()`, which is an atomic operation on POSIX systems. The file's data is never copied — only the directory entry is updated. This is O(1) regardless of file size.

### Cross-Volume Moves

When `rename()` fails (different filesystems), the following sequence is used:

```
1. Copy source → temp file (in target directory, same volume as destination)
2. Verify: compare temp file size against source file size
3. Rename temp → final destination (atomic, same volume)
4. Delete source file
```

The temp file is named `.tmp-{unix_timestamp_millis}` and is created in the target directory to ensure the final rename is atomic (same filesystem). The source is only deleted after both the copy and verification succeed. If verification fails, the temp file is deleted and the operation is reported as an error — the source file is untouched.

### Collision Resolution

If the target filename already exists, a numeric suffix is appended:

```
2024-01-15-143022.jpg       (original)
2024-01-15-143022-1.jpg     (first collision)
2024-01-15-143022-2.jpg     (second collision)
...
```

The resolver checks existence up to 10,000 suffixes, then falls back to a millisecond timestamp suffix.

---

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `clap` | 4 | CLI argument parsing (derive API) |
| `walkdir` | 2 | Recursive directory traversal |
| `blake3` | 1 | Content hashing (standard and keyed modes) |
| `nom-exif` | 1.5 | EXIF metadata (images) and container metadata (video) |
| `reverse_geocoder` | 4 | Offline GPS reverse geocoding (GeoNames k-d tree) |
| `chrono` | 0.4 | Date/time parsing and formatting |
| `indicatif` | 0.17 | Progress bars and spinners |
| `anyhow` | 1 | Error handling for binary crate |
| `thiserror` | 2 | Typed error definitions |
| `tracing` | 0.1 | Structured logging |
| `tracing-subscriber` | 0.3 | Log formatting and filtering |
| `tempfile` | 3 | (dev) Temporary directories for tests |

---

## Build Targets

| Target | Architecture | Use |
|---|---|---|
| `aarch64-apple-darwin` | Apple Silicon (M1/M2/M3/M4) | Primary development and runtime |
| `x86_64-apple-darwin` | Intel Mac | Legacy hardware support |

Build commands:

```bash
# Debug (development)
cargo build

# Release (deployment) — both architectures
cargo build --target aarch64-apple-darwin --release
cargo build --target x86_64-apple-darwin --release

# Run tests
cargo test

# Lint
cargo clippy -- -W clippy::all

# Format check
cargo fmt --check
```

Release binaries are LTO-optimised, stripped, and built with `codegen-units = 1` for maximum performance.
