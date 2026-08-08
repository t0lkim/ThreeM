use anyhow::{Context as _, Result};
use chrono::Utc;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{error, info};

use mmm::{hasher, journal, organiser, reporter, scanner};

use mmm::config::Config;
use mmm::geocoder::GeoLookup;
use mmm::journal::{Journal, JournalEntry, RunHeader};
use mmm::organiser::{ChunkController, MoveRecorder};
use mmm::reporter::JournalStatus;

/// Drives the chunked move phase from the terminal: the progress bar, and the
/// operator's answer at each chunk boundary.
///
/// Everything interactive lives here rather than in the library. The library's
/// job is to hand back what happened; deciding what that looks like on a
/// terminal — and whether to ask at all — is `main`'s.
struct CliController<'a> {
    bar: &'a ProgressBar,
    /// Whether to ask at chunk boundaries. `--no-prompt` answers yes silently.
    prompt: bool,
}

impl ChunkController for CliController<'_> {
    fn chunk_started(&mut self, chunk_number: usize, chunks: usize) {
        self.bar
            .set_message(format!("chunk {chunk_number}/{chunks}"));
    }

    fn file_finished(&mut self) {
        self.bar.inc(1);
    }

    fn should_continue(&mut self, chunk_number: usize, remaining: usize) -> bool {
        if !self.prompt {
            return true;
        }
        // `suspend` clears the bar for the duration of the prompt; the answer
        // travels back out as a value, which is the whole point — the old code
        // exited the process from inside this closure.
        self.bar
            .suspend(|| reporter::prompt_continue(chunk_number, remaining))
    }
}

/// Open this run's journal, or report that there will not be one.
///
/// Called only on the committing path, and deliberately not before: a preview
/// must leave no trace at all, and the surest way to guarantee that is for the
/// journal never to be created on that path rather than for something later to
/// remember not to write to it.
///
/// # Errors
///
/// Returns an error if the journal cannot be created. Nothing has moved at that
/// point, and nothing will: a run that cannot record what it is about to do
/// must not do it.
fn open_journal(config: &Config) -> Result<Option<Journal>> {
    let Some(dir) = config.resolve_journal_dir() else {
        println!();
        reporter::print_journal_location(JournalStatus::Disabled);
        return Ok(None);
    };

    let header = RunHeader::new(
        journal::generate_run_id(),
        config.output_dir(),
        RunHeader::current_argv(),
    );
    let journal = Journal::create(&dir, &header)
        .context("the run journal could not be created, so no files have been moved")?;

    // Printed here as well as in the summary: an interrupted run never reaches
    // its summary, and its operator is the one who most needs this path.
    println!();
    reporter::print_journal_location(JournalStatus::At(journal.path()));

    Ok(Some(journal))
}

/// Close the journal with the `RunCompleted` line, on every path out of a
/// committing run.
///
/// Best effort by design. The run is over and its counts are already on their
/// way to the operator; a journal that cannot take this last line is one whose
/// *missing* `RunCompleted` tells `undo` the run did not finish cleanly, which
/// is exactly what undo needs to know.
fn finish_journal(journal: Option<&mut Journal>, moved: usize, failed: usize, skipped: usize) {
    let Some(journal) = journal else { return };

    if let Err(e) = journal.append(&JournalEntry::RunCompleted {
        moved,
        failed,
        skipped,
        ended_at: Utc::now(),
    }) {
        error!(
            error = %format!("{e:#}"),
            "could not close the run journal; undo will treat this run as interrupted"
        );
    }
}

fn main() -> Result<()> {
    let config = Config::parse();

    // Initialise tracing
    let filter = match config.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    if let Some(notice) = config.deprecation_notice() {
        eprintln!("{notice}");
    }

    // Before the banner, before the scan: a run that is going to be refused
    // must not first tell the operator it is about to move their files.
    if let Err(refusal) = config.validate() {
        anyhow::bail!(refusal);
    }

    // Say which posture we are in before doing any work, not after — a user
    // who expected a preview must not learn otherwise from the aftermath.
    reporter::print_mode_banner(config.is_dry_run());

    info!("mmm v{}", env!("CARGO_PKG_VERSION"));
    info!(
        "scanning {} director{}",
        config.directories.len(),
        if config.directories.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );

    // === PHASE A: SCAN ===
    println!("Scanning directories...");

    let scan_spinner = ProgressBar::new_spinner();
    scan_spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    scan_spinner.set_message("discovering media files...");
    scan_spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let scanner::ScanResult {
        files,
        skipped: scan_skipped,
    } = scanner::scan_directories(&config.directories);
    scan_spinner.finish_with_message(format!("found {} media files", files.len()));

    if files.is_empty() {
        println!("No media files found in the specified directories.");
        if scan_skipped > 0 {
            // "Nothing here" and "we could not look" must never read the same.
            println!(
                "{scan_skipped} entr{} could not be read and {} skipped — see the warnings above.",
                if scan_skipped == 1 { "y" } else { "ies" },
                if scan_skipped == 1 { "was" } else { "were" }
            );
        }
        return Ok(());
    }

    // Dedup
    println!("\nAnalysing for duplicates...");
    let dedup_pb = hasher::hashing_progress_bar(files.len() as u64);
    let dedup_result = hasher::find_duplicates(&files, &dedup_pb);
    dedup_pb.finish_with_message("deduplication complete");

    // Report duplicates
    reporter::print_duplicates(&dedup_result.duplicate_groups);

    let total_duplicate_files: usize = dedup_result
        .duplicate_groups
        .iter()
        .map(|g| g.files.len() - 1)
        .sum();

    // Initialise reverse geocoder
    println!("\nLoading geocoding data...");
    let geo = GeoLookup::new();

    // Plan all moves
    println!("Planning file organisation...");
    let plan_pb = ProgressBar::new(dedup_result.unique.len() as u64);
    plan_pb.set_style(hasher::styled_bar(
        "[{elapsed_precise}] {bar:40.green/white} {pos}/{len} planning",
    ));

    let output_dir = config.output_dir();
    let mut planned_moves = Vec::new();
    let mut plan_errors = 0;

    for file in &dedup_result.unique {
        match organiser::plan_move(file, output_dir, &geo) {
            Ok(planned) => planned_moves.push(planned),
            Err(e) => {
                error!(path = %file.path.display(), error = %e, "failed to plan move");
                plan_errors += 1;
            }
        }
        plan_pb.inc(1);
    }
    plan_pb.finish_with_message("planning complete");

    // Every figure but `organised` is already known, so the two summaries
    // differ in exactly one field rather than in five positional arguments.
    let summary = reporter::RunSummary {
        scanned: files.len(),
        organised: 0,
        duplicate_groups: dedup_result.duplicate_groups.len(),
        duplicate_files: total_duplicate_files,
        scan_skipped,
        hash_skipped: dedup_result.skipped,
        unprocessed: 0,
        errors: plan_errors,
    };

    // === DRY RUN (the default): stop here, before anything is moved ===
    if config.is_dry_run() {
        reporter::print_dry_run(&planned_moves);
        // No journal, and none reported: a preview moves nothing, so there is
        // nothing to undo.
        reporter::print_summary(&summary, JournalStatus::NotNeeded);
        println!("{}", reporter::DRY_RUN_BANNER);
        return Ok(());
    }

    // === JOURNAL: opened before the first move, closed on every way out ===
    let mut journal = open_journal(&config)?;
    let journal_path = journal.as_ref().map(|j| j.path().to_path_buf());
    let journal_status = || match journal_path.as_deref() {
        Some(path) => JournalStatus::At(path),
        None => JournalStatus::Disabled,
    };

    // === Move duplicates to duplicates/ directory ===
    let (dup_moved, dup_errors) = if dedup_result.duplicate_groups.is_empty() {
        (0, 0)
    } else {
        println!("\nMoving duplicates to duplicates/ directory...");
        let mut recorder = MoveRecorder::new(journal.as_mut());
        match organiser::move_duplicates(&dedup_result.duplicate_groups, output_dir, &mut recorder)
        {
            Ok((dm, de)) => {
                println!("  Moved {dm} duplicate files ({de} errors)");
                (dm, de)
            }
            // Nothing in the organise pass has run yet, so every planned move
            // is untouched — but the journal still owes a closing line.
            Err(e) => {
                finish_journal(journal.as_mut(), 0, 0, planned_moves.len());
                return Err(e);
            }
        }
    };

    // === PHASE B: PROCESS (chunked) ===
    println!("\nOrganising files...");

    let move_pb = ProgressBar::new(planned_moves.len() as u64);
    move_pb.set_style(hasher::styled_bar(
        "[{elapsed_precise}] {bar:40.yellow/white} {pos}/{len} {msg}",
    ));

    let mut controller = CliController {
        bar: &move_pb,
        prompt: !config.no_prompt,
    };
    let mut recorder = MoveRecorder::new(journal.as_mut());
    let run = organiser::process_moves(
        &planned_moves,
        config.chunk_size,
        &mut controller,
        &mut recorder,
    );

    // A run that stopped did not complete, and the bar must not claim it did.
    if run.journal_failed {
        move_pb.abandon_with_message("journal write failed");
    } else if run.stopped_early {
        move_pb.abandon_with_message("stopped by user");
        println!(
            "\nStopped by user. {} file{} organised, {} left untouched.",
            run.moved,
            if run.moved == 1 { "" } else { "s" },
            run.unprocessed
        );
    } else {
        move_pb.finish_with_message("organisation complete");
    }

    // Every exit path from the move phase closes the journal, including the
    // early stop and the journal's own failure. `failed` counts moves that were
    // attempted and did not happen; `skipped` counts files never attempted —
    // those the run stopped before, plus those whose destination could not be
    // planned.
    finish_journal(
        journal.as_mut(),
        run.moved + dup_moved,
        run.errors + dup_errors,
        run.unprocessed + plan_errors,
    );

    // Printed on every path out of the move phase, including the early stop —
    // the operator who just interrupted a run is precisely the one who needs
    // to be told what it managed first.
    reporter::print_summary(
        &reporter::RunSummary {
            organised: run.moved,
            unprocessed: run.unprocessed,
            errors: plan_errors + run.errors + dup_errors,
            ..summary
        },
        journal_status(),
    );

    // A run that stopped because it could not record itself is a failed run,
    // and a script driving `mmm` has to be able to tell.
    if run.journal_failed {
        anyhow::bail!(
            "the run journal could not be written, so the run stopped after {} file{} — see the \
             errors above. Nothing further was moved.",
            run.moved,
            if run.moved == 1 { "" } else { "s" }
        );
    }

    Ok(())
}
