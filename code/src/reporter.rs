use std::io::{self, Write};
use std::path::Path;

use crate::hasher::DuplicateGroup;
use crate::journal::{JournalEntry, RunHeader};
use crate::metadata::DateSource;
use crate::organiser::PlannedMove;
use crate::undo::{RestoreOutcome, RestorePlan, RestoreRun, RunRow};

/// Print the duplicate groups found during scanning
pub fn print_duplicates(groups: &[DuplicateGroup]) {
    if groups.is_empty() {
        println!("\nNo duplicates found.");
        return;
    }

    println!("\n═══ Duplicate Groups ═══");
    for (i, group) in groups.iter().enumerate() {
        println!(
            "\nGroup {} ({} files, {} bytes each, hash: {}…):",
            i + 1,
            group.files.len(),
            group.size,
            &group.hash[..16]
        );
        for file in &group.files {
            println!("  → {}", file.display());
        }
    }
    println!(
        "\nTotal: {} duplicate groups, {} duplicate files",
        groups.len(),
        groups.iter().map(|g| g.files.len() - 1).sum::<usize>()
    );
}

/// Announced before anything is scanned, and again after a dry-run listing.
pub const DRY_RUN_BANNER: &str =
    "DRY RUN — no files will be modified. Re-run with --commit to apply.";

/// Announced before any file is moved.
pub const COMMIT_BANNER: &str = "COMMIT MODE — files will be moved.";

/// Print the posture banner so the caller knows which mode they are in
/// before a single file is touched.
pub fn print_mode_banner(dry_run: bool) {
    if dry_run {
        println!("\n{DRY_RUN_BANNER}");
    } else {
        println!("\n{COMMIT_BANNER}");
    }
}

/// Print the planned moves for dry-run mode
pub fn print_dry_run(moves: &[PlannedMove]) {
    if moves.is_empty() {
        println!("\nNo files to organise.");
        return;
    }

    println!("\n═══ Dry Run — Planned Operations ═══\n");

    let mut exif_count = 0;
    let mut fs_count = 0;
    let mut no_date_count = 0;
    let mut with_location = 0;

    for planned in moves {
        let source_tag = match planned.date_source {
            DateSource::Exif => {
                exif_count += 1;
                "[EXIF]"
            }
            DateSource::Filesystem => {
                fs_count += 1;
                "[FS]"
            }
            DateSource::None => {
                no_date_count += 1;
                "[NO DATE]"
            }
        };

        if planned.has_location {
            with_location += 1;
        }

        println!(
            "  {} {} → {}",
            source_tag,
            planned.source.display(),
            planned.destination.display()
        );
    }

    println!("\n═══ Dry Run Summary ═══");
    println!("  Total files: {}", moves.len());
    println!("  Date from EXIF: {exif_count}");
    println!("  Date from filesystem: {fs_count}");
    println!("  No date (unsorted): {no_date_count}");
    println!("  With GPS location: {with_location}");
}

/// Everything the closing summary reports.
///
/// Named fields rather than a row of positional `usize`s. Every figure here
/// is the same type, so a transposed pair would compile, run, and quietly
/// report the wrong thing — and two of these fields exist precisely so that
/// files the run passed over cannot go unnoticed.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunSummary {
    /// Media files the scan discovered.
    pub scanned: usize,
    /// Files actually moved into the output tree.
    pub organised: usize,
    pub duplicate_groups: usize,
    pub duplicate_files: usize,
    /// Entries the scan could not read — see [`crate::scanner::ScanResult`].
    pub scan_skipped: usize,
    /// Files excluded from duplicate detection because they could not be
    /// hashed — see [`crate::hasher::DedupResult`].
    pub hash_skipped: usize,
    /// Files planned but never attempted, because the operator stopped the run
    /// at a chunk prompt — see [`crate::organiser::MoveRun`].
    pub unprocessed: usize,
    pub errors: usize,
}

/// Column width of the summary labels, so every figure lines up.
const LABEL_WIDTH: usize = 20;

/// Label for entries the scan passed over. Exported so the integration suite
/// asserts against the string the binary actually prints.
pub const SCAN_SKIPPED_LABEL: &str = "Unreadable (scan):";

/// Label for files dropped from duplicate detection.
pub const HASH_SKIPPED_LABEL: &str = "Unhashable (dedup):";

/// Label for files the run never got to because it was stopped.
pub const UNPROCESSED_LABEL: &str = "Not processed:";

/// Label for the run journal's location.
pub const JOURNAL_LABEL: &str = "Journal:";

/// Printed in place of a path when `--no-journal` was used.
///
/// A run with no journal is a run that cannot be reversed, and the summary is
/// the last moment at which saying so is any use.
pub const NO_JOURNAL_NOTICE: &str = "none — this run was not recorded and cannot be undone";

/// What became of this run's journal.
///
/// Three states rather than an `Option<&Path>`, because "there is no journal"
/// means two opposite things: a preview recorded nothing because it moved
/// nothing, and `--no-journal` moved files that can now never be put back. The
/// first deserves silence and the second a warning.
#[derive(Debug, Clone, Copy)]
pub enum JournalStatus<'a> {
    /// A preview. Nothing moved, so there is nothing to undo and nothing to say.
    NotNeeded,
    /// Written here.
    At(&'a Path),
    /// Refused by `--no-journal`.
    Disabled,
}

/// Print where this run's journal went, or that there is not one.
///
/// Called from the summary, and again when the journal is opened: a run that is
/// interrupted never reaches its summary, and the operator of an interrupted
/// run is precisely the one who needs the path.
pub fn print_journal_location(journal: JournalStatus<'_>) {
    match journal {
        JournalStatus::NotNeeded => {}
        JournalStatus::At(path) => println!("  {JOURNAL_LABEL:<LABEL_WIDTH$}{}", path.display()),
        JournalStatus::Disabled => {
            println!("  {JOURNAL_LABEL:<LABEL_WIDTH$}{NO_JOURNAL_NOTICE}");
        }
    }
}

/// Print the final summary after processing.
///
/// The skip lines appear only when something was skipped — a run that omitted
/// nothing should not invite the operator to look for what it omitted. When
/// they do appear they are unconditional: a file left out of the plan is
/// reported here or it is not reported at all.
///
/// The journal line answers "how do I undo this?", which is a question every
/// committing run owes an answer to — including the answer "you cannot". See
/// [`JournalStatus`].
pub fn print_summary(summary: &RunSummary, journal: JournalStatus<'_>) {
    println!("\n═══ Processing Complete ═══");
    println!("  {:<LABEL_WIDTH$}{}", "Files scanned:", summary.scanned);
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Files organised:", summary.organised
    );
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Duplicate groups:", summary.duplicate_groups
    );
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Duplicate files:", summary.duplicate_files
    );
    if summary.scan_skipped > 0 {
        println!(
            "  {SCAN_SKIPPED_LABEL:<LABEL_WIDTH$}{}",
            summary.scan_skipped
        );
    }
    if summary.hash_skipped > 0 {
        println!(
            "  {HASH_SKIPPED_LABEL:<LABEL_WIDTH$}{}",
            summary.hash_skipped
        );
    }
    if summary.unprocessed > 0 {
        println!("  {UNPROCESSED_LABEL:<LABEL_WIDTH$}{}", summary.unprocessed);
    }
    if summary.errors > 0 {
        println!("  {:<LABEL_WIDTH$}{}", "Errors:", summary.errors);
    }
    print_journal_location(journal);
    println!("═══════════════════════════\n");
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// Printed instead of a plan when a run has nothing to put back.
pub const NOTHING_TO_UNDO: &str = "This run moved nothing, so there is nothing to put back.";

/// Printed for a run whose journal has no closing line.
pub const INTERRUPTED_RUN_NOTICE: &str =
    "warning: this run never finished — it was interrupted, so its journal records only the \
     moves it managed. Anything it was part-way through is not listed below.";

/// Announce which run is about to be reversed, and how.
pub fn print_restore_plan(plan: &RestorePlan) {
    println!("\n═══ Undo — Run {} ═══", plan.header.run_id);
    println!("  {:<LABEL_WIDTH$}{}", "Started:", plan.header.started_at);
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Library:",
        plan.header.output_dir.display()
    );
    println!("  {JOURNAL_LABEL:<LABEL_WIDTH$}{}", plan.journal.display());
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Files to restore:",
        plan.steps.len()
    );

    if plan.interrupted {
        println!("\n{INTERRUPTED_RUN_NOTICE}");
    }

    if plan.steps.is_empty() {
        println!("\n{NOTHING_TO_UNDO}");
        return;
    }

    println!();
    for step in &plan.steps {
        println!("  {} → {}", step.current.display(), step.original.display());
    }
}

/// Label for files that went back where they came from.
pub const RESTORED_LABEL: &str = "Restored:";

/// Label for files that could not be put back.
pub const RESTORE_FAILED_LABEL: &str = "Could not restore:";

/// Label for files an interrupted undo never reached.
pub const RESTORE_UNPROCESSED_LABEL: &str = "Not attempted:";

/// Label for the directories a restore emptied and removed.
pub const PRUNED_LABEL: &str = "Directories removed:";

/// Print what the undo actually did, file by file and then in total.
///
/// The per-file lines come first and are unconditional: a run that could not
/// put everything back owes the operator the names, not a count they then have
/// to go looking for.
pub fn print_restore_summary(plan: &RestorePlan, run: &RestoreRun, journal: JournalStatus<'_>) {
    if !run.outcomes.is_empty() {
        println!("\n═══ Undo — Results ═══\n");
        for (index, outcome) in &run.outcomes {
            let Some(step) = plan.steps.get(*index) else {
                continue;
            };
            match outcome {
                RestoreOutcome::Restored { at, .. } => {
                    if at == &step.original {
                        println!("  restored   {}", at.display());
                    } else {
                        // The source path was taken, so the file went back
                        // beside its occupant rather than through it. Saying
                        // where matters more than saying it worked.
                        println!(
                            "  restored   {}  (source path was occupied; original name was {})",
                            at.display(),
                            step.original.display()
                        );
                    }
                }
                RestoreOutcome::RestoredUnrecorded => println!(
                    "  restored   {}  (NOT recorded — this undo cannot itself be undone)",
                    step.original.display()
                ),
                RestoreOutcome::Failed { reason } => {
                    println!("  FAILED     {}: {reason}", step.current.display());
                }
            }
        }
    }

    println!("\n═══ Undo Complete ═══");
    println!("  {RESTORED_LABEL:<LABEL_WIDTH$}{}", run.restored);
    if run.failed > 0 {
        println!("  {RESTORE_FAILED_LABEL:<LABEL_WIDTH$}{}", run.failed);
    }
    if run.unprocessed > 0 {
        println!(
            "  {RESTORE_UNPROCESSED_LABEL:<LABEL_WIDTH$}{}",
            run.unprocessed
        );
    }
    if run.pruned_dirs > 0 {
        println!("  {PRUNED_LABEL:<LABEL_WIDTH$}{}", run.pruned_dirs);
    }
    print_journal_location(journal);
    println!("═════════════════════\n");
}

// ---------------------------------------------------------------------------
// Journal inspection
// ---------------------------------------------------------------------------

/// Printed by `mmm journal list` when a library has journals but none readable,
/// or none at all.
pub const NO_RUNS_RECORDED: &str = "No runs recorded for this library.";

/// Print one line per recorded run, newest first.
pub fn print_run_list(rows: &[RunRow]) {
    if rows.is_empty() {
        println!("\n{NO_RUNS_RECORDED}");
        return;
    }

    println!("\n═══ Recorded Runs ═══\n");
    for row in rows {
        match &row.detail {
            Ok(detail) => {
                let status = match &detail.completion {
                    Some(c) => format!(
                        "moved {}, failed {}, skipped {}",
                        c.moved, c.failed, c.skipped
                    ),
                    None => "INTERRUPTED — never finished".to_string(),
                };
                println!("  {}  {}", row.run_id, detail.started_at);
                println!("    {status}");
                println!(
                    "    {} file{} could be put back by `mmm undo --run {}`",
                    detail.restorable,
                    if detail.restorable == 1 { "" } else { "s" },
                    row.run_id
                );
            }
            // An unreadable journal is still a run that happened, and hiding it
            // would make the listing quietly wrong at the one moment it matters.
            Err(error) => {
                println!("  {}  UNREADABLE", row.run_id);
                println!("    {error}");
            }
        }
        println!();
    }
    println!(
        "Total: {} run{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

/// Print one run's journal in full: what it was, then every line it wrote.
pub fn print_run_detail(path: &Path, header: &RunHeader, entries: &[JournalEntry]) {
    println!("\n═══ Run {} ═══", header.run_id);
    println!("  {:<LABEL_WIDTH$}{}", "Started:", header.started_at);
    println!("  {:<LABEL_WIDTH$}{}", "mmm version:", header.mmm_version);
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Library:",
        header.output_dir.display()
    );
    println!(
        "  {:<LABEL_WIDTH$}{}",
        "Schema version:", header.schema_version
    );
    println!("  {:<LABEL_WIDTH$}{}", "Command:", header.argv.join(" "));
    println!("  {JOURNAL_LABEL:<LABEL_WIDTH$}{}", path.display());

    if entries.is_empty() {
        println!("\nThis run recorded no operations.");
        return;
    }

    println!("\n─── Entries ───\n");
    for entry in entries {
        println!("  {}", describe_entry(entry));
    }

    if !entries
        .iter()
        .any(|e| matches!(e, JournalEntry::RunCompleted { .. }))
    {
        println!("\n{INTERRUPTED_RUN_NOTICE}");
    }
}

/// One journal line, as a person reads it.
fn describe_entry(entry: &JournalEntry) -> String {
    match entry {
        JournalEntry::MoveIntent {
            seq,
            source,
            destination,
            source_size,
            kind,
            ..
        } => format!(
            "[{seq:>5}] intent    {:?}  {} → {}  ({source_size} bytes)",
            kind,
            source.display(),
            destination.display()
        ),
        JournalEntry::MoveCommitted {
            seq,
            final_destination,
            move_kind,
        } => format!(
            "[{seq:>5}] committed {}  ({move_kind})",
            final_destination.display()
        ),
        JournalEntry::MoveFailed { seq, reason } => format!("[{seq:>5}] FAILED    {reason}"),
        JournalEntry::DuplicateMoved {
            seq,
            group,
            source,
            destination,
        } => format!(
            "[{seq:>5}] duplicate group {group:03}  {} → {}",
            source.display(),
            destination.display()
        ),
        JournalEntry::RunCompleted {
            moved,
            failed,
            skipped,
            ended_at,
        } => format!(
            "        completed at {ended_at}  (moved {moved}, failed {failed}, skipped {skipped})"
        ),
    }
}

/// Prompt the user to continue processing the next chunk
pub fn prompt_continue(chunk_number: usize, remaining: usize) -> bool {
    print!("\nProcessed chunk {chunk_number}. {remaining} files remaining. Continue? [Y/n] ");
    if io::stdout().flush().is_err() {
        // We could not even show the prompt, so we have no consent to continue.
        return false;
    }

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    let trimmed = input.trim().to_lowercase();
    trimmed.is_empty() || trimmed == "y" || trimmed == "yes"
}
