//! Integration suite for `mmm-dedup-verifier`, driven through the real binary.
//!
//! The verifier had no tests at all until v0.2.0 — recorded as a real gap in
//! `docs/research/coverage-report.md`, on the grounds that it is read-only and
//! cannot lose data. That reasoning covers *data* safety and not the only thing
//! anyone runs the verifier for, which is an answer they can act on: a
//! `[MISMATCH]` is the signal not to delete a duplicate, and a `[OK]` is
//! permission to. Nothing proved either verdict was reachable.
//!
//! What is asserted here:
//!
//! 1. **It names the hash it actually computes.** Every user-facing string said
//!    SHA-256 until v0.2.0 while [`blake3::Hasher::new_keyed`] had always been
//!    the implementation. The whole argument for running a second tool is that
//!    it does not share the first one's failure modes, and a label naming an
//!    algorithm the binary does not use makes that argument unauditable. The
//!    test asserts the correct name *and* the absence of the wrong one, because
//!    a partial rename would otherwise pass.
//! 2. **The three verdicts are reachable**, over a manifest in the format
//!    `organiser.rs` actually writes — genuine copy, altered copy, absent
//!    original — with the documented exit codes.
//! 3. **It parses the manifest `organiser.rs` writes**, outcome comment lines
//!    and all. Nothing else pins the two halves of that format together.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

/// A `duplicates/NNN/` group laid out the way a committing run leaves it.
///
/// The header is copied from `organiser::GroupManifest` rather than reduced to
/// the two lines the verifier reads, so that a change to the written format
/// which the verifier cannot parse fails here instead of in somebody's library.
fn write_group(
    duplicates: &Path,
    index: usize,
    original: &Path,
    original_bytes: &[u8],
    copies: &[(&str, &[u8])],
) -> PathBuf {
    let group = duplicates.join(format!("{index:03}"));
    fs::create_dir_all(&group).unwrap();

    let mut manifest = format!(
        "# Duplicate group {index:03}\n\
         # BLAKE3 hash: {}\n\
         # File size: {} bytes\n\
         # Original kept at: {}\n\
         # Duplicates intended for this directory: {}\n\
         #\n\
         # The paths below are written before the first move, so an\n\
         # interrupted run still records where every file came from.\n\
         # Outcomes follow, appended one line at a time as each move ends.\n\n",
        blake3::hash(original_bytes).to_hex(),
        original_bytes.len(),
        original.display(),
        copies.len(),
    );

    for (name, _) in copies {
        writeln!(manifest, "/somewhere/original/{name}").unwrap();
    }
    manifest.push_str("\n# Outcomes\n");
    for (name, _) in copies {
        writeln!(
            manifest,
            "# moved: /somewhere/original/{name} -> {}",
            group.join(name).display()
        )
        .unwrap();
    }

    fs::write(group.join("manifest.txt"), manifest).unwrap();
    for (name, bytes) in copies {
        fs::write(group.join(name), bytes).unwrap();
    }

    group
}

/// A library with one genuine duplicate group, and the verifier's view of it.
fn verify(duplicates: &Path, extra_args: &[&str]) -> (bool, String) {
    let output = Command::cargo_bin("mmm-dedup-verifier")
        .unwrap()
        .arg(duplicates)
        .args(extra_args)
        .output()
        .unwrap();

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Property 1: the tool names the hash it computes, and does not name the one
/// it does not.
#[test]
fn reports_the_hash_it_actually_computes() {
    let tmp = TempDir::new().unwrap();
    let original = tmp.path().join("kept.jpg");
    fs::write(&original, b"the photograph").unwrap();

    let duplicates = tmp.path().join("duplicates");
    write_group(
        &duplicates,
        0,
        &original,
        b"the photograph",
        &[("copy.jpg", b"the photograph")],
    );

    let (ok, stdout) = verify(&duplicates, &[]);

    assert!(ok, "a genuine duplicate group must exit 0:\n{stdout}");
    assert!(
        stdout.contains("keyed BLAKE3"),
        "the run must name the hash it computes:\n{stdout}"
    );
    // The absence matters as much as the presence: three separate strings said
    // SHA-256, and renaming two of them would satisfy the assertion above.
    assert!(
        !stdout.contains("SHA-256") && !stdout.contains("SHA256"),
        "the run must not name a hash it does not compute:\n{stdout}"
    );
}

/// The `--help` text is a fourth place the algorithm was named, and the one a
/// user reads before deciding whether the second opinion is worth having.
#[test]
fn help_names_the_hash_it_actually_computes() {
    let output = Command::cargo_bin("mmm-dedup-verifier")
        .unwrap()
        .arg("--help")
        .output()
        .unwrap();

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("BLAKE3"), "--help must name BLAKE3:\n{help}");
    assert!(
        !help.contains("SHA-256"),
        "--help must not name SHA-256:\n{help}"
    );
    assert!(
        help.contains("mmm-dedup-verifier"),
        "--help must name the binary as it is installed:\n{help}"
    );
}

/// Property 2, the verdict that stops a deletion: a file in the group directory
/// whose contents differ from the original is reported, named, and fails the
/// run.
#[test]
fn an_altered_copy_is_a_mismatch_and_exits_non_zero() {
    let tmp = TempDir::new().unwrap();
    let original = tmp.path().join("kept.jpg");
    fs::write(&original, b"the photograph").unwrap();

    let duplicates = tmp.path().join("duplicates");
    write_group(
        &duplicates,
        0,
        &original,
        b"the photograph",
        &[("genuine.jpg", b"the photograph")],
    );
    write_group(
        &duplicates,
        1,
        &original,
        b"the photograph",
        // Same length, different bytes — the case a size comparison would pass.
        &[("altered.jpg", b"the photOgraph")],
    );

    let (ok, stdout) = verify(&duplicates, &[]);

    assert!(!ok, "a mismatch must exit non-zero:\n{stdout}");
    assert!(stdout.contains("[OK] Group 000"), "{stdout}");
    assert!(stdout.contains("[MISMATCH] Group 001"), "{stdout}");
    assert!(
        stdout.contains("altered.jpg"),
        "the mismatching file must be named:\n{stdout}"
    );
    assert!(stdout.contains("Hash mismatches: 1"), "{stdout}");
}

/// Property 2, the third verdict: an original that is no longer where the
/// manifest says fails the run, with or without `--check-originals`.
///
/// **This changed in 0.2.2.** It used to fail only under `--check-originals`,
/// which meant the default invocation — the one somebody makes before deleting
/// a `duplicates/` directory — printed "All verified groups are confirmed
/// duplicates" and exited 0 having confirmed nothing at all. A group whose
/// original cannot be found was not checked against anything, and a tool whose
/// entire purpose is confirming-before-deleting must not call that an
/// all-clear. The flag is kept so existing invocations still parse; it no
/// longer changes the outcome.
#[test]
fn a_missing_original_fails_the_run() {
    let tmp = TempDir::new().unwrap();
    let original = tmp.path().join("deleted-since.jpg");

    let duplicates = tmp.path().join("duplicates");
    write_group(
        &duplicates,
        0,
        &original,
        b"the photograph",
        &[("copy.jpg", b"the photograph")],
    );

    let (ok, stdout) = verify(&duplicates, &[]);
    assert!(
        !ok,
        "a missing original means nothing was verified, so the run must fail:\n{stdout}"
    );
    assert!(stdout.contains("[MISSING] Group 000"), "{stdout}");
    assert!(stdout.contains("Original missing: 1"), "{stdout}");
    assert!(
        stdout.contains("NOT verified"),
        "the run must say plainly that the group went unverified:\n{stdout}"
    );

    // The flag is accepted and the verdict is the same.
    let (ok, _stdout) = verify(&duplicates, &["--check-originals"]);
    assert!(!ok, "--check-originals must not soften the verdict");
}

/// A group directory with no manifest is skipped with a warning rather than
/// counted as verified — the documented behaviour, and the one that matters
/// because the alternative is reporting a group as `[OK]` on no evidence.
#[test]
fn a_group_without_a_manifest_is_skipped_not_confirmed() {
    let tmp = TempDir::new().unwrap();
    let duplicates = tmp.path().join("duplicates");
    fs::create_dir_all(duplicates.join("000")).unwrap();
    fs::write(duplicates.join("000").join("orphan.jpg"), b"unaccounted").unwrap();

    let (ok, stdout) = verify(&duplicates, &[]);

    assert!(
        stdout.contains("Groups verified: 0"),
        "a group with no manifest must not be counted as verified:\n{stdout}"
    );
    // **Changed in 0.2.2.** This used to exit 0. A `duplicates/` directory
    // holding files that were never checked is exactly the state in which an
    // all-clear is most dangerous — the operator is about to delete them.
    assert!(
        !ok,
        "files sit in duplicates/ that nothing verified, so this is not a \
         success:\n{stdout}"
    );
}

/// An empty `duplicates/` directory is the ordinary outcome of a library with
/// no duplicates in it, and must not read as a failure.
#[test]
fn no_groups_is_not_a_failure() {
    let tmp = TempDir::new().unwrap();
    let duplicates = tmp.path().join("duplicates");
    fs::create_dir_all(&duplicates).unwrap();

    let (ok, stdout) = verify(&duplicates, &[]);

    assert!(ok, "{stdout}");
    assert!(stdout.contains("No duplicate groups found"), "{stdout}");
}
