# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **BREAKING — previewing is now the default; moving files requires `--commit`.** A bare `mmm ~/Photos` scans, plans, prints the planned moves and exits without touching a single file. Passing `--commit` is the only way to move anything. Output defaults to the first input directory, so the most natural invocation used to rewrite the user's photo library in place with no confirmation and no undo. See [`docs/decisions/adr-001-dry-run-by-default.md`](docs/decisions/adr-001-dry-run-by-default.md).
- Deduplication is gated by `--commit` as well — a preview run reports the duplicate groups it found without creating a `duplicates/` directory.
- A posture banner is printed **before the scan**: `DRY RUN — no files will be modified. Re-run with --commit to apply.` or `COMMIT MODE — files will be moved.`
- `--help` states the safety posture in three places: a `SAFE BY DEFAULT` summary, the `--commit` flag's own text, and a `SAFETY` + `EXAMPLES` block.
- `find_duplicates` takes `&[ScannedFile]` instead of consuming a `Vec`.
- `README.md`, `docs/USER-GUIDE.md` and `docs/TECHNICAL.md` updated so no example command that moves files is missing `--commit`.

### Deprecated

- `--dry-run` / `-d` is now a hidden no-op, kept so existing scripts do not fail on an unknown argument. Passing it prints a one-line deprecation notice to stderr. `--dry-run --commit` resolves to commit — the retired flag is a no-op, not a veto.

### Added

- **Library target `mmm`** (`code/src/lib.rs`) re-exporting `config`, `error`, `geocoder`, `hasher`, `metadata`, `organiser`, `reporter` and `scanner`. Every module was previously private to `main.rs`, which made `tests/` integration tests structurally impossible. Both binaries now consume the library.
- **Offline fixture harness** (`code/tests/common/mod.rs`) — a `MediaTree` builder that synthesises byte-valid JPEGs carrying hand-built EXIF (`DateTimeOriginal`, `OffsetTimeOriginal`, and an optional GPS IFD with rational latitude/longitude and their `Ref` fields), plus `snapshot_tree`, `snapshot_tree_hashed` and `file_contents_by_marker` helpers for golden-tree assertions. No network access and no checked-in test assets.
- **Integration test suites** driving the real binary via `assert_cmd`:
  - `code/tests/fixture_selftest.rs` (6 tests) — proves the synthesised EXIF round-trips through `metadata::extract_metadata`, with a negative control so `DateSource::Exif` cannot pass vacuously.
  - `code/tests/organise.rs` (11 tests) — a default run leaves the input byte-identical; `--commit` files EXIF-dated JPEGs under `YYYY/MM/DD/`; GPS-tagged files gain the location suffix; non-media files are never touched; nested trees are traversed; an empty input exits cleanly with no output tree.
  - `code/tests/dedup.rs` (10 tests) — group formation, `duplicates/NNN/manifest.txt` contents, same-size-different-content files are not grouped, no duplicates means no `duplicates/` directory, and a BLAKE3 multiset conservation check proving no file is ever lost or invented.
- **CI** at `.github/workflows/ci.yml` — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` across `ubuntu-latest` and `macos-latest`, plus a second job running `cargo build --release` on both to catch release-profile-only breakage.
- `code/rustfmt.toml`, and a `[lints.clippy]` section denying `unwrap_used` / `expect_used` and warning on `pedantic`.
- `Config::is_dry_run()`, `Config::deprecation_notice()`, `reporter::print_mode_banner()`, and the `reporter::{DRY_RUN_BANNER, COMMIT_BANNER}` / `config::DRY_RUN_DEPRECATION_NOTICE` constants.
- `impl Default for GeoLookup`.

### Fixed

- **Removed every panicking `expect()` from non-test code.** Four `ProgressStyle::…expect("valid template")` calls collapsed into a shared `hasher::styled_bar()` helper that falls back to the default bar — a cosmetic failure must never abort a run mid-move. `reporter::prompt_continue` now returns `false` on a failed stdout flush rather than panicking: if the prompt could not be shown, there is no consent to continue.
- Unchecked `as` casts in `hasher::partial_hash` replaced with `usize::try_from` / `i64::try_from` that propagate as errors.
- 68 clippy pedantic findings resolved rather than suppressed, including 8 missing `# Errors` doc sections. Three lints are allowed with written reasons in `Cargo.toml`.
- A dedup assertion that passed on APFS and failed on ext4 because it named the retained file rather than deriving it from scan order — caught by the new CI matrix on its first run.

### Known issues

- `mmm-dedup-verifier` is vacuous against a tree `mmm` itself produced, and still exits 0. `move_duplicates` runs before the organise pass, so a finished `manifest.txt` records each original at its *input* path — which the organise pass then empties. The verifier finds nothing, confirms zero groups, prints "All verified groups are confirmed duplicates" and exits 0. Only `--check-originals` turns it into a failure. Pinned by a test that will fail loudly if the manifest is ever fixed.
- `duplicates/NNN` group numbering is not stable across runs — groups are accumulated by iterating a `HashMap`, whose ordering is randomly seeded per process. `duplicates/000/` is only reliable when the tree holds exactly one group.
- The dry-run listing is not a faithful preview of final filenames when two files resolve to the same destination. Collision suffixing (`-1`) happens in `execute_move`, not in `plan_move`.
- `unsorted/` is unreachable through the CLI. `metadata::extract_metadata` falls back to the filesystem timestamp whenever EXIF fails to parse, and a file on disk always has one, so an undateable photo is filed under the date it was *written* rather than under `unsorted/`.
- `nom-exif` resolves a naive EXIF timestamp against the machine's local timezone, so output paths and filenames are machine-dependent for any photo lacking `OffsetTimeOriginal`. CI currently only ever exercises UTC.

[Unreleased]: https://github.com/t0lkim/ThreeM/commits/main
