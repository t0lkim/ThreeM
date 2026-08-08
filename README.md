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

- Recursive multi-directory scanning (22 image + 11 video formats)
- Three-phase BLAKE3 deduplication (size → partial hash → full hash)
- EXIF and video metadata extraction for original capture date
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
