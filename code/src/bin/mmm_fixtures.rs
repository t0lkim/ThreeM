//! `mmm-fixtures`: build a synthetic photo library to try `mmm` on.
//!
//! The problem this solves is that the first thing anybody sensible does with a
//! tool that moves photographs is refuse to point it at their photographs. That
//! is the correct instinct, and it leaves them with nowhere to start. This
//! generates a library that is safe to be wrong about — a few hundred
//! byte-valid images and videos with real EXIF, plus an `EXPECTED.md` stating
//! where each one should end up — so the first run happens over files nobody
//! cares about and the result can actually be checked.
//!
//! The same machinery builds the fixtures for this project's own test suite.
//! What ships here is not a demo of the tests; it is the tests' own inputs,
//! handed over so the claims in the README can be verified rather than taken on
//! trust.

use std::path::{Path, PathBuf};
use std::process;

use anyhow::{bail, Context, Result};
use clap::Parser;

use mmm::fixtures::MediaTree;
use mmm::generate::{expected_markdown, generate, Profile};

#[derive(Parser, Debug)]
#[command(
    name = "mmm-fixtures",
    about = "Generate a synthetic photo library, with a written statement of what mmm should do \
             with it",
    long_about = "Generates byte-valid images and videos with real EXIF — including the \
                  malformed and ambiguous files that have historically broken this tool — and \
                  writes EXPECTED.md alongside them saying where each file should end up and \
                  why.\n\nEverything is reproducible from the seed, which is printed on every \
                  run: a bug report citing a profile and a seed is a complete reproduction.\n\n\
                  These are throwaway files. Nothing here is a real photograph and none of it is \
                  worth keeping.",
    version
)]
struct Args {
    /// Where to build the library. Created if it does not exist; must be empty
    /// unless --force is given.
    #[arg(required = true)]
    directory: PathBuf,

    /// Which library to build: minimal, realistic, awkward, or stress.
    #[arg(long, short, default_value = "realistic")]
    profile: String,

    /// Reproduce a previous library exactly. Omit for a fresh one — the seed
    /// chosen is printed either way.
    #[arg(long, short)]
    seed: Option<u64>,

    /// Write into a directory that already has files in it.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// List the profiles and what each is for, then exit.
    #[arg(long, default_value_t = false)]
    list_profiles: bool,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("mmm-fixtures: {e:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();

    if args.list_profiles {
        for name in Profile::ALL {
            // `ALL` lists only names `parse` accepts — asserted by a unit test —
            // so the `else` is unreachable rather than a case to handle.
            if let Some(profile) = Profile::parse(name) {
                println!("  {name:<10} {}", profile.summary());
            }
        }
        return Ok(());
    }

    let profile = Profile::parse(&args.profile).with_context(|| {
        format!(
            "unknown profile {:?} — expected one of: {}",
            args.profile,
            Profile::ALL.join(", ")
        )
    })?;

    let seed = args.seed.unwrap_or_else(fresh_seed);

    prepare_directory(&args.directory, args.force)?;

    let tree = MediaTree::at(&args.directory)
        .with_context(|| format!("creating {}", args.directory.display()))?;
    let (tree, plan) = generate(tree, profile, seed);

    let expected = tree.path().join("EXPECTED.md");
    std::fs::write(&expected, expected_markdown(&plan))
        .with_context(|| format!("writing {}", expected.display()))?;

    // `MediaTree::at` does not own the directory, so nothing is swept away when
    // this returns — but say so out loud rather than relying on the reader
    // knowing that.
    drop(tree);

    report(&args.directory, profile, seed, plan.len());
    Ok(())
}

/// Refuse to scatter files through a directory that already has something in
/// it.
///
/// This tool writes several hundred files with plausible camera names into
/// whatever path it is handed. Pointed at a real photo directory by a slip of
/// the keyboard, it would leave the owner unable to tell their photographs from
/// the synthetic ones — a mess that `mmm undo` cannot help with, because
/// nothing moved. So the default is to refuse, and `--force` is the way to say
/// you meant it.
fn prepare_directory(dir: &Path, force: bool) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(());
    }

    if !dir.is_dir() {
        bail!("{} exists and is not a directory", dir.display());
    }

    if force {
        return Ok(());
    }

    let mut existing = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .peekable();

    if existing.peek().is_some() {
        bail!(
            "{} is not empty.\n\nThis writes several hundred files with camera-style names. \
             Into a directory that already holds photographs, that is a mess nothing can \
             untangle afterwards — mmm undo cannot help, because nothing was moved.\n\n\
             Pick an empty directory, or pass --force if you meant this one.",
            dir.display()
        );
    }

    Ok(())
}

/// A seed with no dependency on the system RNG: the low bits of the wall clock,
/// which is plenty for "give me a different library this time" and is printed
/// immediately so it can be pinned.
fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x5EED, |d| u64::try_from(d.as_nanos()).unwrap_or(0x5EED))
}

fn report(dir: &Path, profile: Profile, seed: u64, files: usize) {
    let dir = dir.display();
    println!("Built {files} files in {dir}");
    println!("  profile  {profile} — {}", profile.summary());
    println!("  seed     {seed}   (--seed {seed} rebuilds this exact library)");
    println!();
    println!("What should happen to each file is written in {dir}/EXPECTED.md.");
    println!();
    // `--timezone UTC` matches what EXPECTED.md tells the reader to run, and
    // for the same reason: the dated fixtures carry an explicit `+00:00`, so a
    // run in the machine's own zone files them somewhere else and every table
    // in the document reads as a failure when nothing failed. Printing one pair
    // of commands here and a different pair in the document is how a user ends
    // up reporting a correct run as a bug.
    println!("Try it:");
    println!("  mmm {dir} -o /tmp/mmm-demo --timezone UTC            # preview — moves nothing");
    println!("  mmm {dir} -o /tmp/mmm-demo --timezone UTC --commit   # do it");
    println!("  mmm undo /tmp/mmm-demo --commit                      # put it all back");

    if profile == Profile::Awkward {
        println!();
        println!(
            "This profile is deliberately malformed. Warnings and files in unsorted/ are the \
             correct result — EXPECTED.md marks every one."
        );
    }
}
