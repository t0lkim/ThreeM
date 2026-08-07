use std::io::{self, Write};

use crate::hasher::DuplicateGroup;
use crate::metadata::DateSource;
use crate::organiser::PlannedMove;

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
    pub errors: usize,
}

/// Column width of the summary labels, so every figure lines up.
const LABEL_WIDTH: usize = 20;

/// Label for entries the scan passed over. Exported so the integration suite
/// asserts against the string the binary actually prints.
pub const SCAN_SKIPPED_LABEL: &str = "Unreadable (scan):";

/// Label for files dropped from duplicate detection.
pub const HASH_SKIPPED_LABEL: &str = "Unhashable (dedup):";

/// Print the final summary after processing.
///
/// The skip lines appear only when something was skipped — a run that omitted
/// nothing should not invite the operator to look for what it omitted. When
/// they do appear they are unconditional: a file left out of the plan is
/// reported here or it is not reported at all.
pub fn print_summary(summary: &RunSummary) {
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
    if summary.errors > 0 {
        println!("  {:<LABEL_WIDTH$}{}", "Errors:", summary.errors);
    }
    println!("═══════════════════════════\n");
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
