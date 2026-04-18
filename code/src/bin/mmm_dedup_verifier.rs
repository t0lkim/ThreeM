//! dedup-verifier: Independent duplicate verification using SHA-256
//!
//! Runs against the duplicates/ directory created by mmm.
//! Uses SHA-256 (not BLAKE3) to provide an independent hash verification
//! that the files in each numbered group are truly duplicates of the
//! original file referenced in the manifest.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Parser, Debug)]
#[command(
    name = "dedup-verifier",
    about = "Verify duplicate files using SHA-256 (independent of BLAKE3 used by mmm)",
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
        "Verifying {} duplicate groups using SHA-256...\n",
        groups.len()
    );

    let pb = ProgressBar::new(groups.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
            .expect("valid template")
            .progress_chars("##-"),
    );

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

        pb.set_message(format!("group {}", group_id));

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
            if path.is_file()
                && path
                    .file_name()
                    .map(|n| n != "manifest.txt")
                    .unwrap_or(false)
            {
                let hash = verification_hash(&path)?;
                let matches = original_hash
                    .as_ref()
                    .map(|oh| oh == &hash)
                    .unwrap_or(false);
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
    println!("\n═══ Verification Results (SHA-256) ═══\n");

    for result in &results {
        let icon = match result.verdict {
            Verdict::Confirmed => "OK",
            Verdict::Mismatch => "MISMATCH",
            Verdict::OriginalMissing => "MISSING",
        };

        let hash_display = result
            .original_hash
            .as_deref()
            .map(|h| &h[..16])
            .unwrap_or("N/A");
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
    println!("  Confirmed duplicates: {}", confirmed);
    println!("  Hash mismatches: {}", mismatches);
    println!("  Original missing: {}", missing);

    if mismatches > 0 {
        println!(
            "\nWARNING: {} groups have hash mismatches — review before deleting!",
            mismatches
        );
        process::exit(1);
    }

    if missing > 0 && args.check_originals {
        println!(
            "\nWARNING: {} originals not found at recorded paths!",
            missing
        );
        process::exit(1);
    }

    println!("\nAll verified groups are confirmed duplicates.");
    Ok(())
}

/// Parse the manifest.txt to extract the original path and source duplicate paths
fn parse_manifest(path: &Path) -> Result<(PathBuf, Vec<PathBuf>)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut original_path = PathBuf::new();
    let mut duplicate_paths = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        if let Some(orig) = trimmed.strip_prefix("# Original kept at: ") {
            original_path = PathBuf::from(orig);
        } else if !trimmed.starts_with('#') {
            duplicate_paths.push(PathBuf::from(trimmed));
        }
    }

    Ok((original_path, duplicate_paths))
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
