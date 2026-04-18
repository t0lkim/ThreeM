# MultiMediaManager (ThreeM)

Image and video organiser with deduplication, EXIF-based renaming, and date-based directory structure.

## Usage

```bash
# Get help
mmm --help
mmm-dedup-verifier --help

# Preview what would happen (nothing is modified)
mmm ~/Photos --dry-run

# Organise a single directory in place
mmm ~/Photos

# Organise files from multiple sources into a single output
mmm ~/Photos ~/Camera/DCIM ~/Downloads/screenshots -o ~/Organised

# Process in smaller chunks (default: 100 files per batch)
mmm ~/Photos --chunk-size 25

# Skip confirmation prompts between chunks
mmm ~/Photos -o ~/Organised --no-prompt

# Verbose output (repeat for more detail)
mmm ~/Photos -v
mmm ~/Photos -vv

# Verify duplicates independently before deleting
mmm-dedup-verifier ~/Organised/duplicates/
```

## Features

- Recursive multi-directory scanning (22 image + 11 video formats)
- Three-phase BLAKE3 deduplication (size → partial hash → full hash)
- EXIF and video metadata extraction for original capture date
- Offline reverse geocoding via bundled GeoNames dataset
- Date-based directory structure (`YYYY/MM/DD/`)
- Chunked processing with confirmation between batches
- Dry-run mode — preview all operations before committing
- Independent `mmm-dedup-verifier` binary using keyed BLAKE3 for safety

## Language

Rust

## License

MIT
