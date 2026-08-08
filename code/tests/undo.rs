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
use mmm::journal::{Journal, JournalEntry, TRUNCATED_TAIL_NOTICE};
use mmm::reporter::{
    CONFLICTED_LABEL, DRY_RUN_BANNER, POSSIBLY_MOVED_HEADING, POSSIBLY_MOVED_LABEL, RESTORED_LABEL,
    SKIPPED_MODIFIED_LABEL, WILL_SKIP_PREFIX,
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

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
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

/// The size check cannot see an edit that keeps the length, so a file the
/// cascade fully hashed must be journalled with that digest — otherwise undo
/// happily moves back something whose contents are no longer what it recorded.
///
/// The fixture has a duplicate pair deliberately: that is what drives the
/// retained original through phase 3, which is the only place an organise move
/// acquires a digest without paying for one on purpose.
#[test]
fn undo_refuses_a_same_size_edit_to_a_file_the_cascade_hashed() {
    let tree = MediaTree::new()
        .jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("holiday/beach-copy.jpg", "beach.jpg");
    let (_scratch, out_dir) = scratch_output();

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");

    let organised = out_dir.join("2024-01-15/2024-01-15-143000.jpg");
    assert!(
        organised.is_file(),
        "expected the retained original at {}: {:?}",
        organised.display(),
        snapshot_tree(&out_dir)
    );

    // Same length, different bytes — invisible to a size check, and the whole
    // reason the digest has to be recorded.
    let original = fs::read(&organised).expect("reading the organised file");
    let mut edited = original.clone();
    let last = edited.len() - 1;
    edited[last] ^= 0xFF;
    assert_eq!(
        edited.len(),
        original.len(),
        "the edit must not change size"
    );
    fs::write(&organised, &edited).expect("editing the organised file");

    let undone = undo(&out_dir, &["--commit"]);
    assert_failed(&undone, "undo over a same-size edit");
    let stdout = stdout_of(&undone);

    assert!(
        stdout.contains(SKIPPED_MODIFIED_LABEL),
        "the same-size edit must be refused, not silently restored:\n{stdout}"
    );
    // Still where the run left it, with the edit intact. Which of the pair was
    // retained as the original is not asserted: the group's membership order is
    // not a promise, so the other file may legitimately have been restored from
    // `duplicates/` — what matters is that *this* one was refused.
    assert!(
        organised.is_file(),
        "the refused file must stay where the run put it"
    );
    assert_eq!(
        fs::read(&organised).expect("reading the edited file back"),
        edited,
        "the edited file was moved despite its contents having changed"
    );
}

// ---------------------------------------------------------------------------
// The interrupted run
// ---------------------------------------------------------------------------

/// What cutting a journal short left behind.
struct Interrupted {
    /// The move whose outcome went with the partial line.
    seq: u64,
    /// Where that file was before the run — where an undo would put it back, if
    /// anything said it had moved.
    source: PathBuf,
    /// Where it actually is, which only the lost line recorded.
    destination: PathBuf,
    /// Complete entries still in the journal afterwards.
    kept: usize,
}

/// Cut `path` in the middle of the line recording its last committed move.
///
/// This is the closest a test can get to pulling the power mid-run: the journal
/// keeps every complete entry, its final line is half-written, and the move that
/// line described is left with an intent and no outcome. Note what the file
/// system does *not* do — the file itself really did move, because the real run
/// really did move it. That asymmetry is the whole point: the disk and the
/// journal disagree, and undo has to cope with the journal it has.
fn truncate_at_last_commit(path: &Path) -> Interrupted {
    let (_, entries) = Journal::read(path).expect("the journal the run wrote must be readable");

    let (index, seq, destination) = entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| match entry {
            JournalEntry::MoveCommitted {
                seq,
                final_destination,
                ..
            } => Some((index, *seq, final_destination.clone())),
            _ => None,
        })
        .expect("the run must have committed a move for there to be one to interrupt");

    let (source, planned) = entries
        .iter()
        .find_map(|entry| match entry {
            JournalEntry::MoveIntent {
                seq: at,
                source,
                destination,
                ..
            } if *at == seq => Some((source.clone(), destination.clone())),
            _ => None,
        })
        .expect("a commit is always preceded by its own intent");
    // The reports below name the *planned* destination, since that is all a
    // lost commit line leaves behind, while the file is at the final one. This
    // fixture has no collisions so the two agree — if that ever changes, the
    // assertions have to start telling them apart.
    assert_eq!(
        planned, destination,
        "this fixture must not involve collision resolution"
    );

    let text = fs::read_to_string(path).expect("reading the journal to cut it");
    let lines: Vec<&str> = text.lines().collect();
    // Entry `index` is line `index + 1`: the header takes the first line.
    let cut_line = lines[index + 1];
    assert!(
        cut_line.contains("move_committed"),
        "expected to be cutting a commit record, got: {cut_line}"
    );

    // Bytes rather than a string slice: half a line can land inside a
    // multi-byte character, which is exactly one of the shapes a real
    // interruption takes and must not be a panic in the harness.
    let mut truncated = lines[..=index].join("\n").into_bytes();
    truncated.push(b'\n');
    truncated.extend_from_slice(&cut_line.as_bytes()[..cut_line.len() / 2]);
    fs::write(path, &truncated).expect("writing the truncated journal");

    Interrupted {
        seq,
        source,
        destination,
        kept: index,
    }
}

/// `path` relative to `root`, in the form the snapshot helpers use.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or_else(|_| panic!("{} is not under {}", path.display(), root.display()))
        .to_string_lossy()
        .into_owned()
}

/// The digest a `snapshot_tree_hashed` entry recorded for `rel`.
fn hash_of(snapshot: &[String], rel: &str) -> String {
    let prefix = format!("{rel}  ");
    snapshot
        .iter()
        .find_map(|entry| entry.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("{rel} is missing from the snapshot: {snapshot:?}"))
        .to_string()
}

/// The crash-resilience claim, end to end: a journal cut off mid-line still
/// describes every move it completed, `undo` reverses exactly those, and the one
/// move whose outcome was lost is handed to the operator rather than guessed at
/// in either direction.
#[test]
fn an_interrupted_run_restores_what_it_recorded_and_reports_what_it_could_not() {
    let tree = nested_tree();
    let (_scratch, out_dir) = scratch_output();
    let before = snapshot_tree_hashed(tree.path());

    assert_ok(&organise_commit(tree.path(), &out_dir), "commit run");

    let journal_path = journals_in(&out_dir)
        .pop()
        .expect("the commit run must have written a journal");
    let cut = truncate_at_last_commit(&journal_path);

    // --- The journal survives being cut ---------------------------------
    let (header, entries) =
        Journal::read(&journal_path).expect("a truncated journal is still a journal");
    assert_eq!(
        entries.len(),
        cut.kept,
        "every complete entry must survive, and only the half-written one be dropped"
    );
    // "Exactly the moves it recorded" says little about a run that recorded
    // one. The fixture has to leave several complete moves on the near side of
    // the cut for the assertions below to mean anything.
    assert!(
        cut.kept >= 3,
        "too few entries survived the cut for this test to prove much: {}",
        cut.kept
    );
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, JournalEntry::RunCompleted { .. })),
        "a run cut off before its closing line did not finish"
    );
    assert!(
        matches!(entries.last(), Some(JournalEntry::MoveIntent { seq, .. }) if *seq == cut.seq),
        "the surviving tail must be the intent whose outcome went with the cut line: {:?}",
        entries.last()
    );
    assert_eq!(
        header.output_dir, out_dir,
        "the header must still name the library it organised into"
    );

    // --- The preview says what the commit will do -------------------------
    let preview = undo(&out_dir, &[]);
    // Zero: a preview that moves nothing cannot leave the library in a state
    // worth failing over. The refusal belongs to the run that acts.
    assert_ok(&preview, "undo preview of an interrupted run");
    let previewed = stdout_of(&preview);
    assert!(
        previewed.contains(POSSIBLY_MOVED_HEADING)
            && previewed.contains(&cut.source.display().to_string()),
        "the preview must already name the file the commit will not touch:\n{previewed}"
    );

    // --- The undo ---------------------------------------------------------
    let undone = undo(&out_dir, &["--commit"]);
    assert_failed(&undone, "undo of an interrupted run");
    let stdout = stdout_of(&undone);
    let both = format!("{stdout}{}", String::from_utf8_lossy(&undone.stderr));

    assert!(
        both.contains(TRUNCATED_TAIL_NOTICE),
        "the discarded line must be warned about, not silently dropped:\n{both}"
    );
    assert!(
        stdout.contains(POSSIBLY_MOVED_HEADING),
        "an intent with no outcome must be reported as possibly moved:\n{stdout}"
    );
    assert!(
        stdout.contains(&cut.source.display().to_string())
            && stdout.contains(&cut.destination.display().to_string()),
        "the report must name both places the file could be, since it is one of the two:\n{stdout}"
    );
    assert!(
        stdout.contains(POSSIBLY_MOVED_LABEL),
        "the count belongs in the closing table too — the list above it can be long:\n{stdout}"
    );

    // --- Exactly the recorded moves came back -----------------------------
    let cut_rel = relative(tree.path(), &cut.source);
    let expected: Vec<String> = before
        .iter()
        .filter(|entry| !entry.starts_with(&format!("{cut_rel}  ")))
        .cloned()
        .collect();
    assert_eq!(
        expected.len(),
        before.len() - 1,
        "the fixture must have lost exactly one file to the interruption"
    );
    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        expected,
        "the input tree is not the original minus the one move nothing recorded"
    );

    // And the unrecorded one is untouched where the run left it: not restored,
    // and not damaged by the attempt either.
    assert_eq!(
        snapshot_tree_hashed(&out_dir),
        vec![format!(
            "{}  {}",
            relative(&out_dir, &cut.destination),
            hash_of(&before, &cut_rel)
        )],
        "the library should hold precisely the file whose move was never recorded"
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

// ---------------------------------------------------------------------------
// Resolving which run to undo
//
// The three ways `mmm undo` can fail to find a run are all in `main`, and all
// three are the first thing an operator meets when they reach for undo under
// pressure. A wrong message here sends somebody looking for a journal that was
// never written, or worse, convinces them a run they did make was not recorded.
// ---------------------------------------------------------------------------

/// `--run` naming an id that was never recorded is refused, and the refusal
/// says where to look for the ones that were.
#[test]
fn undo_of_a_run_that_was_never_recorded_is_refused() {
    let tree = nested_tree();
    let (_out, out_dir) = scratch_output();
    assert_ok(&organise_commit(tree.path(), &out_dir), "organise --commit");

    let out = undo(&out_dir, &["--run", "20240101-000000-zzzzzz", "--commit"]);
    assert_failed(&out, "undo of an unrecorded run");

    let text = stderr_of(&out);
    assert!(
        text.contains("20240101-000000-zzzzzz") && text.contains("mmm journal list"),
        "the refusal must name the run and point at the listing:\n{text}"
    );
}

/// `--run` naming a run that *was* recorded reverses exactly that run, which is
/// how an operator undoes something other than the most recent thing they did.
#[test]
fn undo_of_a_named_run_reverses_that_run() {
    let tree = nested_tree();
    let (_out, out_dir) = scratch_output();

    let before = snapshot_tree_hashed(tree.path());
    assert_ok(&organise_commit(tree.path(), &out_dir), "organise --commit");

    let ids = run_ids_in(&out_dir);
    assert_eq!(ids.len(), 1, "one run, one journal: {ids:?}");

    let out = undo(&out_dir, &["--run", &ids[0], "--commit"]);
    assert_ok(&out, "undo --run of a recorded run");

    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "naming the run explicitly must reverse it as completely as --last does"
    );
}

/// And a library nobody has ever organised into says so, rather than reporting
/// an io error about a directory that was never meant to exist yet.
#[test]
fn undo_in_a_library_with_no_runs_says_there_is_nothing_to_undo() {
    let (_out, out_dir) = scratch_output();
    fs::create_dir_all(&out_dir).unwrap();

    let out = undo(&out_dir, &["--commit"]);
    assert_failed(&out, "undo in a library with no runs");

    let text = stderr_of(&out);
    assert!(
        text.contains("nothing to undo") && text.contains(&out_dir.display().to_string()),
        "the message must name the library it looked in:\n{text}"
    );
}

/// A journal with nothing to reverse is reversed by doing nothing — and
/// crucially without writing an undo journal of its own, or a library would
/// accumulate a growing trail of empty undos every time somebody
/// double-checked it.
///
/// The journal is written by hand rather than produced by a run, because a run
/// that records a header and then moves nothing is exactly what an interruption
/// between the two leaves behind, and there is no way to ask the binary for one.
#[test]
fn undo_of_a_journal_with_no_moves_records_no_undo() {
    let (_out, out_dir) = scratch_output();
    let journal_dir = out_dir.join(".mmm/journal");
    let run_id = "20240315-103000-aaaaaa";
    drop(
        Journal::create(
            &journal_dir,
            &mmm::journal::RunHeader::new(run_id, &out_dir, vec!["mmm".to_string()]),
        )
        .expect("writing a header-only journal"),
    );

    let before = run_ids_in(&out_dir);
    assert_eq!(before, vec![run_id.to_string()], "got: {before:?}");

    let out = undo(&out_dir, &["--commit"]);
    assert_ok(&out, "undo of a journal with no moves");

    assert_eq!(
        run_ids_in(&out_dir),
        before,
        "an undo that reverses nothing must not add a journal of its own"
    );
}
