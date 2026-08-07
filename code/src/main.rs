use anyhow::Result;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{error, info};

use mmm::{hasher, organiser, reporter, scanner};

use mmm::config::Config;
use mmm::geocoder::GeoLookup;
use mmm::organiser::ChunkController;

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
        reporter::print_summary(&summary);
        println!("{}", reporter::DRY_RUN_BANNER);
        return Ok(());
    }

    // === Move duplicates to duplicates/ directory ===
    let (_dup_moved, dup_errors) = if dedup_result.duplicate_groups.is_empty() {
        (0, 0)
    } else {
        println!("\nMoving duplicates to duplicates/ directory...");
        let (dm, de) = organiser::move_duplicates(&dedup_result.duplicate_groups, output_dir)?;
        println!("  Moved {dm} duplicate files ({de} errors)");
        (dm, de)
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
    let run = organiser::process_moves(&planned_moves, config.chunk_size, &mut controller);

    // A run the operator stopped did not complete, and the bar must not claim
    // it did.
    if run.stopped_early {
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

    // Printed on every path out of the move phase, including the early stop —
    // the operator who just interrupted a run is precisely the one who needs
    // to be told what it managed first.
    reporter::print_summary(&reporter::RunSummary {
        organised: run.moved,
        unprocessed: run.unprocessed,
        errors: plan_errors + run.errors + dup_errors,
        ..summary
    });

    Ok(())
}
