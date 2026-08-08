# MultiMediaManager (ThreeM)

Image and video organiser with deduplication, EXIF-based renaming, and date-based directory structure.

## Safety

`mmm` moves other people's photograph libraries, so the whole tool is built around one rule: **nothing moves unless you say so, and anything that moved can be put back.**

- **Every run is a preview until you pass `--commit`.** Without it, `mmm` scans, plans, prints and exits without creating, moving or deleting a single file — the `duplicates/` directory is not even made. Read the plan, then re-run the identical command with `--commit`.
- **Every committing run is journalled before it acts.** Each move is written to `<output>/.mmm/journal/<run_id>.jsonl` and flushed to disk *before* the file is touched, so a run killed by `Ctrl-C`, a power cut or a full disk still leaves a record of what it moved and where. The journal path is printed as the run starts and again in the closing summary.
- **`mmm undo` replays that journal backwards.** It restores files to where they came from, refuses to move anything whose destination is no longer the file it put there, and is itself journalled — so an undo can be undone. `mmm undo` previews; `mmm undo --commit` acts.
- **Nothing is ever overwritten.** A same-volume move is `link()` + `unlink()`, and `link()` refuses an occupied destination — including a dangling symlink, which an `exists()` check reports as free. A cross-volume move copies to a temp file, compares the BLAKE3 digest of what was read against the digest of what landed, and only then deletes the source. Name collisions get a `-1`, `-2` suffix.
- **Originals are never deleted during deduplication.** One file in each duplicate group stays where it is; only the copies move to `duplicates/`, each group with a `manifest.txt` recording where every file came from. Verify them independently with `mmm-dedup-verifier` before you delete anything.
- **One unreadable file costs one file.** A directory that cannot be read or a photo that cannot be opened is skipped with a warning and counted in the summary — never silently, and never by aborting the run. A file whose contents could not be established is never moved.
- **`--no-journal` is the one way to lose this,** and combined with `--commit` it is refused unless `--i-know-what-im-doing` is also passed.

The reasoning behind the posture is in [ADR-001](docs/decisions/adr-001-dry-run-by-default.md), [ADR-003](docs/decisions/adr-003-atomic-move-semantics.md) and [ADR-004](docs/decisions/adr-004-journal-design.md).

## Usage

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

# Read one file at a time — for a spinning disk or a network share
mmm ~/Photos --threads 1 --commit

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

# Write down the flags you keep typing
mmm config init            # a starter config with every key commented out
mmm config show            # every resolved setting, naming the layer that decided it
mmm config path            # every location searched for a config
mmm config validate        # parse the config, report problems, run nothing
```

Review a plain run first, then re-run the same command with `--commit` to apply it.
`--dry-run` is still accepted as a deprecated no-op so old scripts keep working.

## Features

- Recursive multi-directory scanning — 32 extensions (21 image + 11 video)
- Three-phase BLAKE3 deduplication (size → partial hash → full hash), with phases 2 and 3 hashing in parallel on a pool the run owns — `--threads` sets its width ([ADR-007](docs/decisions/adr-007-parallel-hashing.md))
- EXIF and video metadata extraction for original capture date, from four container families: JPEG, HEIF (HEIC/HEIF/AVIF), QuickTime and MP4. Anything else — every TIFF-based RAW included — falls back to the filesystem timestamp and **says so** per file rather than passing it off as a real date. Verified per format in [`docs/reference/format-support.md`](docs/reference/format-support.md)
- **XMP sidecar support** — `.xmp`, `.aae` and `.thm` files travel with the photograph they belong to, under either naming convention; and a file with no readable date of its own takes one from its `.xmp`, which is the only way a RAW library gets filed by capture date
- **Local wall-clock filing** — an EXIF timestamp is read as what the camera's clock displayed, not as UTC, so a photograph lands under the day it was taken wherever the tool is run. `--timezone` sets what to assume when a file recorded no offset ([ADR-006](docs/decisions/adr-006-timezone-handling.md))
- `--require-exif` to refuse filesystem timestamps outright, routing those files to `unsorted/` under their own names
- Offline reverse geocoding via bundled GeoNames dataset, with coordinates outside the ISO 6709 bounds refused rather than filed under an invented place
- Date-based directory structure, `YYYY-MM-DD/` by default — the directory layout, the filename pattern, the scanned extensions and the `duplicates/` and `unsorted/` names are all configurable ([`docs/reference/configuration.md`](docs/reference/configuration.md))
- **Layered configuration** — user config, then project `mmm.toml`, then `MMM_` environment variables, then the command line, each overriding the one before it; `mmm config show` names the layer that decided every value ([ADR-005](docs/decisions/adr-005-configuration-precedence.md))
- Chunked processing with confirmation between batches, and a closing summary that accounts for every file whether the run finished or you stopped it
- Safe by default — every run is a preview until you pass `--commit`
- **`mmm undo`** — every committing run is journalled before it acts, so it can be replayed backwards and the library put back as it was, even after an interrupted run
- `mmm journal list` / `mmm journal show` to inspect what has been run against a library
- Independent `mmm-dedup-verifier` binary using keyed BLAKE3 for safety

## Building

The crate root is `code/`, not the repository root.

```bash
cd code
cargo build --release          # binaries at code/target/release/{mmm,mmm-dedup-verifier}
cargo install --path .         # or install both into ~/.cargo/bin
cargo test --all-targets       # the full suite
```

There is no installer and no packaged build yet; `cargo install --path code` is the supported way to get `mmm` onto a `PATH`.

Rust **1.87.0** or newer. That floor is declared as `rust-version` in `code/Cargo.toml` and checked by its own CI job, so it is a measurement rather than an aspiration.

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) covers the `code/` layout, the four-command test gate CI enforces, and the rule that any change to a path which moves, records or restores files lands as a failing test first. Bugs go through the [issue template](.github/ISSUE_TEMPLATE/bug_report.md); vulnerabilities go to the contact in [`SECURITY.md`](SECURITY.md), not the issue tracker.

## Documentation

Start at [`docs/index.md`](docs/index.md), which links every document in the repository. The two entry points are the [User Guide](docs/USER-GUIDE.md) and the [Technical Documentation](docs/TECHNICAL.md).

## Language

Rust

## License

MIT
