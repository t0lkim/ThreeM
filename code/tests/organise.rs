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
//! Asserting that `2024-01-15/2024-01-15-143000.jpg` exists is weaker than it
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

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

#[cfg(unix)]
use common::deny_reads;
use common::{
    file_contents_by_marker, journals_in, metadata_snapshot, naive, snapshot_tree,
    snapshot_tree_hashed, MediaTree,
};
use mmm::geocoder::GeoLookup;
use mmm::journal::{IntentKind, Journal, JournalEntry, RunHeader};
use mmm::metadata::{DateSource, FileMetadata};
use mmm::organiser::build_target_path;
use mmm::reporter::{
    CHUNK_PROMPT_PREFIX, COMMIT_BANNER, DRY_RUN_BANNER, HASH_SKIPPED_LABEL, JOURNAL_LABEL,
    NO_JOURNAL_NOTICE, SCAN_SKIPPED_LABEL, UNPROCESSED_LABEL,
};
use mmm::settings::Settings;

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

/// Run `mmm --commit` one file per chunk, answering `replies` at the prompts.
///
/// Deliberately does *not* pass `--no-prompt`: the point is to reach the chunk
/// boundary and decline there, which is the only way to observe what the run
/// does when the operator stops it part-way through.
fn run_commit_answering(input: &Path, output: &Path, replies: &str) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--commit")
        .arg("--chunk-size")
        .arg("1")
        .write_stdin(replies.to_string())
        .output()
        .expect("running mmm in commit mode with a scripted prompt")
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

/// True if `rel` looks like `YYYY-MM-DD/<file>` — the date-directory shape.
///
/// Used where the exact date is not knowable in advance (filesystem-timestamp
/// fallback resolves to "now"), so asserting a literal path would be flaky
/// around a UTC midnight rollover.
fn is_date_tree_path(rel: &str) -> bool {
    let Some((dir, _file)) = rel.split_once('/') else {
        return false;
    };
    let parts: Vec<&str> = dir.split('-').collect();
    parts.len() == 3
        && [4, 2, 2]
            .iter()
            .zip(&parts)
            .all(|(&width, part)| part.len() == width && part.bytes().all(|b| b.is_ascii_digit()))
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
    for created in ["2024-01-15", "unsorted", "duplicates"] {
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
        vec!["2024-01-15/2024-01-15-143000.jpg".to_string()]
    );

    // ...and it is *that* file, not merely a file of that name.
    let landed = file_contents_by_marker(&out_dir);
    assert_eq!(
        landed.get("beach.jpg").map(Vec::as_slice),
        Some(["2024-01-15/2024-01-15-143000.jpg".to_string()].as_slice()),
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
            "2024-01-15/2024-01-15-143000.jpg",
        ),
        (
            "b.jpg",
            naive(2019, 3, 4, 0, 0, 0),
            "2019-03-04/2019-03-04-000000.jpg",
        ),
        (
            "c.jpg",
            naive(2021, 12, 31, 23, 59, 59),
            "2021-12-31/2021-12-31-235959.jpg",
        ),
        (
            "d.jpg",
            naive(2020, 2, 29, 12, 0, 1),
            "2020-02-29/2020-02-29-120001.jpg",
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
        without, "2024-03-20/2024-03-20-090506.jpg",
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
        &format!("2024-02-20/2024-02-20-090506-{expected_part}.jpg")
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

    let scheme = Settings::default()
        .layout()
        .expect("the built-in default formats must be valid");
    let (dir, filename) = build_target_path(&meta, "jpg", "IMG_0001", &GeoLookup::new(), &scheme);
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
        "expected a YYYY-MM-DD/ destination from the filesystem fallback, got {dest}"
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
        vec!["2024-01-15/2024-01-15-143000.jpg".to_string()],
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
        ("top.jpg", "2024-01-01/2024-01-01-010101.jpg"),
        ("a/photo.jpg", "2024-02-02/2024-02-02-020202.jpg"),
        ("a/b/photo.jpg", "2024-03-03/2024-03-03-030303.jpg"),
        ("a/b/c/deep.jpg", "2024-04-04/2024-04-04-040404.jpg"),
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

// ---------------------------------------------------------------------------
// Stopping part-way through
// ---------------------------------------------------------------------------

/// Declining at the first chunk prompt must still produce a summary.
///
/// Before this, the prompt branch called `std::process::exit(0)` from inside a
/// progress-bar `suspend` closure: the process died where it stood, so the run
/// that had just moved files reported nothing about what it had done. The
/// operator was left to work out from the tree itself which photos had already
/// been relocated.
#[test]
fn stopping_at_a_chunk_prompt_still_prints_an_accurate_summary() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("b.jpg", naive(2024, 2, 16, 15, 31, 0), None)
        .jpeg_with_exif("c.jpg", naive(2024, 3, 17, 16, 32, 0), None);

    let (_scratch, out_dir) = scratch_output();

    // One file per chunk, and "no" at the first prompt.
    let out = run_commit_answering(tree.path(), &out_dir, "n\n");
    assert_ok(&out, "commit run stopped at the first chunk prompt");
    let stdout = stdout_of(&out);

    assert_eq!(
        summary_figure(&stdout, "Files scanned:"),
        Some(3),
        "the summary must be printed even though the run was cut short\n{stdout}"
    );
    assert_eq!(
        summary_figure(&stdout, "Files organised:"),
        Some(1),
        "exactly the first chunk should have been organised\n{stdout}"
    );

    assert_eq!(
        summary_figure(&stdout, UNPROCESSED_LABEL),
        Some(2),
        "files the run never got to must be reported, not silently omitted\n{stdout}"
    );

    // What it says, against what it did.
    let landed = file_contents_by_marker(&out_dir);
    assert_eq!(
        landed.len(),
        1,
        "exactly one file should have reached the output tree, got {landed:?}"
    );
    let remaining = snapshot_tree(tree.path());
    assert_eq!(
        remaining.len(),
        2,
        "the two files the operator stopped before must still be in the input tree, got {remaining:?}"
    );
}

/// The counterpart: a run that finished must not invite the operator to look
/// for files it left behind, because there are none.
#[test]
fn a_run_that_finishes_reports_nothing_unprocessed() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("b.jpg", naive(2024, 2, 16, 15, 31, 0), None);

    let (_scratch, out_dir) = scratch_output();
    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run to completion");
    let stdout = stdout_of(&out);

    assert_eq!(summary_figure(&stdout, "Files organised:"), Some(2));
    assert_eq!(
        summary_figure(&stdout, UNPROCESSED_LABEL),
        None,
        "the line belongs only to a run that left something\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Resilience: one unreadable entry must not cost the whole run
// ---------------------------------------------------------------------------

/// The figure printed against `label` in the closing summary block.
///
/// Returns `None` when the label is absent, which is itself an assertion the
/// tests below make — the skip lines only appear when something was skipped.
fn summary_figure(stdout: &str, label: &str) -> Option<usize> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(label))
        .and_then(|line| line.rsplit(char::is_whitespace).next())
        .and_then(|figure| figure.parse().ok())
}

/// A tree with one unreadable directory and one unreadable file must still
/// organise everything else, and must say what it passed over.
///
/// Before this, either one aborted the run: the directory took `scan_directories`
/// out through `?`, and the file took `find_duplicates` out the same way. The
/// operator saw one "Permission denied" line and an untouched library, with no
/// indication that the other several thousand photos were fine.
///
/// **The two fixtures named `twin-*` share a byte length deliberately.** The
/// embedded marker is the declared path and the EXIF stamp is fixed-width, so
/// equal-length names give equal-length files — which is what puts the
/// unreadable one into the size-matched group that phase 2 actually hashes. A
/// file with a unique size is never opened by the dedup pass at all, and the
/// test would prove nothing.
///
/// Skips itself with a printed reason where permission bits do not deny reads
/// (running as root, as some CI containers do).
#[cfg(unix)]
#[test]
fn an_unreadable_directory_and_file_are_skipped_reported_and_left_alone() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("twin-a.jpg", naive(2024, 3, 4, 5, 6, 7), None)
        .jpeg_with_exif("twin-b.jpg", naive(2024, 8, 9, 10, 11, 12), None)
        .jpeg_with_exif("locked/hidden.jpg", naive(2024, 6, 7, 8, 9, 10), None);

    let (_scratch, out_dir) = scratch_output();

    let Some(locked_dir) = deny_reads(&tree.join("locked")) else {
        eprintln!(
            "SKIPPED an_unreadable_directory_and_file_are_skipped_reported_and_left_alone: \
             a 0o000 directory was still readable, so this process ignores permission bits \
             (running as root?)"
        );
        return;
    };
    let Some(_locked_file) = deny_reads(&tree.join("twin-b.jpg")) else {
        eprintln!(
            "SKIPPED an_unreadable_directory_and_file_are_skipped_reported_and_left_alone: \
             a 0o000 file was still readable, so this process ignores permission bits \
             (running as root?)"
        );
        return;
    };

    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run over a tree with unreadable entries");
    let stdout = stdout_of(&out);

    // What the run says it did.
    assert_eq!(
        summary_figure(&stdout, "Files scanned:"),
        Some(3),
        "the three reachable media files should have been found\n{stdout}"
    );
    assert_eq!(
        summary_figure(&stdout, "Files organised:"),
        Some(2),
        "the two readable files should have been organised\n{stdout}"
    );
    assert_eq!(
        summary_figure(&stdout, SCAN_SKIPPED_LABEL),
        Some(1),
        "the unreadable directory must be reported, not silently omitted\n{stdout}"
    );
    assert_eq!(
        summary_figure(&stdout, HASH_SKIPPED_LABEL),
        Some(1),
        "the unhashable file must be reported, not silently omitted\n{stdout}"
    );

    // What the run actually did.
    let landed = file_contents_by_marker(&out_dir);
    assert_eq!(
        landed.keys().cloned().collect::<Vec<_>>(),
        vec!["beach.jpg".to_string(), "twin-a.jpg".to_string()],
        "exactly the readable files should have reached the output tree"
    );

    // A file the run could not read is a file the run does not move. Both
    // unreadable entries are still exactly where the user left them.
    assert!(tree.join("twin-b.jpg").exists());

    // The directory has to be readable again before that claim can be checked
    // inside it — a 0o000 parent hides its contents from `exists()` just as
    // thoroughly as it hid them from the scan.
    drop(locked_dir);
    assert!(tree.join("locked/hidden.jpg").exists());
}

// ---------------------------------------------------------------------------
// The run journal
// ---------------------------------------------------------------------------
//
// The journal is what `mmm undo` replays, so these assertions are about the
// record rather than about the tree: which runs write one, where it goes, and
// whether it describes the moves that actually happened.

/// Run `mmm --commit` with extra arguments appended.
fn run_commit_with(input: &Path, output: &Path, extra: &[&str]) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--commit")
        .arg("--no-prompt")
        .args(extra)
        .output()
        .expect("running mmm in commit mode")
}

/// The single journal under `root`, read back through the real reader.
fn sole_journal(root: &Path) -> (RunHeader, Vec<JournalEntry>) {
    let journals = journals_in(root);
    assert_eq!(
        journals.len(),
        1,
        "expected exactly one journal under {}; found {journals:?}",
        root.display()
    );
    Journal::read(&journals[0]).expect("reading the journal the run just wrote")
}

/// A three-file tree with no duplicates, dated so the destinations are known.
fn plain_tree() -> MediaTree {
    MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("holiday/paris.jpg", naive(2024, 2, 20, 9, 5, 6), None)
        .jpeg_with_exif("holiday/rome.jpg", naive(2024, 3, 1, 8, 0, 0), None)
}

/// A preview moves nothing, so it records nothing. The metadata directory must
/// not exist at all afterwards — "wrote an empty journal" is still a write into
/// a tree the operator asked us only to look at.
#[test]
fn a_preview_writes_no_journal_at_all() {
    let tree = plain_tree();

    let out = run_preview(tree.path());

    assert_ok(&out, "preview run");
    assert!(
        metadata_snapshot(tree.path()).is_empty(),
        "a dry run left metadata behind: {:?}",
        metadata_snapshot(tree.path())
    );
    assert!(
        !tree.join(".mmm").exists(),
        "a dry run must not create the .mmm directory"
    );
    assert!(
        !stdout_of(&out).contains(JOURNAL_LABEL),
        "a preview has nothing to undo, so it must not talk about a journal:\n{}",
        stdout_of(&out)
    );
}

/// A committing run journals an intent and an outcome for every file it moved,
/// and says where the record went.
#[test]
fn a_committing_run_journals_every_move_and_prints_where() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();

    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run");
    let stdout = stdout_of(&out);

    let (header, entries) = sole_journal(&out_dir);
    assert_eq!(header.output_dir, out_dir, "the journal names its own run");
    assert!(
        header.argv.iter().any(|arg| arg == "--commit"),
        "the command line must survive on disk: {:?}",
        header.argv
    );

    let committed: Vec<&PathBuf> = entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::MoveCommitted {
                final_destination, ..
            } => Some(final_destination),
            _ => None,
        })
        .collect();
    assert_eq!(committed.len(), 3, "three files moved: {entries:?}");
    for destination in &committed {
        assert!(
            destination.exists(),
            "the journal names {} as committed, but nothing is there",
            destination.display()
        );
    }

    let intents = entries
        .iter()
        .filter(|e| matches!(e, JournalEntry::MoveIntent { .. }))
        .count();
    assert_eq!(intents, 3, "one intent per move: {entries:?}");

    assert!(
        matches!(
            entries.last(),
            Some(JournalEntry::RunCompleted {
                moved: 3,
                failed: 0,
                skipped: 0,
                ..
            })
        ),
        "a run that finished must say so on its last line: {entries:?}"
    );

    // The path is the one thing the operator needs to undo the run, so the
    // summary carries it.
    let journal_path = journals_in(&out_dir)[0].display().to_string();
    assert!(
        stdout.contains(&journal_path),
        "the run must print where its journal went:\n{stdout}"
    );
}

/// The duplicate pass is journalled through the same mechanism as the organise
/// pass, so undo can put duplicates back too.
#[test]
fn duplicate_relocations_are_journalled_with_their_group() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("copies/beach-again.jpg", "beach.jpg");
    let (_scratch, out_dir) = scratch_output();

    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run over a tree with duplicates");

    let (_header, entries) = sole_journal(&out_dir);
    let relocated: Vec<(usize, &PathBuf)> = entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::DuplicateMoved {
                group, destination, ..
            } => Some((*group, destination)),
            _ => None,
        })
        .collect();

    assert_eq!(
        relocated.len(),
        1,
        "one duplicate was relocated: {entries:?}"
    );
    assert_eq!(relocated[0].0, 0, "it belongs to the first group");
    assert!(
        relocated[0].1.exists(),
        "the journal names {} but nothing is there",
        relocated[0].1.display()
    );
    assert!(
        entries.iter().any(|e| matches!(
            e,
            JournalEntry::MoveIntent {
                kind: IntentKind::Duplicate,
                source_hash: Some(_),
                ..
            }
        )),
        "a duplicate's intent carries the digest the dedup pass already computed: {entries:?}"
    );

    // The retained original of that group was fully hashed to *prove* it was a
    // duplicate. Its organise move used to be journalled with `source_hash:
    // null` anyway, throwing away a digest already paid for — which left undo
    // with only size to go on, and a same-length edit passes a size check.
    assert!(
        entries.iter().any(|e| matches!(
            e,
            JournalEntry::MoveIntent {
                kind: IntentKind::Organise,
                source_hash: Some(_),
                ..
            }
        )),
        "the retained original's organise intent must carry the digest phase 3 \
         already computed for it: {entries:?}"
    );
}

/// A run the operator stops part-way through still closes its journal — an
/// interrupted-looking journal means something quite different to undo, and the
/// operator who declined at a prompt did not interrupt anything.
#[test]
fn a_run_stopped_at_a_chunk_boundary_still_closes_its_journal() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();

    let out = run_commit_answering(tree.path(), &out_dir, "n\n");
    assert_ok(&out, "commit run declined at the first chunk boundary");

    let (_header, entries) = sole_journal(&out_dir);
    let Some(JournalEntry::RunCompleted {
        moved,
        failed,
        skipped,
        ..
    }) = entries.last()
    else {
        panic!("a stopped run must still close its journal: {entries:?}");
    };
    assert_eq!(
        (*moved, *failed, *skipped),
        (1, 0, 2),
        "one file moved before the operator declined, and two were never attempted"
    );
}

/// `--journal-dir` is honoured by the run, not merely parsed.
#[test]
fn the_journal_directory_can_be_moved_off_the_output_tree() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();
    let elsewhere = TempDir::new().expect("creating journal TempDir");

    let out = run_commit_with(
        tree.path(),
        &out_dir,
        &["--journal-dir", &elsewhere.path().display().to_string()],
    );
    assert_ok(&out, "commit run with a relocated journal");

    assert!(
        metadata_snapshot(&out_dir).is_empty(),
        "the output tree must hold no journal when one was asked for elsewhere: {:?}",
        metadata_snapshot(&out_dir)
    );
    let written: Vec<_> = std::fs::read_dir(elsewhere.path())
        .expect("reading the relocated journal directory")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(written.len(), 1, "expected one journal; found {written:?}");
    Journal::read(&written[0]).expect("the relocated journal must be readable");
}

/// `--no-journal` writes nothing and says so. A run that cannot be undone has
/// to admit it at the one moment the operator is reading.
#[test]
fn an_unjournalled_run_says_it_cannot_be_undone() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();

    let out = run_commit_with(
        tree.path(),
        &out_dir,
        &["--no-journal", "--i-know-what-im-doing"],
    );
    assert_ok(&out, "unjournalled commit run");

    assert!(
        metadata_snapshot(&out_dir).is_empty(),
        "--no-journal must write nothing: {:?}",
        metadata_snapshot(&out_dir)
    );
    assert_eq!(
        file_contents_by_marker(&out_dir).len(),
        3,
        "the files should still have been organised"
    );
    assert!(
        stdout_of(&out).contains(NO_JOURNAL_NOTICE),
        "the summary must say the run cannot be undone:\n{}",
        stdout_of(&out)
    );
}

// ---------------------------------------------------------------------------
// Settings reaching the run
// ---------------------------------------------------------------------------
//
// A layer that resolves correctly in a unit test and never reaches the pipeline
// is a setting that does not work. These drive the real binary with a real
// project `mmm.toml` on disk, discovered the way a user's would be — by walking
// up from the working directory — and assert on what the run actually did.

/// A directory holding a project `mmm.toml`, to be used as the run's working
/// directory so discovery finds it.
fn project_config(contents: &str) -> TempDir {
    let dir = TempDir::new().expect("creating project TempDir");
    std::fs::write(dir.path().join("mmm.toml"), contents).expect("writing the project mmm.toml");
    dir
}

/// Run `mmm` from inside `project`, so the config walk starts there.
///
/// The layers *below* the project config are emptied first, and they have to
/// be: the developer's own `~/.config/mmm/config.toml` is a real layer under
/// every assertion here, and an `MMM_CHUNK_SIZE` exported in the shell that ran
/// `cargo test` outranks every file these tests write. Either would fail
/// intermittently, on one machine, and read as a bug in the tool. Nothing calls
/// `set_var` — the environment is set on the child, because Rust test binaries
/// are threaded and one test's variable is every concurrent test's variable.
fn run_in_project(project: &Path, args: &[&str]) -> std::process::Output {
    // Inside `project` rather than a directory of its own: `XDG_CONFIG_HOME`
    // points at somewhere with no `mmm/config.toml` in it, which is the whole
    // requirement, and this way it lives and dies with the fixture.
    let empty_config_home = project.join("xdg-config-home");
    std::fs::create_dir_all(&empty_config_home).expect("creating an empty config home");

    let mut cmd = Command::cargo_bin("mmm").unwrap();
    cmd.current_dir(project)
        .env("XDG_CONFIG_HOME", &empty_config_home)
        .args(args);
    for (key, _) in std::env::vars() {
        if key.starts_with("MMM_") {
            cmd.env_remove(key);
        }
    }
    cmd.output()
        .expect("running mmm inside a project directory")
}

/// A path as a TOML string. Temporary directories hold no quotes or backslashes
/// on the platforms this suite runs on, so no escaping is needed — and if that
/// ever stops being true, the parse error names the file.
fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// The setting nobody can type twice without noticing: where the run writes.
#[test]
fn a_project_config_supplies_the_output_directory() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();
    let project = project_config(&format!("output_dir = {}\n", toml_path(&out_dir)));

    let out = run_in_project(
        project.path(),
        &[
            &tree.path().display().to_string(),
            "--commit",
            "--no-prompt",
        ],
    );
    assert_ok(
        &out,
        "a commit run taking its output directory from a config",
    );

    assert_eq!(
        file_contents_by_marker(&out_dir).len(),
        3,
        "every file should have landed in the configured tree: {:?}",
        snapshot_tree(&out_dir)
    );
    assert!(
        snapshot_tree(tree.path()).is_empty(),
        "and left the input tree drained: {:?}",
        snapshot_tree(tree.path())
    );
}

/// The precedence rule, end to end: the flag wins.
#[test]
fn the_output_flag_outranks_a_project_config() {
    let tree = plain_tree();
    let (_configured, configured_dir) = scratch_output();
    let (_asked_for, asked_for_dir) = scratch_output();
    let project = project_config(&format!("output_dir = {}\n", toml_path(&configured_dir)));

    let out = run_in_project(
        project.path(),
        &[
            &tree.path().display().to_string(),
            "-o",
            &asked_for_dir.display().to_string(),
            "--commit",
            "--no-prompt",
        ],
    );
    assert_ok(&out, "a commit run whose flag contradicts the config");

    assert_eq!(
        file_contents_by_marker(&asked_for_dir).len(),
        3,
        "the flag names where the files go"
    );
    assert!(
        !configured_dir.exists(),
        "the configured directory should not even have been created"
    );
}

/// `chunk_size` is the regression this phase's wiring exists for: it used to
/// carry a clap default, which would have arrived as an opinion of the
/// highest-priority layer and outranked every config file on the machine.
///
/// Observable from outside because reaching a chunk boundary prints a question.
/// Three files with a configured size of one reaches two; the built-in default
/// of 100 would reach none.
#[test]
fn a_project_config_supplies_the_chunk_size() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();
    let project = project_config(&format!(
        "output_dir = {}\nchunk_size = 1\n",
        toml_path(&out_dir)
    ));

    let out = run_in_project(
        project.path(),
        &[&tree.path().display().to_string(), "--commit"],
    );
    assert_ok(&out, "a commit run taking its chunk size from a config");

    assert!(
        stdout_of(&out).contains(CHUNK_PROMPT_PREFIX),
        "a chunk size of one must reach a chunk boundary:\n{}",
        stdout_of(&out)
    );
    assert_eq!(
        file_contents_by_marker(&out_dir).len(),
        3,
        "and the run still organises everything"
    );
}

/// The other half of the rule, on the same setting.
#[test]
fn the_chunk_size_flag_outranks_a_project_config() {
    let tree = plain_tree();
    let (_scratch, out_dir) = scratch_output();
    let project = project_config(&format!(
        "output_dir = {}\nchunk_size = 1\n",
        toml_path(&out_dir)
    ));

    let out = run_in_project(
        project.path(),
        &[
            &tree.path().display().to_string(),
            "--chunk-size",
            "100",
            "--commit",
        ],
    );
    assert_ok(
        &out,
        "a commit run whose flag contradicts the configured size",
    );

    assert!(
        !stdout_of(&out).contains(CHUNK_PROMPT_PREFIX),
        "one chunk of 100 holds all three files, so no boundary is reached:\n{}",
        stdout_of(&out)
    );
    assert_eq!(file_contents_by_marker(&out_dir).len(), 3);
}

/// The formats, end to end, against a golden tree.
///
/// The two settings people actually want to stop retyping, in the two shapes
/// the documentation offers: a nested layout, and a filename that keeps the
/// original stem. Asserted as a whole tree rather than as "the destination
/// contains 2024" — a format that reached the organiser but was applied to only
/// one of the two would satisfy the weaker claim.
#[test]
fn a_project_config_supplies_the_dated_layout_and_the_filename() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("holiday/paris.jpg", naive(2024, 2, 20, 9, 5, 6), None);
    let (_scratch, out_dir) = scratch_output();
    let project = project_config(&format!(
        "output_dir = {}\n\
         date_directory_format = \"%Y/%Y-%m\"\n\
         filename_format = \"{{original_stem}}-{{date}}-{{time}}.{{ext}}\"\n",
        toml_path(&out_dir)
    ));

    let out = run_in_project(
        project.path(),
        &[
            &tree.path().display().to_string(),
            "--commit",
            "--no-prompt",
        ],
    );
    assert_ok(&out, "a commit run taking both formats from a config");

    assert_eq!(
        snapshot_tree(&out_dir),
        vec![
            "2024/2024-01/beach-2024-01-15-143000.jpg".to_string(),
            "2024/2024-02/paris-2024-02-20-090506.jpg".to_string(),
        ]
    );

    // ...and it is those files, not merely files of those names.
    let landed = file_contents_by_marker(&out_dir);
    assert_eq!(
        landed.get("beach.jpg").map(Vec::as_slice),
        Some(["2024/2024-01/beach-2024-01-15-143000.jpg".to_string()].as_slice())
    );
    assert_eq!(
        landed.get("holiday/paris.jpg").map(Vec::as_slice),
        Some(["2024/2024-02/paris-2024-02-20-090506.jpg".to_string()].as_slice())
    );
}

/// A renamed `duplicates_dir` moves the relocated copies, manifest and all.
///
/// The `duplicates/` name was a literal in `organiser.rs` until this phase, so
/// what this really asserts is that the setting is read at all — and the
/// negative half matters as much as the positive one: a run that wrote to both
/// would leave a photo library with two duplicate directories and no error.
#[test]
fn a_project_config_supplies_the_duplicates_directory() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("copy.jpg", "beach.jpg");
    let (_scratch, out_dir) = scratch_output();
    let project = project_config(&format!(
        "output_dir = {}\nduplicates_dir = \"copies\"\n",
        toml_path(&out_dir)
    ));

    let out = run_in_project(
        project.path(),
        &[
            &tree.path().display().to_string(),
            "--commit",
            "--no-prompt",
        ],
    );
    assert_ok(&out, "a commit run with a configured duplicates directory");

    let landed = snapshot_tree(&out_dir);
    assert!(
        landed
            .iter()
            .any(|path| path.starts_with("copies/000/") && path.ends_with("manifest.txt")),
        "the group manifest should be under the configured directory: {landed:?}"
    );
    assert!(
        landed
            .iter()
            .any(|path| path.starts_with("copies/000/") && !path.ends_with("manifest.txt")),
        "the relocated duplicate should be under the configured directory: {landed:?}"
    );
    assert!(
        !out_dir.join("duplicates").exists(),
        "the built-in name must not be written as well: {landed:?}"
    );
    assert!(
        stdout_of(&out).contains("copies/ directory"),
        "the run should say where it is putting them:\n{}",
        stdout_of(&out)
    );
}

/// A renamed `unsorted_dir`, at the level where it is reachable.
///
/// **This one is deliberately not driven through the binary, and the reason is
/// pre-existing behaviour rather than a shortcut.** See
/// `a_file_with_unparseable_exif_is_dated_from_the_filesystem_not_sent_to_unsorted`
/// above: `metadata::extract_metadata` falls back to the filesystem timestamp
/// whenever EXIF cannot be read, and every file on disk has one, so no CLI
/// invocation can put a file in the unsorted directory at all. A test that ran
/// the binary and asserted an empty `no-date/` would pass for the wrong reason
/// and would keep passing if the setting were ignored entirely.
///
/// So the config text is parsed by the real loader and resolved by the real
/// fold — only the undateable *file* is constructed directly, because that is
/// the part the CLI cannot produce.
#[test]
fn a_config_supplied_unsorted_directory_is_where_an_undateable_file_lands() {
    let layer = mmm::settings::parse_layer("unsorted_dir = \"no-date\"\n", Path::new("mmm.toml"))
        .expect("the fixture config must parse");
    let settings = Settings::resolve([layer]);
    let layout = settings.layout().expect("a valid layout");

    let meta = FileMetadata {
        date: None,
        latitude: None,
        longitude: None,
        date_source: DateSource::None,
    };
    let (dir, filename) = build_target_path(&meta, "jpg", "IMG_0001", &GeoLookup::new(), &layout);

    assert_eq!(dir, Path::new("no-date"));
    assert_eq!(filename, "unknown.jpg");
}

/// A `skip_patterns` entry keeps files out of the scan — and, because the run
/// never sees them, leaves them exactly where they are.
///
/// Both halves are asserted. "Not in the output tree" alone would also be true
/// of a file the run had deleted, and the whole point of a skip is that the
/// tool does not touch it.
#[test]
fn a_project_config_skip_pattern_excludes_files_from_the_scan() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("thumb_beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("exports/web.jpg", naive(2024, 2, 20, 9, 5, 6), None);
    let before = snapshot_tree_hashed(tree.path());
    let (_scratch, out_dir) = scratch_output();
    let project = project_config(&format!(
        "output_dir = {}\nskip_patterns = [\"thumb_*.jpg\", \"exports\"]\n",
        toml_path(&out_dir)
    ));

    let out = run_in_project(
        project.path(),
        &[
            &tree.path().display().to_string(),
            "--commit",
            "--no-prompt",
        ],
    );
    assert_ok(&out, "a commit run with configured skip patterns");

    assert_eq!(
        snapshot_tree(&out_dir),
        vec!["2024-01-15/2024-01-15-143000.jpg".to_string()],
        "only the unskipped photograph should have been organised"
    );

    // Untouched, not merely unorganised.
    let after = snapshot_tree_hashed(tree.path());
    let survivors: Vec<&String> = after.iter().collect();
    assert!(
        survivors
            .iter()
            .any(|entry| entry.contains("thumb_beach.jpg")),
        "the skipped file must still be where it was: {after:?}"
    );
    assert!(
        survivors
            .iter()
            .any(|entry| entry.contains("exports/web.jpg")),
        "the skipped directory's contents must still be where they were: {after:?}"
    );
    assert_eq!(
        before.len() - after.len(),
        1,
        "exactly one file — the unskipped one — should have left the input tree"
    );

    assert!(
        stdout_of(&out).contains("excluded by skip_patterns"),
        "a skip that quietly swallowed files would be invisible:\n{}",
        stdout_of(&out)
    );
}

/// A config file cannot make a run destructive. The file below is refused at the
/// parse, so nothing is scanned, nothing is planned, and nothing moves.
#[test]
fn a_config_file_cannot_turn_on_committing() {
    let tree = plain_tree();
    let before = snapshot_tree_hashed(tree.path());
    let project = project_config("commit = true\n");

    let out = run_in_project(project.path(), &[&tree.path().display().to_string()]);

    assert!(
        !out.status.success(),
        "a config naming a command-line-only key must stop the run:\n{}",
        stdout_of(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("commit"),
        "the refusal must name it:\n{stderr}"
    );
    assert!(
        stderr.contains("command line"),
        "and say where it belongs:\n{stderr}"
    );
    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "the library must be untouched"
    );
}
