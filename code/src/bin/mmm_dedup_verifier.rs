//! `mmm-dedup-verifier`: independent duplicate verification using keyed BLAKE3.
//!
//! Runs against the duplicates/ directory created by mmm.
//! Uses BLAKE3 in **keyed** mode, always over the whole file, to verify
//! independently of the main binary's unkeyed three-phase cascade that the
//! files in each numbered group are truly duplicates of the original file
//! referenced in the manifest. See [`verification_hash`] for what "independent"
//! buys.
//!
//! Every string here said SHA-256 until v0.2.0, in the module docs, the `--help`
//! text and three lines of output, while [`verification_hash`] had always been
//! keyed BLAKE3. A verification tool that names the wrong algorithm is worse
//! than one that names none: the whole reason to run it is that it does not
//! share the main binary's failure modes, and that claim is unauditable if the
//! label is wrong.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::ProgressBar;

#[derive(Parser, Debug)]
#[command(
    name = "mmm-dedup-verifier",
    about = "Verify duplicate files using keyed BLAKE3 (independent of the unkeyed cascade mmm \
             uses)",
    version
)]
struct Args {
    /// Path to the duplicates/ directory created by mmm
    #[arg(required = true)]
    duplicates_dir: PathBuf,

    /// Also verify that originals still exist at their recorded paths
    #[arg(long, default_value_t = false)]
    check_originals: bool,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug)]
struct VerificationResult {
    group_id: String,
    original_path: PathBuf,
    original_hash: Option<String>,
    duplicates: Vec<DuplicateCheck>,
    verdict: Verdict,
}

#[derive(Debug)]
struct DuplicateCheck {
    path: PathBuf,
    hash: String,
    matches_original: bool,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Confirmed,
    Mismatch,
    OriginalMissing,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let filter = match args.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    if !args.duplicates_dir.is_dir() {
        eprintln!(
            "Error: {} is not a directory",
            args.duplicates_dir.display()
        );
        process::exit(1);
    }

    // Find all numbered group directories
    let mut groups: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&args.duplicates_dir)
        .with_context(|| format!("reading {}", args.duplicates_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            groups.push(entry.path());
        }
    }
    groups.sort();

    if groups.is_empty() {
        println!(
            "No duplicate groups found in {}",
            args.duplicates_dir.display()
        );
        return Ok(());
    }

    println!(
        "Verifying {} duplicate groups using keyed BLAKE3...\n",
        groups.len()
    );

    let pb = ProgressBar::new(groups.len() as u64);
    pb.set_style(mmm::hasher::styled_bar(
        "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}",
    ));

    let mut results: Vec<VerificationResult> = Vec::new();
    let mut confirmed = 0;
    let mut mismatches = 0;
    let mut missing = 0;

    for group_dir in &groups {
        let group_id = group_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        pb.set_message(format!("group {group_id}"));

        let manifest_path = group_dir.join("manifest.txt");
        if !manifest_path.exists() {
            eprintln!("  Warning: no manifest.txt in {}", group_dir.display());
            pb.inc(1);
            continue;
        }

        let (original_path, _duplicate_source_paths) = parse_manifest(&manifest_path)?;

        // Hash the original (if it exists)
        let original_hash = if original_path.exists() {
            Some(verification_hash(&original_path)?)
        } else {
            None
        };

        // Hash each duplicate file in this group directory
        let mut duplicate_checks = Vec::new();
        for entry in fs::read_dir(group_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.file_name().is_some_and(|n| n != "manifest.txt") {
                let hash = verification_hash(&path)?;
                let matches = original_hash.as_ref().is_some_and(|oh| oh == &hash);
                duplicate_checks.push(DuplicateCheck {
                    path,
                    hash,
                    matches_original: matches,
                });
            }
        }

        let verdict = if original_hash.is_none() {
            missing += 1;
            Verdict::OriginalMissing
        } else if duplicate_checks.iter().all(|d| d.matches_original) {
            confirmed += 1;
            Verdict::Confirmed
        } else {
            mismatches += 1;
            Verdict::Mismatch
        };

        results.push(VerificationResult {
            group_id,
            original_path,
            original_hash,
            duplicates: duplicate_checks,
            verdict,
        });

        pb.inc(1);
    }

    pb.finish_with_message("verification complete");

    // Print results
    println!("\n═══ Verification Results (keyed BLAKE3) ═══\n");

    for result in &results {
        let icon = match result.verdict {
            Verdict::Confirmed => "OK",
            Verdict::Mismatch => "MISMATCH",
            Verdict::OriginalMissing => "MISSING",
        };

        let hash_display = result.original_hash.as_deref().map_or("N/A", |h| &h[..16]);
        println!(
            "  [{}] Group {}: {} ({} duplicates, hash: {}...)",
            icon,
            result.group_id,
            result.original_path.display(),
            result.duplicates.len(),
            hash_display
        );

        if result.verdict == Verdict::Mismatch {
            for dup in &result.duplicates {
                if !dup.matches_original {
                    println!(
                        "    MISMATCH: {} (hash: {}...)",
                        dup.path.display(),
                        &dup.hash[..16]
                    );
                }
            }
        }
    }

    println!("\n═══ Summary ═══");
    println!("  Groups verified: {}", results.len());
    println!("  Confirmed duplicates: {confirmed}");
    println!("  Hash mismatches: {mismatches}");
    println!("  Original missing: {missing}");

    if mismatches > 0 {
        println!("\nWARNING: {mismatches} groups have hash mismatches — review before deleting!");
        process::exit(1);
    }

    // A group whose original cannot be found was not verified against anything.
    // This used to be an error only under `--check-originals`, which meant the
    // default run — the one somebody makes before deleting a `duplicates/`
    // directory — printed an all-clear having confirmed nothing at all. The
    // flag is kept so existing invocations still parse; it no longer changes
    // the outcome.
    if missing > 0 {
        println!(
            "\nWARNING: {missing} originals were not found at their recorded paths, so those \
             groups were NOT verified. Nothing here should be deleted on the strength of this \
             run."
        );
        process::exit(1);
    }

    // Every remaining group is confirmed, so this is the only path on which the
    // all-clear is true. Reaching it with `confirmed == 0` would mean claiming
    // to have verified something while having verified nothing, which is the
    // defect this guard exists to make unreachable.
    if confirmed == 0 {
        println!(
            "\nNothing was verified: no group in {} could be checked. This is not an all-clear.",
            args.duplicates_dir.display()
        );
        process::exit(1);
    }

    println!("\nAll {confirmed} verified groups are confirmed duplicates.");
    Ok(())
}

/// Parse the manifest.txt to extract the original path and source duplicate paths
fn parse_manifest(path: &Path) -> Result<(PathBuf, Vec<PathBuf>)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut original_path = PathBuf::new();
    let mut moved_destination: Option<PathBuf> = None;
    let mut duplicate_paths = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        if let Some(moved_to) = trimmed.strip_prefix("# Original moved to: ") {
            // Written by the organise pass, which runs *after* the dedup pass
            // and therefore after `# Original kept at:` was recorded. It is the
            // path the file is actually at, so it wins — unconditionally, and
            // regardless of the order the two lines appear in.
            //
            // Without this the verifier resolved an input path the organise
            // pass had already emptied, found nothing, confirmed zero groups
            // and still reported an all-clear.
            moved_destination = Some(PathBuf::from(moved_to));
        } else if let Some(orig) = trimmed.strip_prefix("# Original kept at: ") {
            original_path = PathBuf::from(orig);
        } else if !trimmed.starts_with('#') {
            duplicate_paths.push(PathBuf::from(trimmed));
        }
    }

    Ok((moved_destination.unwrap_or(original_path), duplicate_paths))
}

/// Compute independent verification hash using BLAKE3 keyed mode
/// Intentionally different from main binary's approach:
/// - Main binary: BLAKE3 standard mode with 128KB buffer, three-phase cascade
/// - Verifier: BLAKE3 keyed mode with 256KB buffer, always full-file hash
fn verification_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;

    let mut hasher = blake3::Hasher::new_keyed(b"dedup-verifier-independent-key!!");
    let mut buf = [0u8; 256 * 1024];

    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}
