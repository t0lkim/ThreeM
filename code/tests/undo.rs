//! Integration suite for `mmm undo` and `mmm journal`, driven through the real
//! binary.
//!
//! The unit tests in `src/undo.rs` prove the rules in isolation: reverse order,
//! verification before every move, the collision walk, the exit-code decision.
//! What they cannot prove is that a real run, journalled by the real organiser,
//! is actually reversible — that the path the organiser wrote into the journal
//! is the path undo goes looking for, that the two agree about where the journal
//! itself lives, and that the tree afterwards is the tree from before rather
//! than merely a tree of the same shape.
//!
//! So every test here runs `mmm --commit` and then `mmm undo --commit` as two
//! separate processes, and the central assertion is
//! [`snapshot_tree_hashed`] equality: same paths, same bytes, byte for byte.
//! A test that asserted only the paths would pass on a round trip that put every
//! file back under the right name with the wrong contents.
//!
//! ## Layout of a round trip
//!
//! The organise runs write into a scratch `-o` directory rather than in place,
//! so "the input tree is exactly as it was" is a statement about a tree the tool
//! is not also organising into. `mmm undo <output>` reads the journal under
//! `<output>/.mmm/journal/` and moves each file back to the source path recorded
//! in it — which is inside the input tree, wherever that happens to be.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use chrono::Utc;

use common::{
    journals_in, metadata_snapshot, naive, snapshot_tree, snapshot_tree_hashed, MediaTree,
};
use mmm::reporter::{
    CONFLICTED_LABEL, DRY_RUN_BANNER, RESTORED_LABEL, SKIPPED_MODIFIED_LABEL, WILL_SKIP_PREFIX,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Organise `input` into `output`, moving files.
fn organise_commit(input: &Path, output: &Path) -> std::process::Output {
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

/// Run `mmm undo <library>` with whatever else the test needs appended.
///
/// The library is given as a positional rather than by `--journal-dir`, because
/// that is the shape an operator actually types and it exercises the agreement
/// between where the organiser wrote the journal and where undo looks for it.
fn undo(library: &Path, extra: &[&str]) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg("undo")
        .arg(library)
        .args(extra)
        .output()
        .expect("running mmm undo")
}

/// Run `mmm journal <action> …`.
fn journal(args: &[&str]) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg("journal")
        .args(args)
        .output()
        .expect("running mmm journal")
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

/// Assert the process exited non-zero — the contract a script relies on to tell
/// a partial undo from a clean one.
fn assert_failed(out: &std::process::Output, what: &str) {
    assert!(
        !out.status.success(),
        "{what} exited 0, but it did not put everything back\n--- stdout ---\n{}",
        String::from_utf8_lossy(&out.stdout),
    );
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A scratch directory whose `out` child does not yet exist.
fn scratch_output() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("creating output TempDir");
    let out = dir.path().join("out");
    (dir, out)
}

/// The run ids recorded under `root`, oldest first.
fn run_ids_in(root: &Path) -> Vec<String> {
    journals_in(root)
        .iter()
        .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect()
}

/// A tree with nested subdirectories, a file the organiser must not touch, and
/// three distinct dates so every destination is knowable.
fn nested_tree() -> MediaTree {
    MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("holiday/paris.jpg", naive(2024, 2, 20, 9, 5, 6), None)
        .jpeg_with_exif(
            "holiday/2023/rome.jpg",
            naive(2023, 3, 1, 8, 0, 0),
            Some((41.9028, 12.4964)),
        )
        .video("clips/first.mov", b"not a real mov")
        .non_media("notes.txt", b"do not touch me")
}

// ---------------------------------------------------------------------------
// The round trip
// ---------------------------------------------------------------------------

/// The claim the whole phase exists to support: a committed run can be put back
/// exactly, including the files that were several directories deep.
#[test]
fn a_committed_run_can_be_undone_back_to_the_original_bytes() {
    let tree = nested_tree();
    let (_scratch, out_dir) = scratch_output();

    let before = snapshot_tree_hashed(tree.path());
    assert!(
        before
            .iter()
            .any(|entry| entry.starts_with("holiday/2023/")),
        "the fixture must actually be nested, or this test proves less than it says: {before:?}"
    );

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");
    assert_ne!(
        snapshot_tree_hashed(tree.path()),
        before,
        "the organise run moved nothing, so undoing it would prove nothing"
    );

    let undone = undo(&out_dir, &["--commit"]);
    assert_ok(&undone, "undo --commit");

    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "the input tree is not byte-identical to how the run found it"
    );
    assert!(
        stdout_of(&undone).contains(RESTORED_LABEL),
        "the undo must report what it restored:\n{}",
        stdout_of(&undone)
    );

    // The other half of a complete reversal: the library it organised into is
    // empty again, date directories and all. `.mmm/` is excluded from the
    // snapshot and is meant to survive — the journals are the record of both
    // runs, and deleting records is not undo's job.
    assert!(
        snapshot_tree(&out_dir).is_empty(),
        "the library still holds files after the undo: {:?}",
        snapshot_tree(&out_dir)
    );
    assert_eq!(
        run_ids_in(&out_dir).len(),
        2,
        "the run and its undo are both recorded, so the undo is itself undoable"
    );
}

/// Duplicates leave by a different door — `duplicates/NNN/` rather than a date
/// directory, and a `DuplicateMoved` record rather than a `MoveCommitted` one —
/// so a round trip that only covered unique files would miss half the moves.
#[test]
fn duplicates_relocated_by_a_run_are_restored_to_their_own_paths() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("copies/beach-again.jpg", "beach.jpg")
        .jpeg_with_exif("holiday/paris.jpg", naive(2024, 2, 20, 9, 5, 6), None);

    let (_scratch, out_dir) = scratch_output();
    let before = snapshot_tree_hashed(tree.path());

    assert_ok(
        &organise_commit(tree.path(), &out_dir),
        "commit run over a tree with duplicates",
    );
    let organised = snapshot_tree(&out_dir);
    assert!(
        organised
            .iter()
            .any(|rel| rel.starts_with("duplicates/000/") && !rel.ends_with("manifest.txt")),
        "the run must have relocated a duplicate for this test to mean anything: {organised:?}"
    );

    assert_ok(&undo(&out_dir, &["--commit"]), "undo --commit");

    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "the duplicate did not go back to the path it came from"
    );
    // Deliberate, and worth pinning: `duplicates/000/` survives its own
    // manifest. The manifest records a run that really happened.
    assert_eq!(
        snapshot_tree(&out_dir),
        vec!["duplicates/000/manifest.txt".to_string()],
        "everything but the manifest should have gone home"
    );
}

// ---------------------------------------------------------------------------
// The default posture
// ---------------------------------------------------------------------------

/// Undo inherits the posture of everything else: it prints the plan and changes
/// nothing until told to. A preview that quietly moved files back would be the
/// most surprising thing in the tool.
#[test]
fn an_undo_preview_moves_nothing_and_records_nothing() {
    let tree = nested_tree();
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");

    let organised = snapshot_tree_hashed(&out_dir);
    let input = snapshot_tree_hashed(tree.path());
    let journals = metadata_snapshot(&out_dir);

    let preview = undo(&out_dir, &[]);
    assert_ok(&preview, "undo preview");
    let stdout = stdout_of(&preview);

    assert_eq!(
        snapshot_tree_hashed(&out_dir),
        organised,
        "the preview changed the organised tree"
    );
    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        input,
        "the preview put files back into the input tree"
    );
    // A preview of an undo is as read-only as a preview of a run: it does not
    // even leave a journal of its own behind.
    assert_eq!(
        metadata_snapshot(&out_dir),
        journals,
        "the preview wrote a journal for a run that never happened"
    );

    assert!(
        stdout.contains(DRY_RUN_BANNER),
        "the preview must say it changed nothing:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("{}", out_dir.join("2024-01-15").display())),
        "the preview must list where the files would come from:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Ambiguous state
// ---------------------------------------------------------------------------

/// Something else is sitting where the file came from. The file goes back
/// *beside* it, never through it, and the run says so and exits non-zero: the
/// library is not as it was.
#[test]
fn undo_restores_beside_an_occupant_of_the_original_path_and_reports_it() {
    let tree = MediaTree::new().jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None);
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");
    assert!(
        !tree.join("beach.jpg").exists(),
        "the run should have moved the file out of the input tree"
    );

    // Someone has since put a different file where the original was.
    let occupant = b"a different file entirely, written after the run";
    fs::write(tree.join("beach.jpg"), occupant).expect("writing the occupant");

    let undone = undo(&out_dir, &["--commit"]);
    assert_failed(&undone, "undo over an occupied original path");
    let stdout = stdout_of(&undone);

    assert_eq!(
        fs::read(tree.join("beach.jpg")).expect("reading the occupant back"),
        occupant,
        "the occupant was overwritten — nothing may ever be clobbered by an undo"
    );
    assert!(
        tree.join("beach-1.jpg").is_file(),
        "the restored file must land beside the occupant: {:?}",
        snapshot_tree(tree.path())
    );
    assert!(
        stdout.contains(CONFLICTED_LABEL),
        "the conflict must be reported, not merely survived:\n{stdout}"
    );
    assert!(
        stdout.contains("beach-1.jpg"),
        "the report must name where the file actually went:\n{stdout}"
    );
}

/// The file at the recorded destination is no longer the file the run put
/// there, so undo leaves it exactly where it is and says which one it refused.
#[test]
fn undo_skips_a_file_that_changed_after_the_run_and_reports_it() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("holiday/paris.jpg", naive(2024, 2, 20, 9, 5, 6), None);
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");

    let organised = out_dir.join("2024-01-15/2024-01-15-143000.jpg");
    assert!(
        organised.is_file(),
        "expected the organised file at {}: {:?}",
        organised.display(),
        snapshot_tree(&out_dir)
    );
    let mut edited = fs::read(&organised).expect("reading the organised file");
    edited.extend_from_slice(b"appended after the run, so it is a different file now");
    fs::write(&organised, &edited).expect("editing the organised file");

    // A preview taken now must already say the file will be refused. A preview
    // that listed a file the commit then declines to move would be a preview of
    // a different run.
    let preview = undo(&out_dir, &[]);
    assert_ok(&preview, "undo preview over a modified library");
    assert!(
        stdout_of(&preview).contains(WILL_SKIP_PREFIX),
        "the preview must flag the file the commit will refuse:\n{}",
        stdout_of(&preview)
    );

    let undone = undo(&out_dir, &["--commit"]);
    assert_failed(&undone, "undo over a modified library");
    let stdout = stdout_of(&undone);

    assert!(
        stdout.contains(SKIPPED_MODIFIED_LABEL),
        "the skip must be reported:\n{stdout}"
    );
    assert_eq!(
        fs::read(&organised).expect("reading the modified file back"),
        edited,
        "the modified file was moved despite being unrecognisable"
    );
    assert!(
        !tree.join("beach.jpg").exists(),
        "a skipped file must not be restored"
    );
    // The rest of the run is unaffected — one ambiguous file does not abandon
    // the files that are exactly as the journal describes them.
    assert!(
        tree.join("holiday/paris.jpg").is_file(),
        "the untouched file should still have been restored: {:?}",
        snapshot_tree(tree.path())
    );
}

// ---------------------------------------------------------------------------
// Reading the journals back
// ---------------------------------------------------------------------------

/// The second-resolution part of a run id, which is what makes the listing
/// chronological.
fn stamp_of(run_id: &str) -> String {
    run_id
        .rsplit_once('-')
        .map_or_else(|| run_id.to_string(), |(stamp, _)| stamp.to_string())
}

/// Block until the wall clock reads a later second than `stamp`.
///
/// Run ids carry seconds, and `mmm journal list` sorts by run id — so two runs
/// inside the same second are ordered by their random suffix, which is no order
/// at all. Rather than assert a claim that would be true most of the time, the
/// test makes its own premise true first and then checks that it did.
fn wait_past_second(stamp: &str) {
    for _ in 0..120 {
        if Utc::now().format("%Y%m%d-%H%M%S").to_string().as_str() > stamp {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the clock did not advance past {stamp}");
}

/// `journal list` shows every recorded run newest first, and `journal show`
/// renders a chosen one in full.
#[test]
fn journal_list_is_newest_first_and_journal_show_renders_a_run() {
    let tree = nested_tree();
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");
    let organise_id = run_ids_in(&out_dir)
        .pop()
        .expect("the commit run must have written a journal");

    // The undo below writes the second journal; it has to land in a later
    // second for "newest first" to be a statement about time.
    wait_past_second(&stamp_of(&organise_id));
    assert_ok(&undo(&out_dir, &["--commit"]), "undo --commit");

    let ids = run_ids_in(&out_dir);
    assert_eq!(ids.len(), 2, "expected two recorded runs: {ids:?}");
    let (older, newer) = (&ids[0], &ids[1]);
    assert_eq!(
        older, &organise_id,
        "the organise run should be the older of the two: {ids:?}"
    );
    assert_ne!(
        stamp_of(older),
        stamp_of(newer),
        "the two runs landed in the same second, so this test cannot say anything about order"
    );

    let listing = journal(&["list", &out_dir.display().to_string()]);
    assert_ok(&listing, "journal list");
    let listed = stdout_of(&listing);

    let newer_at = listed
        .find(newer.as_str())
        .unwrap_or_else(|| panic!("the undo run is missing from the listing:\n{listed}"));
    let older_at = listed
        .find(older.as_str())
        .unwrap_or_else(|| panic!("the organise run is missing from the listing:\n{listed}"));
    assert!(
        newer_at < older_at,
        "the listing is not newest-first:\n{listed}"
    );

    let shown = journal(&["show", older, &out_dir.display().to_string()]);
    assert_ok(&shown, "journal show");
    let detail = stdout_of(&shown);

    assert!(
        detail.contains(older.as_str()),
        "the detail must name the run it is showing:\n{detail}"
    );
    assert!(
        detail.contains(&out_dir.display().to_string()),
        "the detail must name the library the run organised into:\n{detail}"
    );
    // Both ends of every move, so the operator can see what would be put back
    // where without reading the JSONL themselves.
    assert!(
        detail.contains(&tree.join("holiday/2023/rome.jpg").display().to_string()),
        "the detail must name each file's original path:\n{detail}"
    );
    assert!(
        detail.contains("2023-03-01"),
        "the detail must name where each file went:\n{detail}"
    );

    // A run that does not exist is refused rather than reported as empty.
    let missing = journal(&[
        "show",
        "20240101-000000-zzzzzz",
        &out_dir.display().to_string(),
    ]);
    assert_failed(&missing, "journal show for a run that was never recorded");
}
