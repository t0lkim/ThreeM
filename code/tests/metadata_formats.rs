//! Integration suite for metadata correctness across formats, driven through
//! the real `mmm` binary.
//!
//! Right now that means timezones. A camera writes `2024:03:15 23:30:00` and
//! nothing else — the digits it displayed, with no zone attached — and what the
//! tool does with those digits decides which directory somebody's evening
//! photographs end up in. Reading them as UTC, which is what `and_utc()` does,
//! shifts the recorded time by the machine's own distance from Greenwich and
//! files the photograph under a day the photographer never saw.
//!
//! ## Why the binary and not the library
//!
//! `metadata.rs` and `timezone.rs` already unit-test the resolution order
//! against constructed values, and `Reading::resolve` is thoroughly covered
//! there. None of that establishes what this suite is for: that a real JPEG's
//! `OffsetTimeOriginal` bytes reach the resolver, that `--timezone` on the
//! command line reaches the policy, and that the resulting wall clock reaches
//! the *filename and directory* rather than being resolved correctly and then
//! discarded. Those are four separate wirings between the parser and the path,
//! and only a run through `main` crosses all of them.
//!
//! ## The assertion is the whole path, not the directory
//!
//! Every test below pins `<date-dir>/<filename>`, because the two fail
//! differently and the pair is what makes the suite complete. A UTC-shifted
//! evening photograph keeps its directory and loses its *hour*
//! (`2024-03-15-153000.jpg` for a picture taken at half past eleven at night);
//! a UTC-shifted photograph taken just after midnight loses its *day*. Asserting
//! only the directory would miss the first; asserting only the filename would
//! read as an off-by-a-few-hours cosmetic defect rather than the misfiling it is.
//!
//! ## The environment is an input, so it is controlled
//!
//! Every command runs `--no-config` with the inherited `MMM_` variables
//! stripped. Without that, a developer's own `default_timezone` — in
//! `~/.config/mmm/config.toml` or exported in the shell — would silently
//! outrank what these tests are asserting about, and
//! [`no_configuration_still_files_by_the_wall_clock_and_says_so`] in particular
//! would report `config` where it expects the machine's own zone.
//!
//! What is *not* controlled is the machine's timezone itself. It cannot be:
//! `TZ` is read by `chrono::Local` at first use, and the whole point of the
//! fallback test is what happens on a machine that has one. That test therefore
//! asserts the properties that hold on every machine — where the file lands, and
//! that the run says which zone it assumed — and asserts nothing about *which*
//! zone that is. See [`FALLBACK_TAGS`].

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

use common::{file_contents_by_marker, naive, MediaTree};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The timezone tags a run with nothing configured may print.
///
/// `system` on a machine whose zone resolves, `utc` where the wall clock being
/// read falls in a daylight-saving gap and so never occurred. Both are honest;
/// what neither may be is `exif` (the file said nothing) or `config` (nor did
/// the run).
const FALLBACK_TAGS: [&str; 2] = ["[tz:system]", "[tz:utc]"];

/// A `mmm` command with the configuration layers held out of the way.
fn mmm(input: &Path) -> Command {
    let mut cmd = Command::cargo_bin("mmm").expect("locating the mmm binary");
    cmd.arg(input).arg("--no-config");
    for (key, _) in std::env::vars() {
        if key.starts_with("MMM_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// Organise `input` into a fresh output directory, returning where each marked
/// fixture landed, relative to that directory.
///
/// `extra` is appended to the command line — `["--timezone", "+08:00"]`, or
/// nothing at all.
///
/// This *moves* the fixtures out of `input`, so a preview of the same tree has
/// to be taken before this is called, not after. A second run would scan an
/// empty directory and assert nothing.
fn organise(
    input: &Path,
    extra: &[&str],
) -> (TempDir, std::collections::BTreeMap<String, Vec<String>>) {
    let out_dir = TempDir::new().expect("creating output TempDir");
    let output = out_dir.path().join("out");

    let result = mmm(input)
        .arg("-o")
        .arg(&output)
        .arg("--commit")
        .arg("--no-prompt")
        .args(extra)
        .output()
        .expect("running mmm in commit mode");

    assert!(
        result.status.success(),
        "mmm {extra:?} exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        result.status.code(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );

    let landed = file_contents_by_marker(&output);
    (out_dir, landed)
}

/// The dry-run listing for `input`, which is where the per-file timezone tag is
/// printed.
///
/// Previewing rather than committing on purpose: the tag is a decision *about*
/// the run, and a user has to be able to see it before agreeing to anything.
fn preview_listing(input: &Path, extra: &[&str]) -> String {
    let result = mmm(input)
        .args(extra)
        .output()
        .expect("running mmm in preview mode");

    assert!(
        result.status.success(),
        "mmm {extra:?} (preview) exited with {:?}\n--- stderr ---\n{}",
        result.status.code(),
        String::from_utf8_lossy(&result.stderr),
    );
    String::from_utf8_lossy(&result.stdout).into_owned()
}

/// Assert that the fixture declared at `marker` landed at exactly `expected`.
fn assert_landed_at(
    landed: &std::collections::BTreeMap<String, Vec<String>>,
    marker: &str,
    expected: &str,
) {
    assert_eq!(
        landed.get(marker).map(Vec::as_slice),
        Some([expected.to_string()].as_slice()),
        "{marker} did not land at {expected}; the whole tree was {landed:#?}"
    );
}

/// The single line of a dry-run listing that mentions `needle`.
fn listing_line(listing: &str, needle: &str) -> String {
    let mut lines = listing.lines().filter(|line| line.contains(needle));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("no listing line mentions {needle}:\n{listing}"))
        .to_string();
    assert!(
        lines.next().is_none(),
        "more than one listing line mentions {needle}:\n{listing}"
    );
    line
}

// ---------------------------------------------------------------------------
// A file that recorded its own offset
// ---------------------------------------------------------------------------

/// The defect in its original form: half past eleven at night, filed under half
/// past eleven at night.
///
/// The file carries `+08:00`, so a UTC reading would put it at 15:30 — same
/// directory, wrong filename, and a photograph that claims to have been taken
/// in the afternoon.
#[test]
fn an_offset_tag_files_an_evening_photograph_under_its_own_wall_clock() {
    let tree = MediaTree::new().jpeg_with_offset(
        "evening.jpg",
        naive(2024, 3, 15, 23, 30, 0),
        Some("+08:00"),
        None,
    );

    let (_out, landed) = organise(tree.path(), &[]);
    assert_landed_at(&landed, "evening.jpg", "2024-03-15/2024-03-15-233000.jpg");
}

/// The same defect where it costs a *day* rather than an hour.
///
/// Half past midnight on the 16th in Singapore is half past four on the
/// afternoon of the 15th in UTC. This is the case the phase description names:
/// the photograph lands in a directory the photographer never had a picture
/// taken in.
#[test]
fn an_offset_tag_keeps_an_after_midnight_photograph_on_the_day_it_was_taken() {
    let tree = MediaTree::new().jpeg_with_offset(
        "just-after-midnight.jpg",
        naive(2024, 3, 16, 0, 30, 0),
        Some("+08:00"),
        None,
    );

    let (_out, landed) = organise(tree.path(), &[]);
    assert_landed_at(
        &landed,
        "just-after-midnight.jpg",
        "2024-03-16/2024-03-16-003000.jpg",
    );
}

/// And the run says the file was the one that knew.
#[test]
fn an_offset_tag_is_reported_as_the_files_own_testimony() {
    let tree = MediaTree::new().jpeg_with_offset(
        "evening.jpg",
        naive(2024, 3, 15, 23, 30, 0),
        Some("+08:00"),
        None,
    );

    let listing = preview_listing(tree.path(), &[]);
    let line = listing_line(&listing, "evening.jpg");
    assert!(
        line.contains("[tz:exif]"),
        "the offset tag was not reported as coming from the file: {line}"
    );
    assert!(
        listing.contains("Timezone recorded by the file: 1"),
        "the summary did not tally the file's own offset:\n{listing}"
    );
}

// ---------------------------------------------------------------------------
// A file that recorded no offset, and a run that was told which to assume
// ---------------------------------------------------------------------------

/// `--timezone` answers a bare wall clock — and, crucially, answers it *without
/// moving it*.
///
/// The photograph is filed under 23:30 on the 15th because that is what the
/// camera displayed. What `+08:00` decides is the instant the run records, not
/// the digits it files under; the assertion is the same path as the
/// offset-tagged case above, which is the point.
#[test]
fn a_configured_timezone_answers_a_file_that_carries_no_offset() {
    let tree =
        MediaTree::new().jpeg_with_offset("evening.jpg", naive(2024, 3, 15, 23, 30, 0), None, None);

    // Preview first: the commit run below moves the fixture out of the tree.
    let listing = preview_listing(tree.path(), &["--timezone", "+08:00"]);
    let line = listing_line(&listing, "evening.jpg");
    assert!(
        line.contains("[tz:config]"),
        "the configured zone was not reported as configured: {line}"
    );
    assert!(
        listing.contains("Timezone from default_timezone: 1"),
        "the summary did not tally the configured zone:\n{listing}"
    );

    let (_out, landed) = organise(tree.path(), &["--timezone", "+08:00"]);
    assert_landed_at(&landed, "evening.jpg", "2024-03-15/2024-03-15-233000.jpg");
}

/// A negative offset is passable in both spellings.
///
/// `--timezone -05:30` reads as an unknown flag `-0` under clap's defaults, so
/// the arm of the world that needs the flag most would have found only the
/// `=` spelling worked. `allow_hyphen_values` on the argument is what makes the
/// first of these two lines a valid invocation; this test is why it is set.
#[test]
fn a_negative_offset_is_accepted_with_or_without_an_equals_sign() {
    for spelling in [
        vec!["--timezone", "-05:30"],
        vec!["--timezone=-05:30"],
        vec!["--timezone", "America/Denver"],
    ] {
        let tree = MediaTree::new().jpeg_with_offset(
            "evening.jpg",
            naive(2024, 3, 15, 23, 30, 0),
            None,
            None,
        );

        let (_out, landed) = organise(tree.path(), &spelling);
        assert_eq!(
            landed.get("evening.jpg").map(Vec::as_slice),
            Some(["2024-03-15/2024-03-15-233000.jpg".to_string()].as_slice()),
            "mmm {spelling:?} did not file the photograph under its own wall clock"
        );
    }
}

/// An IANA name is accepted as well as a fixed offset, and resolves to the
/// zone's offset *at that reading* rather than to a constant.
///
/// Filing is unmoved either way, so what this proves is that
/// `--timezone Asia/Singapore` is not quietly rejected on the way to the policy
/// — the reason `chrono-tz` was added at all.
#[test]
fn a_configured_iana_zone_is_accepted_on_the_command_line() {
    let tree =
        MediaTree::new().jpeg_with_offset("evening.jpg", naive(2024, 3, 15, 23, 30, 0), None, None);

    let (_out, landed) = organise(tree.path(), &["--timezone", "Asia/Singapore"]);
    assert_landed_at(&landed, "evening.jpg", "2024-03-15/2024-03-15-233000.jpg");
}

/// Whatever the run is told, the wall clock does not move.
///
/// The load-bearing property stated as a test: the same fixture under five
/// zones spanning a day's width lands in exactly one place. If any of these
/// diverge, the tool has gone back to shifting people's photographs by their
/// distance from Greenwich — and a library organised on one machine would no
/// longer match the same library organised on another.
#[test]
fn the_configured_zone_never_moves_the_directory_a_file_is_filed_under() {
    for zone in [
        "+14:00",
        "+08:00",
        "UTC",
        "-05:30",
        "-11:00",
        "Pacific/Apia",
    ] {
        let tree = MediaTree::new().jpeg_with_offset(
            "evening.jpg",
            naive(2024, 3, 15, 23, 30, 0),
            None,
            None,
        );

        let (_out, landed) = organise(tree.path(), &["--timezone", zone]);
        assert_eq!(
            landed.get("evening.jpg").map(Vec::as_slice),
            Some(["2024-03-15/2024-03-15-233000.jpg".to_string()].as_slice()),
            "--timezone {zone} moved the wall clock the file was filed under"
        );
    }
}

// ---------------------------------------------------------------------------
// A file that recorded no offset, and a run that was told nothing
// ---------------------------------------------------------------------------

/// Nothing in the file, nothing on the command line — and the photograph still
/// lands under the digits the camera wrote, on any machine.
///
/// This is the case where the tool has to guess, and the two halves of "guess
/// well" are asserted separately: the guess does not affect *filing* (which is
/// why the destination is knowable in a test at all), and the run does not hide
/// that it guessed.
#[test]
fn no_configuration_still_files_by_the_wall_clock_and_says_so() {
    let tree =
        MediaTree::new().jpeg_with_offset("evening.jpg", naive(2024, 3, 15, 23, 30, 0), None, None);

    // Preview first: the commit run below moves the fixture out of the tree.
    let listing = preview_listing(tree.path(), &[]);
    let line = listing_line(&listing, "evening.jpg");
    assert!(
        FALLBACK_TAGS.iter().any(|tag| line.contains(tag)),
        "a run with nothing configured did not say which zone it fell back to: {line}"
    );
    assert!(
        !line.contains("[tz:exif]") && !line.contains("[tz:config]"),
        "the run claimed a zone that neither the file nor the command line gave it: {line}"
    );

    let (_out, landed) = organise(tree.path(), &[]);
    assert_landed_at(&landed, "evening.jpg", "2024-03-15/2024-03-15-233000.jpg");
}

/// The same run, seen from the summary rather than the per-file listing.
///
/// Worth asserting separately because the summary is what somebody reads before
/// deciding whether to pass `--commit`, and "every date in this run came from
/// this machine's clock settings" is exactly the thing that ought to give them
/// pause.
#[test]
fn the_summary_tallies_a_run_that_had_to_assume_a_zone() {
    let tree = MediaTree::new()
        .jpeg_with_offset("evening.jpg", naive(2024, 3, 15, 23, 30, 0), None, None)
        .jpeg_with_offset("morning.jpg", naive(2024, 3, 16, 7, 5, 0), None, None);

    let listing = preview_listing(tree.path(), &[]);
    assert!(
        listing.contains("Timezone from this machine: 2")
            || listing.contains("Timezone assumed as UTC: 2"),
        "the summary did not tally two assumed zones:\n{listing}"
    );
    assert!(
        listing.contains("Timezone recorded by the file: 0"),
        "the summary claimed a file recorded its own zone when none did:\n{listing}"
    );
}
