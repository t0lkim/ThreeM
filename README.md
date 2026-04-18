# MultiMediaManager (ThreeM)

Image and video organiser with deduplication, EXIF-based renaming, and date-based directory structure.

## Usage

```bash
# Preview what would happen (nothing is modified)
media-organiser ~/Photos --dry-run

# Organise files from multiple sources
media-organiser ~/Photos ~/Camera/DCIM -o ~/Organised

# Verify duplicates independently
dedup-verifier ~/Organised/duplicates/
```

## Features

- Recursive multi-directory scanning (22 image + 11 video formats)
- Three-phase BLAKE3 deduplication (size → partial hash → full hash)
- EXIF and video metadata extraction for original capture date
- Offline reverse geocoding via bundled GeoNames dataset
- Date-based directory structure (`YYYY/MM/DD/`)
- Chunked processing with confirmation between batches
- Dry-run mode — preview all operations before committing
- Independent `dedup-verifier` binary using keyed BLAKE3 for safety

## Language

Rust

## License

MIT
