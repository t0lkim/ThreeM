//! Reversing a run: reading a journal back and putting every file it moved
//! where it came from.
//!
//! The journal ([`crate::journal`]) is written so that this module can exist —
//! every move's intent is on the disk before the move, and every outcome names
//! the path the file actually reached. Undo is therefore not a guess about what
//! a run probably did; it is a replay of what the run said it was doing, in
//! reverse.
//!
//! Three rules shape everything below:
//!
//! * **Reverse order, always.** A run can move `a.jpg` to `b.jpg` and then move
//!   something else to `a.jpg`. Undoing forwards would put the first file back
//!   on top of the second; undoing backwards frees each path before anything
//!   needs it.
//! * **The restore is itself a move, so it goes through the same recorder.**
//!   [`crate::organiser::recorded_move`] writes the intent before it acts, which
//!   makes an interrupted undo exactly as recoverable as an interrupted run —
//!   and makes an undo undoable.
//! * **Nothing is overwritten.** The restore uses the same no-clobber move as
//!   the organiser, so a source path that is occupied again yields a
//!   collision-suffixed name rather than destroying whatever is sitting there.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tracing::{debug, error, warn};

use crate::journal::{self, IntentKind, Journal, JournalEntry, RunHeader};
use crate::metadata::DateSource;
use crate::organiser::{
    recorded_move, MoveKind, MovePurpose, MoveRecorder, PlannedMove, RecordedMoveError,
};
use crate::METADATA_DIR_NAME;

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// One file to put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreStep {
    /// The sequence number of the move being reversed, so a step can always be
    /// traced to the journal line that caused it.
    pub seq: u64,
    /// Where the run left the file.
    pub current: PathBuf,
    /// Where the run found it, and where it is going back to.
    pub original: PathBuf,
    /// The size the file had immediately before it moved, when the journal
    /// recorded one. Evidence for the verification pass.
    pub source_size: Option<u64>,
    /// The digest the run recorded, when it had one — duplicates always, unique
    /// files never.
    pub source_hash: Option<String>,
    /// Why the file was moved in the first place.
    pub kind: IntentKind,
}

/// Everything a run's journal says about putting that run back.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    /// The journal this plan was read from.
    pub journal: PathBuf,
    pub header: RunHeader,
    /// The moves to reverse, already in the order they must be performed.
    pub steps: Vec<RestoreStep>,
    /// The run never wrote its closing line, so it was interrupted rather than
    /// finished. The steps below are still every move it managed to record.
    pub interrupted: bool,
}

/// What the intent line said, for the commit line that will refer back to it.
struct Intent {
    source: PathBuf,
    size: u64,
    hash: Option<String>,
    kind: IntentKind,
}

/// Read `journal_path` and work out how to undo the run it describes.
///
/// # Errors
///
/// Returns an error if the journal cannot be read — see [`Journal::read`],
/// which tolerates an interrupted final line but not a corrupt middle one.
pub fn plan_restore(journal_path: &Path) -> Result<RestorePlan> {
    let (header, entries) = Journal::read(journal_path)?;
    Ok(build_plan(journal_path.to_path_buf(), header, &entries))
}

/// The pure half of [`plan_restore`]: journal contents in, plan out.
///
/// Split out so the ordering rule — the property that actually matters — is
/// testable without a filesystem.
pub fn build_plan(journal: PathBuf, header: RunHeader, entries: &[JournalEntry]) -> RestorePlan {
    let mut intents: HashMap<u64, Intent> = HashMap::new();
    let mut interrupted = true;

    for entry in entries {
        match entry {
            JournalEntry::MoveIntent {
                seq,
                source,
                source_size,
                source_hash,
                kind,
                ..
            } => {
                intents.insert(
                    *seq,
                    Intent {
                        source: source.clone(),
                        size: *source_size,
                        hash: source_hash.clone(),
                        kind: *kind,
                    },
                );
            }
            JournalEntry::RunCompleted { .. } => interrupted = false,
            _ => {}
        }
    }

    // Reverse: a run that moved `a` to `b` and then something else onto `a`
    // must be undone from the far end, or the second move's source path is
    // still occupied when the first needs it back.
    let mut steps = Vec::new();
    for entry in entries.iter().rev() {
        let (seq, current, recorded_source, fallback_kind) = match entry {
            JournalEntry::MoveCommitted {
                seq,
                final_destination,
                ..
            } => (*seq, final_destination.clone(), None, IntentKind::Organise),
            // A duplicate's commit record is `DuplicateMoved`, not
            // `MoveCommitted` — one commit line per move, of the type that
            // fits. Undo has to treat both as commits or every relocated
            // duplicate stays in `duplicates/NNN/` forever.
            JournalEntry::DuplicateMoved {
                seq,
                source,
                destination,
                ..
            } => (
                *seq,
                destination.clone(),
                Some(source.clone()),
                IntentKind::Duplicate,
            ),
            _ => continue,
        };

        let intent = intents.get(&seq);
        // `DuplicateMoved` carries its own source, so a journal whose intent
        // line was lost can still restore it. `MoveCommitted` does not, and
        // without the intent there is nowhere to put the file back to — saying
        // so is the only honest option.
        let Some(original) = intent.map(|i| i.source.clone()).or(recorded_source) else {
            warn!(
                seq,
                current = %current.display(),
                "this move has a commit record but no intent, so nothing on disk says where \
                 the file came from; it cannot be restored"
            );
            continue;
        };

        steps.push(RestoreStep {
            seq,
            current,
            original,
            source_size: intent.map(|i| i.size),
            source_hash: intent.and_then(|i| i.hash.clone()),
            kind: intent.map_or(fallback_kind, |i| i.kind),
        });
    }

    RestorePlan {
        journal,
        header,
        steps,
        interrupted,
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// What became of one [`RestoreStep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Put back. `at` is where it actually landed, which is not always
    /// `original`: the restore refuses to overwrite, so an occupied source path
    /// yields a collision-suffixed name instead.
    Restored { at: PathBuf, kind: MoveKind },
    /// Moved back, but the journal line recording it could not be written, so
    /// this undo is not itself fully reversible. The run stops here.
    RestoredUnrecorded,
    /// The move back did not happen. The file is still where the run left it.
    Failed { reason: String },
}

/// What an undo run did.
///
/// Every step is accounted for exactly once:
/// `restored + failed + unprocessed == plan.steps.len()`.
#[derive(Debug, Default, Clone)]
pub struct RestoreRun {
    /// One entry per attempted step, indexed into [`RestorePlan::steps`].
    pub outcomes: Vec<(usize, RestoreOutcome)>,
    pub restored: usize,
    pub failed: usize,
    /// Steps never attempted, because the run stopped before reaching them.
    pub unprocessed: usize,
    /// The undo stopped because it could not record itself.
    pub journal_failed: bool,
    /// Directories the restore emptied and then removed.
    pub pruned_dirs: usize,
}

/// Put every file in `plan` back, recording the restore as it goes.
///
/// Infallible by signature for the same reason [`crate::organiser::process_moves`]
/// is: one file that cannot be put back is not a reason to abandon the rest, and
/// the caller owes the operator a complete account either way. The one condition
/// that does stop the run is a journal that cannot be written — every restore
/// after it would be one nothing could reverse.
pub fn execute_restore(plan: &RestorePlan, recorder: &mut MoveRecorder<'_>) -> RestoreRun {
    let mut run = RestoreRun::default();
    let mut vacated: Vec<PathBuf> = Vec::new();

    for (index, step) in plan.steps.iter().enumerate() {
        let planned = PlannedMove {
            source: step.current.clone(),
            destination: step.original.clone(),
            date_source: DateSource::None,
            has_location: false,
        };
        let purpose = MovePurpose::Restore {
            hash: step.source_hash.as_deref(),
        };

        let outcome = match recorded_move(recorder, &planned, purpose) {
            Ok(outcome) => {
                debug!(
                    seq = step.seq,
                    from = %step.current.display(),
                    to = %outcome.destination.display(),
                    "restored"
                );
                run.restored += 1;
                note_vacated(&mut vacated, &step.current);
                RestoreOutcome::Restored {
                    at: outcome.destination,
                    kind: outcome.kind,
                }
            }
            Err(RecordedMoveError::Move(e)) => {
                let reason = format!("{e:#}");
                error!(
                    seq = step.seq,
                    from = %step.current.display(),
                    to = %step.original.display(),
                    error = %reason,
                    "restore failed"
                );
                run.failed += 1;
                RestoreOutcome::Failed { reason }
            }
            // `moved` distinguishes an intent that could not be written — which
            // stops the move before it happens — from an outcome that could
            // not be, which does not un-move the file.
            Err(RecordedMoveError::Journal { error, moved }) => {
                error!(
                    seq = step.seq,
                    moved,
                    error = %format!("{error:#}"),
                    "the undo journal could not be written; stopping so that no further restore \
                     goes unrecorded"
                );
                run.journal_failed = true;
                if moved {
                    run.restored += 1;
                    note_vacated(&mut vacated, &step.current);
                    RestoreOutcome::RestoredUnrecorded
                } else {
                    run.failed += 1;
                    RestoreOutcome::Failed {
                        reason: format!("{error:#}"),
                    }
                }
            }
        };

        run.outcomes.push((index, outcome));
        if run.journal_failed {
            break;
        }
    }

    run.unprocessed = plan.steps.len() - run.outcomes.len();
    run.pruned_dirs = prune_empty_dirs(&vacated, &plan.header.output_dir);
    run
}

/// Remember the directory a restored file just left.
fn note_vacated(vacated: &mut Vec<PathBuf>, moved_from: &Path) {
    if let Some(parent) = moved_from.parent() {
        vacated.push(parent.to_path_buf());
    }
}

/// Remove the directories the restore emptied, and nothing else.
///
/// Walks upward from each vacated directory towards `boundary` — the output
/// tree the run organised into — and stops the moment a directory will not go.
/// `boundary` itself is never removed: the operator asked for a library there,
/// and emptying it is not the same as deleting it.
///
/// The guarantee that a directory holding files `mmm` did not create survives is
/// `fs::remove_dir` itself, not a heuristic about which files those might be: it
/// refuses any directory with anything at all in it. That is deliberately
/// conservative in one visible way — a duplicate group's `manifest.txt` keeps
/// `duplicates/NNN/` alive after its files have gone home. The manifest is the
/// record of a run that really happened, and deleting records is not undo's job.
///
/// Returns the number of directories removed.
pub fn prune_empty_dirs(vacated: &[PathBuf], boundary: &Path) -> usize {
    let mut removed = 0;

    for start in vacated {
        let mut cursor: &Path = start;

        loop {
            // Never climb out of the tree the run wrote into, never remove the
            // tree itself, and never touch the metadata directory — the journal
            // being written by this very undo lives in it.
            if cursor == boundary || !cursor.starts_with(boundary) {
                break;
            }
            if cursor
                .components()
                .any(|c| c.as_os_str() == METADATA_DIR_NAME)
            {
                break;
            }

            match fs::remove_dir(cursor) {
                Ok(()) => {
                    debug!(dir = %cursor.display(), "removed a directory the restore emptied");
                    removed += 1;
                }
                // Already gone, because two restored files shared this parent.
                // Its own parent may now be empty, so keep climbing.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // Not empty, or not ours to remove. Either way its ancestors
                // are not empty either.
                Err(_) => break,
            }

            let Some(parent) = cursor.parent() else { break };
            cursor = parent;
        }
    }

    removed
}

// ---------------------------------------------------------------------------
// Reading journals back for the operator
// ---------------------------------------------------------------------------

/// The closing line of a run that finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub moved: usize,
    pub failed: usize,
    pub skipped: usize,
    pub ended_at: DateTime<Utc>,
}

/// What one run's journal says about itself.
#[derive(Debug, Clone)]
pub struct RunDetail {
    pub started_at: DateTime<Utc>,
    pub mmm_version: String,
    pub output_dir: PathBuf,
    pub argv: Vec<String>,
    /// Moves this run committed — what an undo of it would put back.
    pub restorable: usize,
    /// `None` when the run never wrote its closing line: it was interrupted.
    pub completion: Option<Completion>,
}

/// One row of `mmm journal list`.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: String,
    pub path: PathBuf,
    /// `Err` when the journal exists but will not parse. Reported rather than
    /// propagated: one unreadable journal must not hide the ten good ones next
    /// to it, which is the whole reason a listing is useful after a bad run.
    pub detail: Result<RunDetail, String>,
}

/// Summarise every run recorded in `dir`, newest first.
///
/// # Errors
///
/// Returns an error only if the directory itself cannot be read. Individual
/// unreadable journals become `Err` rows.
pub fn summarise_runs(dir: &Path) -> Result<Vec<RunRow>> {
    let paths = journal::journals_newest_first(dir)?;

    Ok(paths
        .into_iter()
        .map(|path| {
            let run_id = journal::run_id_of(&path).unwrap_or_else(|| path.display().to_string());
            let detail = Journal::read(&path)
                .map(|(header, entries)| detail_of(&header, &entries))
                .map_err(|e| format!("{e:#}"));
            RunRow {
                run_id,
                path,
                detail,
            }
        })
        .collect())
}

/// Read one run in full, for `mmm journal show`.
///
/// # Errors
///
/// Returns an error if the journal cannot be read.
pub fn read_run(path: &Path) -> Result<(RunHeader, Vec<JournalEntry>)> {
    Journal::read(path).with_context(|| format!("reading the journal {}", path.display()))
}

/// Fold a journal's contents into the summary a listing shows.
pub fn detail_of(header: &RunHeader, entries: &[JournalEntry]) -> RunDetail {
    let restorable = entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                JournalEntry::MoveCommitted { .. } | JournalEntry::DuplicateMoved { .. }
            )
        })
        .count();

    let completion = entries.iter().find_map(|e| match e {
        JournalEntry::RunCompleted {
            moved,
            failed,
            skipped,
            ended_at,
        } => Some(Completion {
            moved: *moved,
            failed: *failed,
            skipped: *skipped,
            ended_at: *ended_at,
        }),
        _ => None,
    });

    RunDetail {
        started_at: header.started_at,
        mmm_version: header.mmm_version.clone(),
        output_dir: header.output_dir.clone(),
        argv: header.argv.clone(),
        restorable,
        completion,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn header() -> RunHeader {
        RunHeader::new(
            "20240315-103000-abc123",
            "/photos",
            vec!["mmm".to_string(), "/photos".to_string()],
        )
    }

    fn intent(seq: u64, source: &str, destination: &str, kind: IntentKind) -> JournalEntry {
        JournalEntry::MoveIntent {
            seq,
            source: PathBuf::from(source),
            destination: PathBuf::from(destination),
            source_size: 1234,
            source_hash: None,
            kind,
        }
    }

    fn committed(seq: u64, at: &str) -> JournalEntry {
        JournalEntry::MoveCommitted {
            seq,
            final_destination: PathBuf::from(at),
            move_kind: MoveKind::Renamed,
        }
    }

    fn plan_of(entries: &[JournalEntry]) -> RestorePlan {
        build_plan(
            PathBuf::from("/photos/.mmm/journal/x.jsonl"),
            header(),
            entries,
        )
    }

    // -----------------------------------------------------------------
    // Planning
    // -----------------------------------------------------------------

    #[test]
    fn a_committed_move_becomes_a_step_back_to_its_source() {
        let plan = plan_of(&[
            intent(
                0,
                "/photos/IMG_1.JPG",
                "/out/2024-03-15/a.jpg",
                IntentKind::Organise,
            ),
            committed(0, "/out/2024-03-15/a.jpg"),
        ]);

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(
            plan.steps[0].current,
            PathBuf::from("/out/2024-03-15/a.jpg")
        );
        assert_eq!(plan.steps[0].original, PathBuf::from("/photos/IMG_1.JPG"));
        assert_eq!(plan.steps[0].kind, IntentKind::Organise);
    }

    /// The commit line, not the intent line, says where the file actually is:
    /// collision resolution can land it at `a-1.jpg`. Restoring from the
    /// planned destination would move the wrong file, or none at all.
    #[test]
    fn a_step_starts_from_where_the_file_landed_not_where_it_was_planned() {
        let plan = plan_of(&[
            intent(
                0,
                "/photos/IMG_1.JPG",
                "/out/2024-03-15/a.jpg",
                IntentKind::Organise,
            ),
            committed(0, "/out/2024-03-15/a-1.jpg"),
        ]);

        assert_eq!(
            plan.steps[0].current,
            PathBuf::from("/out/2024-03-15/a-1.jpg")
        );
    }

    /// The ordering rule. A run that vacates a path and then fills it must be
    /// undone from the far end, or the first file goes back on top of the
    /// second.
    #[test]
    fn steps_are_in_reverse_order_of_the_moves_that_made_them() {
        let plan = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            committed(0, "/out/x.jpg"),
            intent(1, "/photos/b.jpg", "/photos/a.jpg", IntentKind::Organise),
            committed(1, "/photos/a.jpg"),
        ]);

        let seqs: Vec<u64> = plan.steps.iter().map(|s| s.seq).collect();
        assert_eq!(seqs, vec![1, 0], "the later move is undone first");
    }

    /// `DuplicateMoved` is a commit record too. Missing it would strand every
    /// relocated duplicate in `duplicates/NNN/`.
    #[test]
    fn a_relocated_duplicate_is_restored_like_any_other_move() {
        let plan = plan_of(&[
            intent(
                0,
                "/photos/copy.jpg",
                "/out/duplicates/000/copy.jpg",
                IntentKind::Duplicate,
            ),
            JournalEntry::DuplicateMoved {
                seq: 0,
                group: 0,
                source: PathBuf::from("/photos/copy.jpg"),
                destination: PathBuf::from("/out/duplicates/000/copy.jpg"),
            },
        ]);

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].original, PathBuf::from("/photos/copy.jpg"));
        assert_eq!(plan.steps[0].kind, IntentKind::Duplicate);
    }

    /// A duplicate's record carries its own source, so it survives losing its
    /// intent line — which is what a journal truncated in the middle of a run
    /// looks like from the far side.
    #[test]
    fn a_duplicate_can_be_restored_from_its_own_record_alone() {
        let plan = plan_of(&[JournalEntry::DuplicateMoved {
            seq: 7,
            group: 1,
            source: PathBuf::from("/photos/copy.jpg"),
            destination: PathBuf::from("/out/duplicates/001/copy.jpg"),
        }]);

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].original, PathBuf::from("/photos/copy.jpg"));
    }

    /// Nothing on disk says where this file came from, so there is nothing to
    /// do but say so. Inventing a destination is how an undo loses a photo.
    #[test]
    fn a_commit_with_no_intent_and_no_source_is_not_restorable() {
        let plan = plan_of(&[committed(3, "/out/2024-03-15/a.jpg")]);
        assert!(plan.steps.is_empty());
    }

    /// A move that failed never happened, so there is nothing to reverse.
    #[test]
    fn a_failed_move_produces_no_step() {
        let plan = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            JournalEntry::MoveFailed {
                seq: 0,
                reason: "disk full".to_string(),
            },
        ]);
        assert!(plan.steps.is_empty());
    }

    /// An intent with no outcome at all is the interrupted-mid-rename case. It
    /// is not restorable — nothing says the file moved — and it must not be
    /// silently treated as one that did.
    #[test]
    fn an_intent_with_no_outcome_produces_no_step() {
        let plan = plan_of(&[intent(
            0,
            "/photos/a.jpg",
            "/out/x.jpg",
            IntentKind::Organise,
        )]);
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn a_run_without_its_closing_line_is_reported_as_interrupted() {
        let unfinished = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            committed(0, "/out/x.jpg"),
        ]);
        assert!(unfinished.interrupted);

        let finished = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            committed(0, "/out/x.jpg"),
            JournalEntry::RunCompleted {
                moved: 1,
                failed: 0,
                skipped: 0,
                ended_at: Utc::now(),
            },
        ]);
        assert!(!finished.interrupted);
    }

    // -----------------------------------------------------------------
    // Execution
    // -----------------------------------------------------------------

    /// Build a two-file organised tree by hand and undo it. Real files, real
    /// moves — the restore's job is filesystem work, and asserting on anything
    /// less would not establish that it does it.
    #[test]
    fn a_restore_puts_files_back_and_removes_the_directories_it_empties() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let day = root.join("2024-03-15");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("a.jpg"), b"first").unwrap();
        fs::write(day.join("b.jpg"), b"second").unwrap();

        let plan = RestorePlan {
            journal: root.join("journal.jsonl"),
            header: RunHeader::new("20240315-103000-abc123", root, vec![]),
            steps: vec![
                RestoreStep {
                    seq: 1,
                    current: day.join("b.jpg"),
                    original: root.join("in/second.jpg"),
                    source_size: Some(6),
                    source_hash: None,
                    kind: IntentKind::Organise,
                },
                RestoreStep {
                    seq: 0,
                    current: day.join("a.jpg"),
                    original: root.join("in/first.jpg"),
                    source_size: Some(5),
                    source_hash: None,
                    kind: IntentKind::Organise,
                },
            ],
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.restored, 2);
        assert_eq!(run.failed, 0);
        assert_eq!(run.unprocessed, 0);
        assert_eq!(fs::read(root.join("in/first.jpg")).unwrap(), b"first");
        assert_eq!(fs::read(root.join("in/second.jpg")).unwrap(), b"second");
        assert!(
            !day.exists(),
            "the date directory the restore emptied should be gone"
        );
        assert_eq!(run.pruned_dirs, 1);
        assert!(root.exists(), "the library itself is never removed");
    }

    /// The no-clobber promise. A source path that has been filled again since
    /// the run keeps its occupant; the restored file lands beside it.
    #[test]
    fn a_restore_never_overwrites_whatever_now_occupies_the_source() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("in")).unwrap();
        fs::create_dir_all(root.join("2024-03-15")).unwrap();
        fs::write(root.join("2024-03-15/a.jpg"), b"the organised file").unwrap();
        fs::write(root.join("in/first.jpg"), b"something else entirely").unwrap();

        let plan = RestorePlan {
            journal: root.join("journal.jsonl"),
            header: RunHeader::new("20240315-103000-abc123", root, vec![]),
            steps: vec![RestoreStep {
                seq: 0,
                current: root.join("2024-03-15/a.jpg"),
                original: root.join("in/first.jpg"),
                source_size: None,
                source_hash: None,
                kind: IntentKind::Organise,
            }],
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.restored, 1);
        assert_eq!(
            fs::read(root.join("in/first.jpg")).unwrap(),
            b"something else entirely",
            "the file that was already there must survive untouched"
        );
        let RestoreOutcome::Restored { at, .. } = &run.outcomes[0].1 else {
            panic!("expected a restore, got {:?}", run.outcomes[0].1);
        };
        assert_eq!(at, &root.join("in/first-1.jpg"));
    }

    /// One file that cannot be put back is not a reason to abandon the rest.
    #[test]
    fn a_missing_file_is_counted_and_the_rest_still_restored() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("2024-03-15")).unwrap();
        fs::write(root.join("2024-03-15/b.jpg"), b"still here").unwrap();

        let plan = RestorePlan {
            journal: root.join("journal.jsonl"),
            header: RunHeader::new("20240315-103000-abc123", root, vec![]),
            steps: vec![
                RestoreStep {
                    seq: 0,
                    current: root.join("2024-03-15/gone.jpg"),
                    original: root.join("in/gone.jpg"),
                    source_size: None,
                    source_hash: None,
                    kind: IntentKind::Organise,
                },
                RestoreStep {
                    seq: 1,
                    current: root.join("2024-03-15/b.jpg"),
                    original: root.join("in/b.jpg"),
                    source_size: None,
                    source_hash: None,
                    kind: IntentKind::Organise,
                },
            ],
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.restored, 1);
        assert_eq!(run.failed, 1);
        assert!(root.join("in/b.jpg").exists());
        assert!(matches!(run.outcomes[0].1, RestoreOutcome::Failed { .. }));
    }

    /// An undo is a run like any other, and its journal has to describe it well
    /// enough for a second undo to reverse it.
    #[test]
    fn a_restore_records_itself_well_enough_to_be_undone_again() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("2024-03-15")).unwrap();
        fs::write(root.join("2024-03-15/a.jpg"), b"photo").unwrap();

        let mut journal = Journal::create(
            &root.join(".mmm/journal"),
            &RunHeader::new("20240315-110000-undo01", root, vec![]),
        )
        .unwrap();

        let plan = RestorePlan {
            journal: root.join("journal.jsonl"),
            header: RunHeader::new("20240315-103000-abc123", root, vec![]),
            steps: vec![RestoreStep {
                seq: 0,
                current: root.join("2024-03-15/a.jpg"),
                original: root.join("in/a.jpg"),
                source_size: None,
                source_hash: None,
                kind: IntentKind::Organise,
            }],
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::new(Some(&mut journal)));
        assert_eq!(run.restored, 1);

        let undo_journal = journal.path().to_path_buf();
        drop(journal);

        // Reading the undo's own journal back must yield the opposite move.
        let replan = plan_restore(&undo_journal).unwrap();
        assert_eq!(replan.steps.len(), 1);
        assert_eq!(replan.steps[0].current, root.join("in/a.jpg"));
        assert_eq!(replan.steps[0].original, root.join("2024-03-15/a.jpg"));

        let (_, entries) = Journal::read(&undo_journal).unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                JournalEntry::MoveIntent {
                    kind: IntentKind::Restore,
                    ..
                }
            )),
            "a restore must be recorded as one: {entries:?}"
        );
    }

    // -----------------------------------------------------------------
    // Pruning
    // -----------------------------------------------------------------

    /// The guarantee that matters: a directory still holding somebody's file
    /// is not removed, and neither is anything above it.
    #[test]
    fn pruning_leaves_a_directory_that_still_holds_a_file() {
        let tmp = TempDir::new().unwrap();
        let day = tmp.path().join("2024-03-15");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("someone-elses.txt"), b"not mine").unwrap();

        assert_eq!(prune_empty_dirs(std::slice::from_ref(&day), tmp.path()), 0);
        assert!(day.exists());
    }

    /// A duplicate group's manifest is a record of a run that happened.
    /// Pruning does not delete records.
    #[test]
    fn pruning_leaves_a_duplicate_group_that_still_holds_its_manifest() {
        let tmp = TempDir::new().unwrap();
        let group = tmp.path().join("duplicates/000");
        fs::create_dir_all(&group).unwrap();
        fs::write(group.join("manifest.txt"), b"group 0").unwrap();

        assert_eq!(
            prune_empty_dirs(std::slice::from_ref(&group), tmp.path()),
            0
        );
        assert!(group.join("manifest.txt").exists());
    }

    #[test]
    fn pruning_never_removes_the_library_itself() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(prune_empty_dirs(&[tmp.path().to_path_buf()], tmp.path()), 0);
        assert!(tmp.path().exists());
    }

    /// Nothing outside the tree the run wrote into is pruning's business, even
    /// if a step's path points there.
    #[test]
    fn pruning_does_not_climb_out_of_the_library() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        let library = tmp.path().join("library");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&library).unwrap();

        assert_eq!(
            prune_empty_dirs(std::slice::from_ref(&outside), &library),
            0
        );
        assert!(outside.exists());
    }

    /// The journal being written by this very undo lives under `.mmm/`.
    #[test]
    fn pruning_never_touches_the_metadata_directory() {
        let tmp = TempDir::new().unwrap();
        let meta = tmp.path().join(".mmm/journal");
        fs::create_dir_all(&meta).unwrap();

        assert_eq!(prune_empty_dirs(std::slice::from_ref(&meta), tmp.path()), 0);
        assert!(meta.exists());
    }

    /// Emptying a nested tree should collapse the whole chain, not just its
    /// deepest directory.
    #[test]
    fn pruning_climbs_while_the_directories_keep_coming_up_empty() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(prune_empty_dirs(&[deep], tmp.path()), 3);
        assert!(!tmp.path().join("a").exists());
        assert!(tmp.path().exists());
    }

    /// Two files out of one directory means that directory is offered twice.
    /// The second pass must keep climbing rather than stopping on "already
    /// gone", or a parent that is now empty survives.
    #[test]
    fn pruning_a_directory_twice_still_reaches_its_parent() {
        let tmp = TempDir::new().unwrap();
        let day = tmp.path().join("2024/2024-03-15");
        fs::create_dir_all(&day).unwrap();

        let removed = prune_empty_dirs(&[day.clone(), day], tmp.path());
        assert_eq!(removed, 2, "the directory and its now-empty parent");
        assert!(!tmp.path().join("2024").exists());
    }

    // -----------------------------------------------------------------
    // Listing
    // -----------------------------------------------------------------

    #[test]
    fn a_listing_reports_runs_newest_first_with_what_each_would_restore() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut first = Journal::create(
            dir,
            &RunHeader::new("20240315-100000-aaaaaa", "/out", vec![]),
        )
        .unwrap();
        first
            .append(&intent(0, "/in/a.jpg", "/out/a.jpg", IntentKind::Organise))
            .unwrap();
        first.append(&committed(0, "/out/a.jpg")).unwrap();
        first
            .append(&JournalEntry::RunCompleted {
                moved: 1,
                failed: 0,
                skipped: 0,
                ended_at: Utc::now(),
            })
            .unwrap();
        drop(first);

        // A second run that was interrupted: no closing line.
        let mut second = Journal::create(
            dir,
            &RunHeader::new("20240316-100000-bbbbbb", "/out", vec![]),
        )
        .unwrap();
        second
            .append(&intent(0, "/in/b.jpg", "/out/b.jpg", IntentKind::Organise))
            .unwrap();
        drop(second);

        let rows = summarise_runs(dir).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].run_id, "20240316-100000-bbbbbb");
        let newest = rows[0].detail.as_ref().unwrap();
        assert_eq!(newest.restorable, 0);
        assert!(
            newest.completion.is_none(),
            "a run with no closing line was interrupted"
        );

        let oldest = rows[1].detail.as_ref().unwrap();
        assert_eq!(oldest.restorable, 1);
        assert_eq!(oldest.completion.as_ref().unwrap().moved, 1);
    }

    /// One bad journal must not hide the good ones next to it — a listing is
    /// most useful precisely when something has gone wrong.
    #[test]
    fn an_unreadable_journal_becomes_a_row_rather_than_an_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        drop(
            Journal::create(
                dir,
                &RunHeader::new("20240315-100000-aaaaaa", "/out", vec![]),
            )
            .unwrap(),
        );
        fs::write(dir.join("20240316-100000-bbbbbb.jsonl"), b"").unwrap();

        let rows = summarise_runs(dir).unwrap();

        assert_eq!(rows.len(), 2);
        assert!(rows[0].detail.is_err(), "the empty journal cannot be read");
        assert!(rows[1].detail.is_ok(), "the good one is still listed");
    }
}
