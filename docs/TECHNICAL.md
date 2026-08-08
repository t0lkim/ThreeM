# mmm Technical Documentation

## Architecture

The system uses a **two-pass architecture**:

- **Phase A (Scan):** Walk directories, discover media files, build the dedup table, extract metadata, plan all moves. This phase is entirely read-only. Without `--commit`, execution stops here — that is the default posture (see [ADR-001](decisions/adr-001-dry-run-by-default.md)).
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
│  6. reporter.rs   → Print the plan (if no --commit)     │
└─────────────────────────────────────────────────────────┘
                          │
                 (without --commit, stops here)
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
| `scanner.rs` | Recursive directory traversal, extension filtering, skip-and-count on unreadable entries |
| `hasher.rs` | Three-phase dedup cascade, BLAKE3 hashing, the bounded `HashPool`, skip-and-count on unhashable files |
| `metadata.rs` | EXIF extraction (images), container metadata (video), filesystem fallback |
| `geocoder.rs` | Offline reverse geocoding via GeoNames k-d tree |
| `naming.rs` | How names are spelled: filename sanitising, the four-digit year range |
| `organiser.rs` | Target path computation, atomic file moves, duplicate movement, chunked execution |
| `reporter.rs` | Dry-run output, duplicate listing, summary reports, chunk prompts |
| `error.rs` | Typed error definitions (thiserror) |
| `main.rs` | Orchestration, building the hashing pool, progress bars, terminal prompting via `ChunkController` |
| `bin/dedup_verifier.rs` | Independent verification binary |

---

## Deduplication: Three-Phase Cascade

The deduplication strategy is designed to minimise I/O. Most files in a typical photo library are unique, so the goal is to prove uniqueness as cheaply as possible and only pay the cost of full-file hashing for the tiny subset that survives the cheap filters.

Phase 1 is serial and reads nothing. **Phases 2 and 3 hash in parallel**, on a pool whose width the run owns — see [Concurrency](#concurrency-a-bounded-pool-of-the-runs-own) below and [ADR-007](decisions/adr-007-parallel-hashing.md) for the reasoning and the measurements.

### Phase 1: Group by File Size

**Cost:** Zero I/O (filesystem metadata only, already collected during scan). **Serial.**

Files are grouped by byte size. Any file whose size is unique across the entire input set is immediately classified as unique and skipped for all further hashing.

**Typical elimination rate:** 70-90% of files. Two different photos almost never have the exact same byte count.

It stays serial by measurement, not by oversight: 12 µs against phase 3's 94 ms on the benchmark corpus — 0.012% of the cascade. A thread pool cannot make free work cheaper.

### Phase 2: Partial BLAKE3 Hash

**Cost:** at most 128KB read per file (first 64KB + last 64KB). **Parallel.**

For files that share a size with at least one other file, a partial hash is computed. Files within a size group that have different partial hashes are classified as unique.

**Why first + last:** Two photos with the same file size almost never have identical header and trailer bytes. This is especially effective for media files where headers contain unique EXIF data and trailers contain format-specific padding or checksums.

#### Which bytes are actually read

The boundary is a named rule (`partial_coverage`), not two inline comparisons, because the promotion below turns on getting it right:

| File size | Bytes hashed | `PartialCoverage` |
|---|---|---|
| `0 ..= 64KB` | the whole file | `WholeFile` |
| `64KB+1 ..= 128KB` | the first 64KB only | `HeadOnly` |
| `128KB+1 ..` | the first and last 64KB | `HeadAndTail` |

**The middle band is the surprising one.** A 100KB file is fingerprinted on its first 64KB and nothing else, so two files of exactly that length differing only past that point both survive to phase 3 and are separated there. That is the cascade working — a partial hash is a filter, and a filter is allowed false positives. The band exists because below 128KB a tail read would overlap the head, re-reading bytes already hashed in the one phase whose job is to avoid reads. The behaviour there is deliberately frozen: changing which bytes are hashed would repartition every existing library on upgrade.

#### Files phase 2 settles outright

At `WholeFile` coverage the partial digest **is** the file's full-content digest, so a group of such files is a confirmed duplicate group the moment phase 2 buckets it. It is not sent to phase 3, which would only read every one of those files a second time to recompute a number already in hand. On a library of screenshots, thumbnails and sidecars that second read is most of the cascade's I/O.

The promotion is a property of the **read**, not of the scan record. `partial_hash` takes the scan-time size as a *check*: it compares against the length of the open handle, refuses a mismatch as a per-file skip, and refuses again if a read returns fewer bytes than that length promised. Only then is the coverage claim made.

#### Bucket key

Phase 2 buckets by **`(size, digest)`**, not by digest alone. A partial hash is head-only below 128KB, so two files of different lengths can legitimately share one — a truncated download and the file it came from. Phase 1 already separated them, and phase 2 flattens every size group into a single call, so keying by digest alone would put them back together and buy each such pair a full read it does not need.

### Phase 3: Full BLAKE3 Hash

**Cost:** Full file read (streaming, 128KB buffer). **Parallel.**

Only files that matched on both size AND partial hash, *and* whose partial hash did not already cover them entirely, reach this phase. A streaming full-file BLAKE3 hash is computed and compared. Files with matching full hashes are confirmed as true duplicates (cryptographic certainty).

Buckets by the **digest alone**, which merges nothing: equal full hashes mean equal content, which means equal size, so those files shared a size group already.

**Typical volume:** Less than 1% of input files reach this phase — and it is nonetheless 92% of the cascade's time, because it is the only phase that reads whole files.

### Cascade Summary

```
All files
  │
  ├── Phase 1: Group by size ──────── unique sizes → UNIQUE (skip)   [serial]
  │     │
  │     └── size matches
  │           │
  │           ├── Phase 2: Partial hash ── unique partials → UNIQUE (skip)
  │           │     │                                                [parallel]
  │           │     ├── partial matches, whole file covered (≤64KB)
  │           │     │     └──────────────────────→ DUPLICATE GROUP (no second read)
  │           │     │
  │           │     └── partial matches, partially covered
  │           │           │
  │           │           └── Phase 3: Full hash ── unique fulls → UNIQUE
  │           │                 │                                   [parallel]
  │           │                 └── full matches → DUPLICATE GROUP
  │           │
  │           └── ...
  └── ...
```

### Determinism

The cascade's working sets are `HashMap`s, whose iteration order differs between two runs in one process, let alone between machines. Every ordering a user can observe is therefore **settled by sorting after the hashing has collected**, which is also what keeps the answer independent of thread completion order:

- **Within a duplicate group:** sorted by `by_depth_then_path` — shallowest path first, then lexicographically smallest. `files[0]` is the retained original, the one copy left where it is.
- **Between groups:** sorted by the group's BLAKE3 digest, with the retained original's path as a tie-break. A group therefore keeps its `duplicates/NNN` number between runs over the same tree.
- **`unique`:** sorted by the same comparator. It is the order the organiser plans moves in, so it decides which of two files competing for one name gets `photo.jpg` and which gets `photo-1.jpg`.

The contract: **a run at `--threads 1` and a run at the default produce byte-identical duplicate groups, the same retained original in each, the same group numbering and the same plan order.** Only the pace changes.

### Concurrency: a bounded pool of the run's own

`HashPool` wraps a dedicated `rayon::ThreadPool`, built in `main` and passed into `find_duplicates` — never rayon's global pool, which is process-wide, built once from whoever asked first, and therefore neither reconfigurable nor testable at two widths in one process. Building it before the cascade means a thread count that cannot be spawned stops the run before the cascade opens a single file.

- **Width:** `--threads <N>` or `hash_threads`, defaulting to `min(available_parallelism(), 8)`. The bound is a queue depth for the storage device, not a use of the CPU — see [How many threads](USER-GUIDE.md#how-many-threads).
- **Flat, not group-at-a-time.** Both phases hand their whole candidate set to one call. Duplicate groups are typically *pairs*, so parallelising within a group would cap the cascade at two-way concurrency however many cores were free.
- **`NonZeroUsize` throughout,** because rayon reads `num_threads(0)` as "use your default" — a zero passed through would be no bound at all rather than a very small one. Zero and anything above 1024 are refused by `validate_hash_threads`, with one message shared by the TOML deserialiser, the `MMM_HASH_THREADS` parser and clap.
- **No shared counter.** `map(...).collect()` yields results in **input** order regardless of completion order, so the skip count is a local in a serial fold and the warnings come out reproducibly. The per-file `Result` carries the resilience contract across the thread boundary intact.

### Progress accounting

The bar counts **reads, not files**, and `find_duplicates` owns its position, length and message for the duration of the call. Counting files would make it lie about where the time goes: phase 1 retires most of a library in microseconds, so a per-file bar leaps to 90% in the first millisecond and spends the whole run on the last tenth. Each phase-2 candidate is charged two reads — the partial hash, and the full hash it may need — and the second is refunded for every file phase 2 rules out or settles. Position rises, length only falls, so the fraction never goes backwards, and the bar ends exactly full by doing the work rather than by `finish()` papering over a shortfall. A file that will not open still ticks: the read was attempted, and the operator waited for it.

### Measured

Apple M1 Pro, 8 cores, NVMe-class SSD, page-cache warm. Full tables in [`docs/research/hashing-baseline.md`](research/hashing-baseline.md).

| Measurement | Serial | Parallel | Speedup |
|---|---|---|---|
| Whole cascade | 102.49 ms | 39.174 ms | **2.62×** |
| Phase 3 — full hash | 94.347 ms | 37.181 ms | 2.54× |
| Phase 2 — partial hash | 3.6495 ms | 1.3194 ms | 2.77× |

The corpus has no file below 100 KiB, so the phase-2 promotion scores 0% there; it was measured separately over 6 400 duplicate 40 KiB files, where a whole run went 0.61 s → 0.52 s and phase 3 went from 6 400 reads to none.

### Implementation Details

- Hash algorithm: **BLAKE3** (standard mode, unkeyed)
- Partial hash read: first 64KB and, past 128KB, last 64KB, via a bounded `Read::take` and `File::seek(SeekFrom::End)`. The length comes from the open file handle and is checked against the scan record — see [Resilience](#resilience-one-bad-file-costs-one-file) below
- Full hash read: streaming 128KB buffer via `File::read` loop
- Hash output: 256-bit hex string (64 characters)
- Data structures: `HashMap<u64, Vec<ScannedFile>>` for size groups; `HashMap<(u64, PartialHash), Vec<&ScannedFile>>` for phase 2 and `HashMap<String, Vec<&ScannedFile>>` for phase 3, both produced by one generic `group_by_key`

---

## Resilience: one bad file costs one file

A photo library is a live filesystem. Files are locked, deleted and rewritten underneath a run that may take hours — a camera import still writing, a sync client rewriting a photo, a directory the user has no permission to read. None of that is exceptional, so none of it aborts a run.

`scan_directories` and `find_duplicates` are both **infallible by signature**. There is no `Result` to propagate, which is what makes "one unreadable file took the whole library down" a state the code cannot reach rather than a bug to be careful about.

| Failure | Response |
|---|---|
| A directory the walk cannot descend into | Warn, skip that entry, continue the walk. Counted in `ScanResult::skipped`. |
| A media file whose metadata cannot be read | Warn, skip that file, continue. Counted in `ScanResult::skipped`. |
| A candidate that cannot be opened or read during partial or full hashing | Warn, exclude from duplicate detection, continue. Counted in `DedupResult::skipped`. |
| A file that **shrank** between the scan and the hash | Hashed at its current length. The partial hash reads the length from the open handle and tolerates short reads, so a file that lost bytes mid-run produces a digest of what is actually there rather than an error. |

**Excluded means excluded from everything.** A file the dedup pass could not read is absent from `DedupResult::unique` too, so nothing downstream moves a file whose content was never established. Its bytes are left exactly where the user put them.

**Every skip is reported.** Both counts surface in the closing summary, on lines that appear only when the count is non-zero:

```
═══ Processing Complete ═══
  Files scanned:      1284
  Files organised:    1281
  Duplicate groups:   4
  Duplicate files:    9
  Unreadable (scan):  1
  Unhashable (dedup): 2
═══════════════════════════
```

That is the point of the counters: a summary that silently omitted files would be indistinguishable from a clean run over a library that was only partly processed. Each skipped entry also logs a `warn!` naming the path and the underlying error, which is visible at the default log level.

---

## Stopping a run part-way

Phase B is `organiser::process_moves(planned, chunk_size, controller)`. It walks the planned moves in chunks and returns a `MoveRun`; every planned file is accounted for in exactly one of its three counts:

```
moved + errors + unprocessed == planned.len()
```

Interaction is inverted out of the library through the `ChunkController` trait, which has three methods and a default for each: `chunk_started`, `file_finished`, and `should_continue`. `main` implements it over the progress bar and the `[Y/n]` prompt; a test implements it with a fixed script. The library moves files — it does not own a terminal, and **it does not end the process**.

Declining at a chunk boundary sets `stopped_early`, records the untouched remainder in `unprocessed`, and breaks the loop. The closing summary is then printed on the way out, gaining one line:

```
  Not processed:      412
```

Earlier versions called `std::process::exit(0)` from inside the progress bar's `suspend` closure. The process died where it stood: no summary, no destructors between that closure and `main`, and no answer to the only question an operator who has just stopped a run actually has — how much of it happened. `--chunk-size 0` is also read as "do not chunk" rather than passed to `slice::chunks`, which panics on a zero size.

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
# Duplicates intended for this directory: 2
#
# The paths below are written before the first move, so an
# interrupted run still records where every file came from.
# Outcomes follow, appended one line at a time as each move ends.

~/Photos/IMG_0042.jpg
~/Camera/DCIM/IMG_0042.jpg

# Outcomes
# moved: ~/Photos/IMG_0042.jpg -> ~/Organised/duplicates/000/IMG_0042.jpg
# FAILED: ~/Camera/DCIM/IMG_0042.jpg: moving … : No such file or directory (os error 2)
```

- Lines starting with `#` are metadata (hash, size, original path, per-file outcomes).
- Non-comment lines are the source paths of the duplicate files that were *intended* for this group directory.
- The verifier parses the `# Original kept at:` line to locate the original for hash comparison.

**Write ordering is a safety property, not a formatting detail.** The header and the complete intended source list are written and `fsync`ed *before* the group's first file moves; each outcome line is appended and `fsync`ed as that move ends. A run interrupted part-way through a group therefore leaves a manifest that still says where every file came from and how far the group got. Writing the manifest after the moves — as the tool did before v0.2 — meant an interrupted run left duplicates relocated with no record of their origins at all.

If the manifest itself becomes unwritable mid-group (a full disk), the remaining files in that group are left where they are and counted as errors, rather than moved without a record.

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

**A parsed date whose year will not fit in four digits is treated as no date at all**, and the file falls through to its filesystem timestamp. `chrono` accepts `-0044:03:15 10:00:00` from an EXIF `DateTimeOriginal` without complaint; filing it produced a directory named `-44` and a filename opening with `-`, which every command-line tool reads as a flag. Years `0000`–`9999` are kept and zero-padded — `0000:01:01` is what a camera with a flat battery writes, and `0000/01/01/` says so where `unsorted/unknown.jpg` would discard the file's own name.

---

## Path Derivation

The target path is `<output>/YYYY-MM-DD/YYYY-MM-DD-HHMMSS[-location].ext`, and three invariants hold over it for *any* input, not merely for the inputs the CLI happens to produce:

| Invariant | Why it is not obvious |
|---|---|
| The derived directory is either four-two-two ASCII digits or exactly `unsorted` | The year came from `{}`, not `{:04}`, so years under 1000 produced `44/03/15` and negative years `-44/03/15`. |
| The derived filename is a single ordinary path component — no `/`, no `\`, no `\0`, no leading `.` | The location suffix was sanitised; the extension was pasted in verbatim. |
| The destination is strictly inside the output directory | Follows from the two above. `build_target_path` is public and its extension argument is arbitrary text: `"../../etc/passwd"` used to land the file outside the output tree entirely. |

Both text inputs — the geocoded location and the extension — go through `naming::sanitise_for_filename`, which maps one character to one character (spaces to `-`, anything not alphanumeric/`-`/`_` to `_`). The one-for-one part matters: dropping characters instead could reduce a location name to the empty string in the middle of assembling a filename.

These are asserted as property tests in `code/tests/path_properties.rs` rather than as examples, because every example is a place somebody thought to look. The counterexamples that first broke them are checked in alongside as `path_properties.proptest-regressions` and re-run before any new case is generated.

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
| `rayon` | 1 | Parallel hashing in dedup phases 2 and 3, on a pool the run owns — see [ADR-007](decisions/adr-007-parallel-hashing.md) |
| `nom-exif` | 1.5 | EXIF metadata (images) and container metadata (video) |
| `reverse_geocoder` | 4 | Offline GPS reverse geocoding (GeoNames k-d tree) |
| `chrono` | 0.4 | Date/time parsing and formatting |
| `indicatif` | 0.17 | Progress bars and spinners |
| `anyhow` | 1 | Error handling for binary crate |
| `thiserror` | 2 | Typed error definitions |
| `tracing` | 0.1 | Structured logging |
| `tracing-subscriber` | 0.3 | Log formatting and filtering |
| `tempfile` | 3 | (dev) Temporary directories for tests |
| `criterion` | 0.8 | (dev) Throughput benchmarks for the dedup cascade — `code/benches/hashing.rs` |

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

# Benchmark the dedup cascade (~109 s; see docs/research/hashing-baseline.md)
cargo bench --bench hashing

# Compile and run every benchmark once, without sampling — for CI
cargo bench -- --test

# Lint
cargo clippy -- -W clippy::all

# Format check
cargo fmt --check
```

Release binaries are LTO-optimised, stripped, and built with `codegen-units = 1` for maximum performance.
