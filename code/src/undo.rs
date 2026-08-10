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
//! * **Ambiguity is refused, not resolved.** A journal describes the library as
//!   the run left it. If the file at a recorded destination is gone, or is no
//!   longer the file that was put there, the world has moved on since and
//!   nothing in the journal says how — so that file is reported and left alone.
//!   See [`verify_step`].
//! * **What cannot be reversed is still reported.** A run killed between an
//!   intent and its outcome leaves a move nothing can undo, because nothing says
//!   whether it happened. Silence there would be the worst of both: see
//!   [`UnresolvedIntent`].

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tracing::{debug, error, warn};

use crate::hasher;
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

/// A move whose intent reached the disk and whose outcome never did.
///
/// This is exactly what an interruption mid-rename looks like from the far
/// side. The intent is synced *before* the move is attempted, so its presence
/// says only that the run was about to act: the file is now either still at
/// `source` or already at `destination`, and no line in the journal says which.
///
/// Undo will not guess between them. Restoring a file that never moved would
/// take it from where it belongs; skipping one that did would leave it in the
/// library with nothing recording it. So the move is neither reversed nor
/// silently dropped — it is handed to the operator to check, which is the only
/// party that can actually look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedIntent {
    pub seq: u64,
    /// Where the file was before the run touched it.
    pub source: PathBuf,
    /// Where the run was about to put it. Not necessarily where it is: a move
    /// that got as far as resolving a collision would have landed beside this.
    pub destination: PathBuf,
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
    /// Moves the journal began and never finished recording. Not steps — there
    /// is nothing safe to do about them — but not nothing either. See
    /// [`UnresolvedIntent`].
    pub unresolved: Vec<UnresolvedIntent>,
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
    // Every sequence number some later line accounted for, however it turned
    // out. An intent missing from this set is one the run never finished
    // reporting on.
    let mut settled: HashSet<u64> = HashSet::new();
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
            // All three are outcomes: the move happened, the move happened by
            // the duplicate door, or the move was attempted and did not. Any of
            // them answers the question the intent asked.
            JournalEntry::MoveCommitted { seq, .. }
            | JournalEntry::DuplicateMoved { seq, .. }
            | JournalEntry::MoveFailed { seq, .. } => {
                settled.insert(*seq);
            }
            JournalEntry::RunCompleted { .. } => interrupted = false,
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

    // Journal order, not reverse: this is a list for a person to work through
    // rather than a sequence of operations, and the order the run recorded them
    // in is the order they happened.
    let unresolved = entries
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::MoveIntent {
                seq,
                source,
                destination,
                kind,
                ..
            } if !settled.contains(seq) => Some(UnresolvedIntent {
                seq: *seq,
                source: source.clone(),
                destination: destination.clone(),
                kind: *kind,
            }),
            _ => None,
        })
        .collect();

    RestorePlan {
        journal,
        header,
        steps,
        unresolved,
        interrupted,
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// What is actually sitting at a step's recorded destination.
///
/// A journal describes the library as one run left it, which may have been
/// weeks ago. Between then and now a file can be deleted, edited, or replaced
/// by something else under the same name, and no amount of reading the journal
/// will reveal it — only looking will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The file is where the run left it, and is still the file the run moved.
    Intact,
    /// Nothing is at the recorded destination.
    Missing,
    /// Something is there, but it is not what the run put there.
    Modified { detail: String },
    /// The check itself could not be made, so nothing is known either way.
    /// Treated exactly like a refusal: an unanswered question is not a yes.
    Unverifiable { detail: String },
}

impl Verification {
    /// Whether the step may proceed. Only [`Intact`](Self::Intact) may.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        matches!(self, Self::Intact)
    }
}

/// The first few characters of a digest, for a message a person will read.
///
/// Takes characters rather than slicing bytes: a journal is a text file an
/// operator can edit, and a `source_hash` that is not the 64 hex characters it
/// should be must produce a bad message, never a panic.
fn abbreviate(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// Look at what is at `step.current` and decide whether it is safe to move.
///
/// The checks run cheapest-first and stop at the first answer: existence, then
/// file-ness, then size, then — only when the run recorded one — the digest.
/// Every one of them can only ever *refuse*; none of them can turn an absent
/// or altered file into a restorable one.
///
/// A hash is recorded for duplicates and for restores, never for uniquely
/// organised files (the dedup cascade never fully hashes those), so the
/// expensive check costs nothing on the common path.
#[must_use]
pub fn verify_step(step: &RestoreStep) -> Verification {
    // `symlink_metadata`, not `metadata`: a symlink left where the file was is
    // a replacement, and following it would verify some other file entirely
    // and then move the link.
    let meta = match fs::symlink_metadata(&step.current) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Verification::Missing,
        Err(e) => {
            return Verification::Unverifiable {
                detail: format!("it could not be inspected: {e}"),
            }
        }
    };

    if !meta.is_file() {
        return Verification::Modified {
            detail: "a directory or link now occupies this path".to_string(),
        };
    }

    if let Some(expected) = step.source_size {
        if meta.len() != expected {
            return Verification::Modified {
                detail: format!(
                    "it is now {} bytes, and the run recorded {expected}",
                    meta.len()
                ),
            };
        }
    }

    if let Some(expected) = &step.source_hash {
        match hasher::full_hash(&step.current) {
            Ok(actual) if &actual == expected => {}
            Ok(actual) => {
                return Verification::Modified {
                    detail: format!(
                        "its contents have changed (now {}…, the run recorded {}…)",
                        abbreviate(&actual),
                        abbreviate(expected)
                    ),
                }
            }
            Err(e) => {
                return Verification::Unverifiable {
                    detail: format!("its contents could not be read: {e:#}"),
                }
            }
        }
    }

    Verification::Intact
}

/// Check every step of a plan, in order, without touching anything.
///
/// Used by the preview: an operator deciding whether to commit needs to know
/// which files will be refused *before* they commit, not from the aftermath.
#[must_use]
pub fn verify_plan(plan: &RestorePlan) -> Vec<Verification> {
    plan.steps.iter().map(verify_step).collect()
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// What became of one [`RestoreStep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Put back where it came from, under its original name.
    Restored { at: PathBuf, kind: MoveKind },
    /// Put back, but something else now occupies the original path, so the file
    /// landed beside it under a collision-suffixed name. Nothing was
    /// overwritten, and the library is not quite as it was.
    Conflicted { at: PathBuf, kind: MoveKind },
    /// Moved back, but the journal line recording it could not be written, so
    /// this undo is not itself fully reversible. The run stops here.
    RestoredUnrecorded,
    /// Nothing is at the recorded destination, so there is nothing to put back.
    SkippedMissing,
    /// Something is at the recorded destination, but it is not the file the run
    /// moved there. Left exactly where it is.
    SkippedModified { reason: String },
    /// The move back was attempted and did not happen, or could not be
    /// attempted safely. The file is still where the run left it.
    Failed { reason: String },
}

/// What an undo run did.
///
/// Every step is accounted for exactly once:
/// `restored + conflicted + skipped_missing + skipped_modified + failed +
/// unprocessed == plan.steps.len()`.
#[derive(Debug, Default, Clone)]
pub struct RestoreRun {
    /// One entry per considered step, indexed into [`RestorePlan::steps`].
    pub outcomes: Vec<(usize, RestoreOutcome)>,
    /// Files put back under their original name.
    pub restored: usize,
    /// Files put back beside an occupant of their original path.
    pub conflicted: usize,
    /// Files that were no longer at the destination the run recorded.
    pub skipped_missing: usize,
    /// Files that were still there but were no longer the same file.
    pub skipped_modified: usize,
    pub failed: usize,
    /// Steps never attempted, because the run stopped before reaching them.
    pub unprocessed: usize,
    /// The undo stopped because it could not record itself.
    pub journal_failed: bool,
    /// Directories the restore emptied and then removed.
    pub pruned_dirs: usize,
}

impl RestoreRun {
    /// Files this undo moved: back to their own name, or beside it.
    ///
    /// A conflicted file did leave the library, so the run's journal has to
    /// count it as moved even though the tree is not quite as it was.
    #[must_use]
    pub fn moved(&self) -> usize {
        self.restored + self.conflicted
    }

    /// Files this undo deliberately left alone, plus those it never reached.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped_missing + self.skipped_modified + self.unprocessed
    }

    /// Why this undo did not put the run back exactly as it was, in the words
    /// the operator gets — or `None` when it did.
    ///
    /// Returned rather than printed so the exit-code decision is one testable
    /// value, and so `main` cannot exit zero on a run whose table said
    /// otherwise. A conflicted file counts: it was restored, but not to the
    /// path it came from, and a script that deletes the library on a clean undo
    /// would be wrong to treat that as clean.
    #[must_use]
    pub fn shortfall(&self) -> Option<String> {
        let parts = [
            (self.failed, "could not be restored"),
            (self.skipped_missing, "skipped (missing)"),
            (self.skipped_modified, "skipped (modified)"),
            (self.conflicted, "restored under a different name"),
            (self.unprocessed, "never attempted"),
        ];

        let described: Vec<String> = parts
            .iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{count} {label}"))
            .collect();

        (!described.is_empty()).then(|| described.join(", "))
    }
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
        // Checked here, immediately before the move, rather than once up front:
        // the restores run in reverse and can themselves change what is at a
        // later step's destination, so a verdict from the top of the loop would
        // already be stale by the time it was used.
        match verify_step(step) {
            Verification::Intact => {}
            Verification::Missing => {
                warn!(
                    seq = step.seq,
                    at = %step.current.display(),
                    "nothing is where the run left this file, so there is nothing to put back"
                );
                run.skipped_missing += 1;
                run.outcomes.push((index, RestoreOutcome::SkippedMissing));
                continue;
            }
            Verification::Modified { detail } => {
                warn!(
                    seq = step.seq,
                    at = %step.current.display(),
                    detail,
                    "this is no longer the file the run moved, so it has been left alone"
                );
                run.skipped_modified += 1;
                run.outcomes
                    .push((index, RestoreOutcome::SkippedModified { reason: detail }));
                continue;
            }
            // Not knowing is not the same as knowing it is fine, and undo does
            // not act on the difference.
            Verification::Unverifiable { detail } => {
                error!(
                    seq = step.seq,
                    at = %step.current.display(),
                    detail,
                    "this file could not be checked, so it has not been moved"
                );
                run.failed += 1;
                run.outcomes
                    .push((index, RestoreOutcome::Failed { reason: detail }));
                continue;
            }
        }

        let planned = PlannedMove {
            source: step.current.clone(),
            destination: step.original.clone(),
            date_source: DateSource::None,
            // A restore puts a file back where it came from; no date was read
            // and no wall clock chosen.
            timezone_source: None,
            has_location: false,
            // A restore carries its digest on the purpose, below.
            known_hash: None,
            // A sidecar's own move was journalled as an entry in its own right,
            // so the plan already holds a step for it. Attaching it here as well
            // would try to restore the same file twice — the second attempt
            // finding nothing where the first had already moved it from.
            sidecars: Vec::new(),
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
                note_vacated(&mut vacated, &step.current);
                // The move is the authority on whether the original path was
                // taken: `execute_move` walks the collision candidates and only
                // ever lands somewhere else when the name it wanted was held.
                // Deciding from the result rather than from a pre-flight
                // `exists()` means the report cannot disagree with the disk.
                if outcome.destination == step.original {
                    run.restored += 1;
                    RestoreOutcome::Restored {
                        at: outcome.destination,
                        kind: outcome.kind,
                    }
                } else {
                    warn!(
                        seq = step.seq,
                        original = %step.original.display(),
                        at = %outcome.destination.display(),
                        "the original path is occupied by another file, so this one was restored \
                         beside it rather than over it"
                    );
                    run.conflicted += 1;
                    RestoreOutcome::Conflicted {
                        at: outcome.destination,
                        kind: outcome.kind,
                    }
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

    /// A move that failed never happened, so there is nothing to reverse — and
    /// nothing ambiguous about it either. The journal answered the question.
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
        assert!(
            plan.unresolved.is_empty(),
            "a recorded failure is an outcome, not an open question: {:?}",
            plan.unresolved
        );
    }

    /// An intent with no outcome at all is the interrupted-mid-rename case. It
    /// is not restorable — nothing says the file moved — and it must not be
    /// silently treated as one that did, in either direction.
    #[test]
    fn an_intent_with_no_outcome_produces_no_step_but_is_reported() {
        let plan = plan_of(&[intent(
            0,
            "/photos/a.jpg",
            "/out/x.jpg",
            IntentKind::Organise,
        )]);

        assert!(plan.steps.is_empty(), "nothing says the file moved");
        assert_eq!(
            plan.unresolved,
            vec![UnresolvedIntent {
                seq: 0,
                source: PathBuf::from("/photos/a.jpg"),
                destination: PathBuf::from("/out/x.jpg"),
                kind: IntentKind::Organise,
            }],
            "both ends of the move must reach the operator — those are the two places to look"
        );
    }

    /// The ordinary case, and the one a false positive here would ruin: a run
    /// that finished has nothing for the operator to check by hand.
    #[test]
    fn a_completed_run_leaves_nothing_unresolved() {
        let plan = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            committed(0, "/out/x.jpg"),
            intent(
                1,
                "/photos/copy.jpg",
                "/out/duplicates/000/copy.jpg",
                IntentKind::Duplicate,
            ),
            JournalEntry::DuplicateMoved {
                seq: 1,
                group: 0,
                source: PathBuf::from("/photos/copy.jpg"),
                destination: PathBuf::from("/out/duplicates/000/copy.jpg"),
            },
            JournalEntry::RunCompleted {
                moved: 2,
                failed: 0,
                skipped: 0,
                ended_at: Utc::now(),
            },
        ]);

        assert_eq!(plan.steps.len(), 2);
        assert!(plan.unresolved.is_empty(), "{:?}", plan.unresolved);
    }

    /// Only the move the interruption caught is ambiguous. Reporting the whole
    /// run would bury the one file that actually needs looking at.
    #[test]
    fn only_the_intent_the_interruption_caught_is_reported() {
        let plan = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            committed(0, "/out/x.jpg"),
            intent(1, "/photos/b.jpg", "/out/y.jpg", IntentKind::Organise),
            committed(1, "/out/y.jpg"),
            // The run died here, between writing this intent and acting on it.
            intent(2, "/photos/c.jpg", "/out/z.jpg", IntentKind::Organise),
        ]);

        let seqs: Vec<u64> = plan.unresolved.iter().map(|u| u.seq).collect();
        assert_eq!(seqs, vec![2]);
        assert_eq!(plan.steps.len(), 2, "the recorded moves still come back");
        assert!(plan.interrupted);
    }

    /// The list is for a person to work through, so it reads in the order the
    /// run recorded the moves rather than in the reverse order the steps use.
    #[test]
    fn unresolved_intents_are_listed_in_the_order_the_run_recorded_them() {
        let plan = plan_of(&[
            intent(0, "/photos/a.jpg", "/out/x.jpg", IntentKind::Organise),
            intent(1, "/photos/b.jpg", "/out/y.jpg", IntentKind::Organise),
            intent(2, "/photos/c.jpg", "/out/z.jpg", IntentKind::Organise),
        ]);

        let seqs: Vec<u64> = plan.unresolved.iter().map(|u| u.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
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
            unresolved: Vec::new(),
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
            unresolved: Vec::new(),
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.conflicted, 1, "restored, but not to its own name");
        assert_eq!(run.restored, 0);
        assert_eq!(
            fs::read(root.join("in/first.jpg")).unwrap(),
            b"something else entirely",
            "the file that was already there must survive untouched"
        );
        let RestoreOutcome::Conflicted { at, .. } = &run.outcomes[0].1 else {
            panic!("expected a conflict, got {:?}", run.outcomes[0].1);
        };
        assert_eq!(at, &root.join("in/first-1.jpg"));
        assert!(
            run.shortfall().is_some(),
            "a library that is not as it was must not report a clean undo"
        );
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
            unresolved: Vec::new(),
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.restored, 1);
        assert_eq!(run.skipped_missing, 1);
        assert_eq!(run.failed, 0, "an absent file is a skip, not a failure");
        assert!(root.join("in/b.jpg").exists());
        assert!(matches!(run.outcomes[0].1, RestoreOutcome::SkippedMissing));
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
            unresolved: Vec::new(),
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
    // Verification
    // -----------------------------------------------------------------

    /// A step pointing at `path`, claiming the file there is `size` bytes and,
    /// when given, hashes to `hash`.
    fn step_for(path: &Path, size: Option<u64>, hash: Option<&str>) -> RestoreStep {
        RestoreStep {
            seq: 0,
            current: path.to_path_buf(),
            original: path.with_file_name("original.jpg"),
            source_size: size,
            source_hash: hash.map(ToString::to_string),
            kind: IntentKind::Organise,
        }
    }

    #[test]
    fn a_file_matching_what_the_run_recorded_verifies_intact() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jpg");
        fs::write(&file, b"photo").unwrap();
        let hash = crate::hasher::full_hash(&file).unwrap();

        assert_eq!(
            verify_step(&step_for(&file, Some(5), Some(&hash))),
            Verification::Intact
        );
    }

    #[test]
    fn a_file_that_is_no_longer_there_verifies_missing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            verify_step(&step_for(&tmp.path().join("gone.jpg"), Some(5), None)),
            Verification::Missing
        );
    }

    /// The cheap check, and the one that catches an edit in place.
    #[test]
    fn a_file_of_a_different_size_verifies_modified() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jpg");
        fs::write(&file, b"a much longer photo than before").unwrap();

        let Verification::Modified { detail } = verify_step(&step_for(&file, Some(5), None)) else {
            panic!("a file that grew must not verify as intact");
        };
        assert!(
            detail.contains('5'),
            "the recorded size belongs in the reason: {detail}"
        );
    }

    /// The check that catches a replacement of exactly the same length — the
    /// case size alone cannot see.
    #[test]
    fn a_file_of_the_same_size_but_different_contents_verifies_modified() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jpg");
        fs::write(&file, b"first").unwrap();
        let hash = crate::hasher::full_hash(&file).unwrap();
        fs::write(&file, b"other").unwrap();

        assert!(
            matches!(
                verify_step(&step_for(&file, Some(5), Some(&hash))),
                Verification::Modified { .. }
            ),
            "same length, different bytes — only the digest can tell"
        );
    }

    /// A run records no digest for a uniquely organised file, so the only
    /// evidence available is existence and size. That is not a reason to refuse.
    #[test]
    fn a_step_with_no_recorded_evidence_verifies_on_existence_alone() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.jpg");
        fs::write(&file, b"anything at all").unwrap();

        assert_eq!(
            verify_step(&step_for(&file, None, None)),
            Verification::Intact
        );
    }

    /// A directory standing where the file was is a replacement, not a file to
    /// move — and moving it would take its contents with it.
    #[test]
    fn a_directory_where_the_file_was_verifies_modified() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.jpg");
        fs::create_dir(&path).unwrap();

        assert!(matches!(
            verify_step(&step_for(&path, None, None)),
            Verification::Modified { .. }
        ));
    }

    /// Verification uses `symlink_metadata`, so a link left in the file's place
    /// is judged as the link it is rather than as whatever it points at.
    #[test]
    fn a_symlink_where_the_file_was_verifies_modified() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("elsewhere.jpg");
        fs::write(&target, b"photo").unwrap();
        let link = tmp.path().join("a.jpg");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(
            matches!(
                verify_step(&step_for(&link, Some(5), None)),
                Verification::Modified { .. }
            ),
            "following the link would verify one file and then move another"
        );
    }

    // -----------------------------------------------------------------
    // Refusing rather than guessing
    // -----------------------------------------------------------------

    /// The headline behaviour: a file that has changed since the run is left
    /// exactly where it is, and said so.
    #[test]
    fn a_modified_file_is_reported_and_left_alone() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("2024-03-15")).unwrap();
        let organised = root.join("2024-03-15/a.jpg");
        fs::write(&organised, b"edited since the run").unwrap();

        let plan = RestorePlan {
            journal: root.join("journal.jsonl"),
            header: RunHeader::new("20240315-103000-abc123", root, vec![]),
            steps: vec![RestoreStep {
                seq: 0,
                current: organised.clone(),
                original: root.join("in/a.jpg"),
                source_size: Some(5),
                source_hash: None,
                kind: IntentKind::Organise,
            }],
            unresolved: Vec::new(),
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.skipped_modified, 1);
        assert_eq!(run.restored, 0);
        assert!(organised.exists(), "the file must not have been moved");
        assert!(!root.join("in/a.jpg").exists());
        assert!(matches!(
            run.outcomes[0].1,
            RestoreOutcome::SkippedModified { .. }
        ));
    }

    /// A skipped file writes no journal line at all: nothing happened to it, so
    /// an undo of this undo has nothing to reverse.
    #[test]
    fn a_skipped_file_is_never_recorded_as_moved() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("2024-03-15")).unwrap();
        fs::write(root.join("2024-03-15/a.jpg"), b"edited since the run").unwrap();

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
                source_size: Some(5),
                source_hash: None,
                kind: IntentKind::Organise,
            }],
            unresolved: Vec::new(),
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::new(Some(&mut journal)));
        assert_eq!(run.skipped_modified, 1);

        let path = journal.path().to_path_buf();
        drop(journal);
        let (_, entries) = Journal::read(&path).unwrap();
        assert!(
            entries.is_empty(),
            "a file that was never touched must leave no trace: {entries:?}"
        );
    }

    /// One ambiguous file does not stop the ones either side of it.
    #[test]
    fn every_step_gets_its_own_verdict() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let day = root.join("2024-03-15");
        fs::create_dir_all(&day).unwrap();
        fs::create_dir_all(root.join("in")).unwrap();
        fs::write(day.join("good.jpg"), b"photo").unwrap();
        fs::write(day.join("changed.jpg"), b"no longer five").unwrap();
        fs::write(day.join("conflicted.jpg"), b"photo").unwrap();
        // Something has taken the path this last one came from.
        fs::write(root.join("in/conflicted.jpg"), b"a newer file").unwrap();

        let step = |name: &str| RestoreStep {
            seq: 0,
            current: day.join(name),
            original: root.join("in").join(name),
            source_size: Some(5),
            source_hash: None,
            kind: IntentKind::Organise,
        };

        let plan = RestorePlan {
            journal: root.join("journal.jsonl"),
            header: RunHeader::new("20240315-103000-abc123", root, vec![]),
            steps: vec![
                step("good.jpg"),
                step("changed.jpg"),
                step("conflicted.jpg"),
                step("vanished.jpg"),
            ],
            unresolved: Vec::new(),
            interrupted: false,
        };

        let run = execute_restore(&plan, &mut MoveRecorder::disabled());

        assert_eq!(run.restored, 1);
        assert_eq!(run.skipped_modified, 1);
        assert_eq!(run.conflicted, 1);
        assert_eq!(run.skipped_missing, 1);
        assert_eq!(run.failed, 0);
        assert_eq!(
            run.outcomes.len(),
            4,
            "every step is accounted for exactly once"
        );
        assert_eq!(
            fs::read(root.join("in/conflicted.jpg")).unwrap(),
            b"a newer file",
            "the occupant of a conflicted path is never overwritten"
        );
        assert!(root.join("in/conflicted-1.jpg").exists());
    }

    /// The exit-code decision, as one value. Anything short of "every file is
    /// back under its own name" has to be visible to a script.
    #[test]
    fn a_clean_undo_reports_no_shortfall_and_anything_else_does() {
        let clean = RestoreRun {
            restored: 3,
            ..RestoreRun::default()
        };
        assert!(clean.shortfall().is_none());

        let messy = RestoreRun {
            restored: 1,
            conflicted: 1,
            skipped_missing: 2,
            ..RestoreRun::default()
        };
        let shortfall = messy.shortfall().expect("this undo was not clean");
        assert!(shortfall.contains("2 skipped (missing)"), "{shortfall}");
        assert!(shortfall.contains('1'), "{shortfall}");
    }

    /// A conflicted file did leave the library, so the undo's own journal has to
    /// count it as moved — otherwise its `RunCompleted` line disagrees with the
    /// commit records above it.
    #[test]
    fn the_counts_a_journal_records_add_up_to_every_step() {
        let run = RestoreRun {
            outcomes: Vec::new(),
            restored: 2,
            conflicted: 1,
            skipped_missing: 3,
            skipped_modified: 4,
            failed: 5,
            unprocessed: 6,
            journal_failed: false,
            pruned_dirs: 0,
        };

        assert_eq!(run.moved(), 3);
        assert_eq!(run.skipped(), 13);
        assert_eq!(run.moved() + run.skipped() + run.failed, 21);
    }

    /// A hand-edited journal must produce a bad message, never a panic.
    #[test]
    fn abbreviating_a_hash_shorter_than_the_abbreviation_is_not_a_panic() {
        assert_eq!(abbreviate("abc"), "abc");
        assert_eq!(abbreviate(""), "");
        assert_eq!(abbreviate("0123456789abcdef"), "0123456789ab");
    }

    // -----------------------------------------------------------------
    // Verification
    // -----------------------------------------------------------------

    /// Only `Intact` may proceed, and the other three verdicts are each a
    /// refusal.
    ///
    /// Found by mutation testing: `is_intact` could be made to return `true`
    /// for everything or `false` for everything and the whole suite stayed
    /// green, because nothing in the crate calls it — `execute_restore`
    /// matches on the variants directly. It is public API on a public enum and
    /// its contract is the one sentence undo rests on, so it is pinned here
    /// rather than deleted; that it has no in-crate caller is recorded in
    /// `docs/research/mutation-testing.md` as the finding it is.
    #[test]
    fn only_an_intact_file_may_be_restored() {
        assert!(Verification::Intact.is_intact());

        for refused in [
            Verification::Missing,
            Verification::Modified {
                detail: "size changed".to_string(),
            },
            Verification::Unverifiable {
                detail: "it could not be inspected".to_string(),
            },
        ] {
            assert!(
                !refused.is_intact(),
                "{refused:?} is a refusal, not a licence to move the file"
            );
        }
    }

    /// "I could not look" is not reported as "it is gone".
    ///
    /// The two verdicts are counted differently and printed differently, and
    /// the difference is the whole of the doc comment on
    /// [`Verification::Unverifiable`]: `Missing` says the file the run left
    /// here is no longer here, which for somebody in the middle of a recovery
    /// is a statement about their photograph. A file sitting safely inside a
    /// directory this process cannot search is not that.
    ///
    /// Found by mutation testing: widening the `NotFound` guard to match every
    /// error — so that a permission failure returns `Missing` — survived the
    /// whole suite. The `NotFound` path itself was tested; the path where the
    /// question could not be asked at all was not.
    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_inspected_is_not_reported_as_missing() {
        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let hidden = locked.join("photo.jpg");
        fs::write(&hidden, b"contents").unwrap();

        let Some(_guard) = crate::fixtures::deny_reads(&locked) else {
            eprintln!(
                "SKIPPED a_file_that_cannot_be_inspected_is_not_reported_as_missing: \
                 a 0o000 directory was still searchable, so this process ignores \
                 permission bits (running as root?)"
            );
            return;
        };

        let step = RestoreStep {
            seq: 1,
            current: hidden,
            original: tmp.path().join("photo.jpg"),
            source_size: Some(8),
            source_hash: None,
            kind: IntentKind::Organise,
        };

        assert!(
            matches!(verify_step(&step), Verification::Unverifiable { .. }),
            "a file inside an unsearchable directory has not been shown to be gone, \
             so the verdict must say the check failed: {:?}",
            verify_step(&step)
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
