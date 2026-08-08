# MultiMediaManager (ThreeM)

Image and video organiser with deduplication, EXIF-based renaming, and date-based directory structure.

## Usage

**Safe by default.** `mmm` previews and changes nothing unless you pass `--commit`.

```bash
# Get help
mmm --help
mmm-dedup-verifier --help

# Preview what would happen (nothing is modified — this is the default)
mmm ~/Photos

# Organise a single directory in place — MOVES FILES
mmm ~/Photos --commit

# Organise files from multiple sources into a single output — MOVES FILES
mmm ~/Photos ~/Camera/DCIM ~/Downloads/screenshots -o ~/Organised --commit

# Process in smaller chunks (default: 100 files per batch)
mmm ~/Photos --chunk-size 25 --commit

# Skip confirmation prompts between chunks
mmm ~/Photos -o ~/Organised --no-prompt --commit

# Verbose output (repeat for more detail)
mmm ~/Photos -v
mmm ~/Photos -vv

# Assume a timezone for files that recorded no offset of their own
mmm ~/Photos --timezone Asia/Singapore

# Refuse filesystem timestamps — anything not dated by the file goes to unsorted/
mmm ~/Photos --require-exif --commit

# Leave .xmp / .aae / .thm sidecars where they are
mmm ~/Photos --no-sidecars --commit

# Verify duplicates independently before deleting
mmm-dedup-verifier ~/Organised/duplicates/

# See what has been run against a library
mmm journal list ~/Organised

# Put the last run back — preview first, then commit
mmm undo ~/Organised
mmm undo ~/Organised --commit
```

Review a plain run first, then re-run the same command with `--commit` to apply it.
`--dry-run` is still accepted as a deprecated no-op so old scripts keep working.

## Features

- Recursive multi-directory scanning — 32 extensions (21 image + 11 video)
- Three-phase BLAKE3 deduplication (size → partial hash → full hash)
- EXIF and video metadata extraction for original capture date, from four container families: JPEG, HEIF (HEIC/HEIF/AVIF), QuickTime and MP4. Anything else — every TIFF-based RAW included — falls back to the filesystem timestamp and **says so** per file rather than passing it off as a real date. Verified per format in [`docs/reference/format-support.md`](docs/reference/format-support.md)
- **XMP sidecar support** — `.xmp`, `.aae` and `.thm` files travel with the photograph they belong to, under either naming convention; and a file with no readable date of its own takes one from its `.xmp`, which is the only way a RAW library gets filed by capture date
- **Local wall-clock filing** — an EXIF timestamp is read as what the camera's clock displayed, not as UTC, so a photograph lands under the day it was taken wherever the tool is run. `--timezone` sets what to assume when a file recorded no offset ([ADR-006](docs/decisions/adr-006-timezone-handling.md))
- `--require-exif` to refuse filesystem timestamps outright, routing those files to `unsorted/` under their own names
- Offline reverse geocoding via bundled GeoNames dataset
- Date-based directory structure (`YYYY-MM-DD/`)
- Chunked processing with confirmation between batches
- Safe by default — every run is a preview until you pass `--commit`
- **`mmm undo`** — every committing run is journalled before it acts, so it can be replayed backwards and the library put back as it was, even after an interrupted run
- `mmm journal list` / `mmm journal show` to inspect what has been run against a library
- Independent `mmm-dedup-verifier` binary using keyed BLAKE3 for safety

## Language

Rust

## License

MIT
