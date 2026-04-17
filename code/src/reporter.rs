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
    println!("  Date from EXIF: {}", exif_count);
    println!("  Date from filesystem: {}", fs_count);
    println!("  No date (unsorted): {}", no_date_count);
    println!("  With GPS location: {}", with_location);
}

/// Print the final summary after processing
pub fn print_summary(
    total_scanned: usize,
    total_moved: usize,
    duplicate_groups: usize,
    duplicate_files: usize,
    errors: usize,
) {
    println!("\n═══ Processing Complete ═══");
    println!("  Files scanned:      {}", total_scanned);
    println!("  Files organised:    {}", total_moved);
    println!("  Duplicate groups:   {}", duplicate_groups);
    println!("  Duplicate files:    {}", duplicate_files);
    if errors > 0 {
        println!("  Errors:             {}", errors);
    }
    println!("═══════════════════════════\n");
}

/// Prompt the user to continue processing the next chunk
pub fn prompt_continue(chunk_number: usize, remaining: usize) -> bool {
    print!(
        "\nProcessed chunk {}. {} files remaining. Continue? [Y/n] ",
        chunk_number, remaining
    );
    io::stdout().flush().expect("flush stdout");

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    let trimmed = input.trim().to_lowercase();
    trimmed.is_empty() || trimmed == "y" || trimmed == "yes"
}
