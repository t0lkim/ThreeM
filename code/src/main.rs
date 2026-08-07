use anyhow::Result;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{error, info};

use mmm::{hasher, organiser, reporter, scanner};

use mmm::config::Config;
use mmm::geocoder::GeoLookup;

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

    let files = scanner::scan_directories(&config.directories)?;
    scan_spinner.finish_with_message(format!("found {} media files", files.len()));

    if files.is_empty() {
        println!("No media files found in the specified directories.");
        return Ok(());
    }

    // Dedup
    println!("\nAnalysing for duplicates...");
    let dedup_pb = hasher::hashing_progress_bar(files.len() as u64);
    let dedup_result = hasher::find_duplicates(&files, &dedup_pb)?;
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

    // === DRY RUN (the default): stop here, before anything is moved ===
    if config.is_dry_run() {
        reporter::print_dry_run(&planned_moves);
        reporter::print_summary(
            dedup_result.unique.len() + total_duplicate_files,
            0,
            dedup_result.duplicate_groups.len(),
            total_duplicate_files,
            plan_errors,
        );
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
    let total = planned_moves.len();
    let chunks: Vec<&[organiser::PlannedMove]> = planned_moves.chunks(config.chunk_size).collect();
    let mut moved = 0;
    let mut move_errors = 0;

    let move_pb = ProgressBar::new(total as u64);
    move_pb.set_style(hasher::styled_bar(
        "[{elapsed_precise}] {bar:40.yellow/white} {pos}/{len} {msg}",
    ));

    for (i, chunk) in chunks.iter().enumerate() {
        move_pb.set_message(format!("chunk {}/{}", i + 1, chunks.len()));

        for planned in *chunk {
            match organiser::execute_move(planned) {
                Ok(()) => moved += 1,
                Err(e) => {
                    error!(
                        src = %planned.source.display(),
                        dst = %planned.destination.display(),
                        error = %e,
                        "move failed"
                    );
                    move_errors += 1;
                }
            }
            move_pb.inc(1);
        }

        // Prompt to continue (unless last chunk, no-prompt mode, or only one chunk)
        let remaining = total - (moved + move_errors);
        if remaining > 0 && !config.no_prompt && chunks.len() > 1 {
            move_pb.suspend(|| {
                if !reporter::prompt_continue(i + 1, remaining) {
                    println!("Stopped by user. {moved} files processed so far.");
                    std::process::exit(0);
                }
            });
        }
    }

    move_pb.finish_with_message("organisation complete");

    reporter::print_summary(
        dedup_result.unique.len() + total_duplicate_files,
        moved,
        dedup_result.duplicate_groups.len(),
        total_duplicate_files,
        plan_errors + move_errors + dup_errors,
    );

    Ok(())
}
