//! Integration suite for the organiser, driven through the real `mmm` binary.
//!
//! These tests exercise the destructive path end to end: a synthetic media tree
//! goes in, the actual CLI runs against it, and the resulting tree is asserted
//! against a golden snapshot. Nothing is mocked — the binary does the scanning,
//! the EXIF parsing, the geocoding and the file moves.
//!
//! ## Why the binary and not the library
//!
//! The safety posture this phase introduced (`--commit` required, dry-run by
//! default) lives in `main`, not in the library. A library-level test would
//! happily call `execute_move` and prove nothing about whether a plain
//! `mmm ~/Photos` is safe to run by accident — which is the single most
//! important property here. So every posture assertion goes through
//! `assert_cmd`.
//!
//! ## Proving *which* file landed where
//!
//! Asserting that `2024/01/15/2024-01-15-143000.jpg` exists is weaker than it
//! looks: any file of that name satisfies it. Every fixture therefore carries
//! an embedded `MMMTEST:<declared path>;` marker, and the assertions below go
//! through [`file_contents_by_marker`] so they pin the specific source file to
//! the specific destination.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

use common::{file_contents_by_marker, naive, snapshot_tree, snapshot_tree_hashed, MediaTree};
use mmm::geocoder::GeoLookup;
use mmm::metadata::{DateSource, FileMetadata};
use mmm::organiser::build_target_path;
use mmm::reporter::{COMMIT_BANNER, DRY_RUN_BANNER};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Run `mmm` against `input`, previewing only — no `--commit`.
///
/// Deliberately passes no `-o`, so the output directory defaults to the input
/// directory. That is the most dangerous shape a real invocation can take
/// (`mmm ~/Photos`), and it is the one the default posture has to make safe.
fn run_preview(input: &Path) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .output()
        .expect("running mmm in preview mode")
}

/// Run `mmm --commit` against `input`, organising into `output`.
fn run_commit(input: &Path, output: &Path) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--commit")
        .arg("--no-prompt")
        .output()
        .expect("running mmm in commit mode")
}

/// Assert the process exited 0, printing both streams if it did not.
fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A scratch directory whose `out` child does not yet exist, so a test can
/// assert that the binary did not create it.
fn scratch_output() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("creating output TempDir");
    let out = dir.path().join("out");
    assert!(!out.exists(), "the scratch output path must start absent");
    (dir, out)
}

/// True if `rel` looks like `YYYY/MM/DD/<file>` — the date-tree shape.
///
/// Used where the exact date is not knowable in advance (filesystem-timestamp
/// fallback resolves to "now"), so asserting a literal path would be flaky
/// around a UTC midnight rollover.
fn is_date_tree_path(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    parts.len() == 4
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts[..3]
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// The default posture: a plain run must change nothing
// ---------------------------------------------------------------------------

#[test]
fn a_default_run_leaves_the_input_tree_byte_identical() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif(
            "holiday/paris.jpg",
            naive(2024, 2, 20, 9, 5, 6),
            Some((48.8584, 2.2945)),
        )
        .jpeg_raw("broken.jpg", b"not a jpeg")
        .video("clip.mov", b"not a real mov")
        .non_media("notes.txt", b"do not touch me");

    let before = snapshot_tree_hashed(tree.path());
    let out = run_preview(tree.path());
    let after = snapshot_tree_hashed(tree.path());

    assert_ok(&out, "preview run");
    assert_eq!(
        before, after,
        "a run without --commit modified the input tree — the safety posture is broken"
    );

    // Hashed snapshots cover file content and layout, but not directories that
    // were created and left empty. A dry run must not create those either.
    for created in ["2024", "unsorted", "duplicates"] {
        assert!(
            !tree.join(created).exists(),
            "preview created {created}/ in the input tree"
        );
    }

    let stdout = stdout_of(&out);
    assert!(
        stdout.contains(DRY_RUN_BANNER),
        "preview did not announce its posture:\n{stdout}"
    );
    assert!(
        !stdout.contains(COMMIT_BANNER),
        "preview announced COMMIT MODE:\n{stdout}"
    );
}

#[test]
fn a_default_run_still_prints_the_plan_it_declined_to_execute() {
    // A preview that changed nothing but also told the user nothing would be
    // useless. The listing is the entire product of a dry run.
    let tree = MediaTree::new().jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None);

    let out = run_preview(tree.path());
    assert_ok(&out, "preview run");

    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("2024-01-15-143000.jpg"),
        "the planned destination was not shown to the user:\n{stdout}"
    );
    assert!(
        stdout.contains("beach.jpg"),
        "the source file was not shown to the user:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// --commit: the date tree
// ---------------------------------------------------------------------------

#[test]
fn commit_moves_an_exif_dated_jpeg_into_its_date_path() {
    let tree = MediaTree::new().jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None);
    let (_scratch, out_dir) = scratch_output();

    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run");

    assert!(
        stdout_of(&out).contains(COMMIT_BANNER),
        "commit mode did not announce itself before moving files"
    );

    // The destination is derived from the exact datetime written into the
    // fixture's EXIF, so this pins the whole EXIF -> path pipeline.
    assert_eq!(
        snapshot_tree(&out_dir),
        vec!["2024/01/15/2024-01-15-143000.jpg".to_string()]
    );

    // ...and it is *that* file, not merely a file of that name.
    let landed = file_contents_by_marker(&out_dir);
    assert_eq!(
        landed.get("beach.jpg").map(Vec::as_slice),
        Some(["2024/01/15/2024-01-15-143000.jpg".to_string()].as_slice()),
        "the file that landed did not come from the declared source"
    );

    // A move, not a copy.
    assert!(
        !tree.join("beach.jpg").exists(),
        "the source file survived a move"
    );
    assert!(
        snapshot_tree(tree.path()).is_empty(),
        "the input tree should be empty after every file moved out: {:?}",
        snapshot_tree(tree.path())
    );
}

#[test]
fn commit_derives_the_path_from_each_file_s_own_datetime() {
    // Four files spread across boundaries that a date-formatting bug tends to
    // land on: a single-digit month and day, midnight, and the last second of
    // a year.
    let cases = [
        (
            "a.jpg",
            naive(2024, 1, 15, 14, 30, 0),
            "2024/01/15/2024-01-15-143000.jpg",
        ),
        (
            "b.jpg",
            naive(2019, 3, 4, 0, 0, 0),
            "2019/03/04/2019-03-04-000000.jpg",
        ),
        (
            "c.jpg",
            naive(2021, 12, 31, 23, 59, 59),
            "2021/12/31/2021-12-31-235959.jpg",
        ),
        (
            "d.jpg",
            naive(2020, 2, 29, 12, 0, 1),
            "2020/02/29/2020-02-29-120001.jpg",
        ),
    ];

    let mut tree = MediaTree::new();
    for (rel, dt, _) in cases {
        tree = tree.jpeg_with_exif(rel, dt, None);
    }
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let landed = file_contents_by_marker(&out_dir);
    for (rel, _, expected) in cases {
        assert_eq!(
            landed.get(rel).map(Vec::as_slice),
            Some([expected.to_string()].as_slice()),
            "{rel} did not land at {expected}; whole tree was {:?}",
            snapshot_tree(&out_dir)
        );
    }
}

// ---------------------------------------------------------------------------
// GPS
// ---------------------------------------------------------------------------

#[test]
fn a_gps_tagged_jpeg_gains_a_location_suffix() {
    // Same second, two files: one with coordinates, one without. Holding the
    // datetime constant means the *only* difference between the two filenames
    // is the thing under test. (Different dates keep them out of each other's
    // way in the output tree — a same-path collision would muddy the result.)
    let tree = MediaTree::new()
        .jpeg_with_exif(
            "with-gps.jpg",
            naive(2024, 2, 20, 9, 5, 6),
            Some((-33.8688, 151.2093)), // Sydney
        )
        .jpeg_with_exif("without-gps.jpg", naive(2024, 3, 20, 9, 5, 6), None);
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");
    let landed = file_contents_by_marker(&out_dir);

    let with = &landed.get("with-gps.jpg").expect("GPS file did not land")[0];
    let without = &landed
        .get("without-gps.jpg")
        .expect("non-GPS file did not land")[0];

    assert_eq!(
        without, "2024/03/20/2024-03-20-090506.jpg",
        "a file without coordinates must get a bare date filename"
    );

    // The suffix itself comes from the geocoder, whose city-name output is a
    // property of the bundled GeoNames dataset rather than of this crate — so
    // this asserts the *plumbing* (EXIF GPS reaches the geocoder and reaches
    // the filename) against the geocoder's own answer, not a hard-coded city
    // that a dataset refresh would invalidate.
    let expected_part = GeoLookup::new()
        .lookup(-33.8688, 151.2093)
        .expect("geocoder returned nothing for Sydney")
        .filename_part;
    assert_eq!(
        with,
        &format!("2024/02/20/2024-02-20-090506-{expected_part}.jpg")
    );

    // Independently of the dataset, the suffix must be non-empty and must
    // carry the right country — a suffix of "" or the wrong hemisphere would
    // satisfy the assertion above if the geocoder were broken in the same way
    // in both places.
    assert!(
        with.ends_with("-AU.jpg"),
        "Sydney coordinates should geocode to an AU location, got {with}"
    );
}

// ---------------------------------------------------------------------------
// Files with no usable date
// ---------------------------------------------------------------------------

#[test]
fn the_unsorted_path_is_used_when_a_file_genuinely_has_no_date() {
    // `unsorted/` is the organiser's answer to a file with no date at all.
    // This pins that contract at the level where it is actually reachable —
    // see the CLI-level test below for why it cannot be reached end to end.
    let meta = FileMetadata {
        date: None,
        latitude: None,
        longitude: None,
        date_source: DateSource::None,
    };

    let (dir, filename) = build_target_path(&meta, "jpg", &GeoLookup::new());
    assert_eq!(dir, Path::new("unsorted"));
    assert_eq!(filename, "unknown.jpg");
}

#[test]
fn a_file_with_unparseable_exif_is_dated_from_the_filesystem_not_sent_to_unsorted() {
    // NOTE — this documents real behaviour that differs from what one might
    // expect. `metadata::extract_metadata` falls back to the filesystem's
    // created/modified timestamp whenever EXIF cannot be parsed, and a file on
    // disk always has one. So no CLI invocation can put a file in `unsorted/`:
    // the fallback intercepts first, and the file is organised under the date
    // it was written instead.
    //
    // Whether that is the right product decision is a separate question (a
    // fabricated "date" from a copy operation is not the photo's date). It is
    // asserted here so the behaviour is pinned and visible rather than
    // discovered later by someone wondering why `unsorted/` is always empty.
    let tree = MediaTree::new().jpeg_raw("broken.jpg", b"this is not a JPEG");
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let landed = file_contents_by_marker(&out_dir);
    let dest = &landed.get("broken.jpg").expect("the file did not land")[0];

    assert!(
        !dest.starts_with("unsorted/"),
        "a filesystem-dated file reached unsorted/ — the fallback in \
         metadata::extract_metadata must have changed, which is a real \
         behavioural change worth reviewing rather than just re-baselining"
    );
    assert!(
        is_date_tree_path(dest),
        "expected a YYYY/MM/DD/ destination from the filesystem fallback, got {dest}"
    );
    // The scanner lower-cases every extension it records, so the destination
    // extension is exactly "jpg" — asserting the precise value rather than a
    // suffix match also pins that normalisation.
    assert_eq!(
        Path::new(dest).extension().and_then(|e| e.to_str()),
        Some("jpg"),
        "the original extension must be preserved, got {dest}"
    );
}

// ---------------------------------------------------------------------------
// Non-media
// ---------------------------------------------------------------------------

#[test]
fn non_media_files_are_never_touched_in_either_mode() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .non_media("notes.txt", b"plain text")
        .non_media("manual.pdf", b"%PDF-1.4 not really")
        .non_media("nested/deep/receipt.pdf", b"%PDF-1.4 also not really")
        .non_media("no-extension", b"bare file");

    let non_media: Vec<String> = snapshot_tree_hashed(tree.path())
        .into_iter()
        .filter(|line| !line.starts_with("beach.jpg"))
        .collect();
    assert_eq!(non_media.len(), 4, "fixture setup: {non_media:?}");

    // Preview mode.
    let (_s1, out_dir_a) = scratch_output();
    assert_ok(&run_preview(tree.path()), "preview run");
    assert_eq!(
        snapshot_tree_hashed(tree.path())
            .into_iter()
            .filter(|line| !line.starts_with("beach.jpg"))
            .collect::<Vec<_>>(),
        non_media,
        "a preview run disturbed a non-media file"
    );
    assert!(!out_dir_a.exists());

    // Commit mode — the media file leaves, everything else stays put.
    let (_s2, out_dir_b) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir_b), "commit run");

    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        non_media,
        "a commit run moved or altered a non-media file"
    );
    assert_eq!(
        snapshot_tree(&out_dir_b),
        vec!["2024/01/15/2024-01-15-143000.jpg".to_string()],
        "a non-media file was copied into the output tree"
    );
}

// ---------------------------------------------------------------------------
// Recursion
// ---------------------------------------------------------------------------

#[test]
fn nested_subdirectories_are_traversed_and_their_files_organised() {
    // Depth, a directory with no media of its own, and two files that share a
    // leaf name at different depths — the flat date tree has to keep them
    // apart on their own merits.
    let tree = MediaTree::new()
        .jpeg_with_exif("top.jpg", naive(2024, 1, 1, 1, 1, 1), None)
        .jpeg_with_exif("a/photo.jpg", naive(2024, 2, 2, 2, 2, 2), None)
        .jpeg_with_exif("a/b/photo.jpg", naive(2024, 3, 3, 3, 3, 3), None)
        .jpeg_with_exif("a/b/c/deep.jpg", naive(2024, 4, 4, 4, 4, 4), None)
        .non_media("a/b/notes.txt", b"stay")
        .empty_dir("a/b/c/empty");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let landed = file_contents_by_marker(&out_dir);
    for (declared, expected) in [
        ("top.jpg", "2024/01/01/2024-01-01-010101.jpg"),
        ("a/photo.jpg", "2024/02/02/2024-02-02-020202.jpg"),
        ("a/b/photo.jpg", "2024/03/03/2024-03-03-030303.jpg"),
        ("a/b/c/deep.jpg", "2024/04/04/2024-04-04-040404.jpg"),
    ] {
        assert_eq!(
            landed.get(declared).map(Vec::as_slice),
            Some([expected.to_string()].as_slice()),
            "{declared} did not land at {expected}; whole tree was {:?}",
            snapshot_tree(&out_dir)
        );
    }

    // Nothing invented, nothing lost: four media files in, four out.
    assert_eq!(snapshot_tree(&out_dir).len(), 4);
    assert_eq!(
        snapshot_tree(tree.path()),
        vec!["a/b/notes.txt".to_string()],
        "only the non-media file should remain behind"
    );
}

// ---------------------------------------------------------------------------
// Empty input
// ---------------------------------------------------------------------------

#[test]
fn an_empty_input_directory_exits_successfully_with_no_output_tree() {
    let tree = MediaTree::new();

    // Preview.
    let preview = run_preview(tree.path());
    assert_ok(&preview, "preview run over an empty directory");
    assert!(snapshot_tree(tree.path()).is_empty());

    // Commit — the mode that would actually create directories.
    let (_scratch, out_dir) = scratch_output();
    let commit = run_commit(tree.path(), &out_dir);
    assert_ok(&commit, "commit run over an empty directory");

    assert!(
        !out_dir.exists(),
        "an empty input created an output tree at {}",
        out_dir.display()
    );
    assert!(snapshot_tree(tree.path()).is_empty());
}

#[test]
fn a_directory_holding_only_non_media_is_treated_as_empty() {
    // The scanner filters by extension, so a directory full of documents has
    // no media in it — same outcome as an empty one, and equally must not
    // produce an output tree.
    let tree = MediaTree::new()
        .non_media("notes.txt", b"text")
        .non_media("sub/manual.pdf", b"%PDF-1.4")
        .empty_dir("sub/empty");

    let before = snapshot_tree_hashed(tree.path());
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    assert!(
        !out_dir.exists(),
        "a non-media-only input created an output tree"
    );
    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "a non-media-only input tree was modified"
    );
}
