//! Integration suite for metadata correctness across formats.
//!
//! Two layers, and the split is deliberate. The **timezone** half drives the
//! real `mmm` binary, because what it is asserting is a chain of wirings that
//! only a whole run crosses. The **format-coverage** half calls
//! [`extract_metadata`] directly, because what it is asserting is narrower and
//! sharper: for each container family the scanner claims to accept, does the
//! date actually come out, and is it the date that went in? Routing that through
//! the binary would answer it only indirectly, through a filename.
//!
//! ## Timezones
//!
//! A camera writes `2024:03:15 23:30:00` and
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
use std::str::FromStr as _;

use assert_cmd::Command;
use tempfile::TempDir;

use common::{file_contents_by_marker, naive, MediaTree, VideoSpec};
use mmm::metadata::{extract_metadata, DateSource, FileMetadata};
use mmm::timezone::{Timezone, TimezonePolicy, TimezoneSource};

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
    preview_listing_with_env(input, extra, &[])
}

/// The same, with environment overrides applied on top of the stripped
/// environment.
///
/// The environment is the only layer below the command line these tests can
/// reach — the file layers are held off by `--no-config`, which is what makes
/// every other assertion in this suite independent of the machine. Setting a
/// variable here is therefore how a "a config layer said X, the command line
/// said Y" precedence claim gets tested at all.
fn preview_listing_with_env(input: &Path, extra: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = mmm(input);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let result = cmd
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

// ---------------------------------------------------------------------------
// Format coverage
// ---------------------------------------------------------------------------
//
// `IMAGE_EXTENSIONS` and `VIDEO_EXTENSIONS` between them name thirty-two
// extensions, and until these tests existed not one of them beyond `.jpg` had
// ever been shown to yield a date. That is a worse position than it sounds: an
// extension the scanner admits but the extractor cannot read does not fail, it
// *degrades* — silently, to the file's modification time, which on a library
// that has been copied between disks is the date of the copy. The tool reports
// no distinction, so a person organising ten thousand RAW files by "date taken"
// gets ten thousand files organised by something else entirely and no reason to
// doubt it.
//
// Each test below therefore asserts three things and not one: that the family
// parses at all, that the datetime is the one the fixture wrote, and — where a
// family does *not* parse — that the tool now says so out loud.

/// A fixture read as a still image with the run's zone under our control.
///
/// The policy is a fixed offset rather than the default, so nothing here
/// depends on the machine's own zone. Fixtures that carry their own offset tag
/// should never consult it; that they do not is part of what is asserted.
fn read_image(tree: &MediaTree, rel: &str) -> FileMetadata {
    read_with(tree, rel, false, "+08:00")
}

/// A fixture read as a video, likewise.
fn read_video(tree: &MediaTree, rel: &str, zone: &str) -> FileMetadata {
    read_with(tree, rel, true, zone)
}

fn read_with(tree: &MediaTree, rel: &str, is_video: bool, zone: &str) -> FileMetadata {
    let policy = TimezonePolicy::new(Some(
        Timezone::from_str(zone).unwrap_or_else(|e| panic!("{zone} is not a timezone: {e}")),
    ));
    extract_metadata(&tree.join(rel), is_video, &policy)
        .unwrap_or_else(|e| panic!("extracting metadata from fixture {rel}: {e}"))
}

/// The exact datetime, as the local wall clock plus offset the fixture declared.
fn assert_dated(meta: &FileMetadata, rel: &str, expected: &str) {
    assert_eq!(
        meta.date_source,
        DateSource::Exif,
        "{rel}: the date did not come from the file's own metadata"
    );
    assert_eq!(
        meta.date.map(|d| d.to_string()),
        Some(expected.to_string()),
        "{rel}: wrong datetime"
    );
}

// --- HEIF/HEIC ------------------------------------------------------------

/// The format every iPhone photograph has been in since 2017, and the one this
/// suite most needed to prove.
///
/// A HEIC keeps its EXIF in an item addressed by an offset table rather than in
/// a segment you meet by reading forwards, so "the EXIF parser works" does not
/// imply "HEIC works" — the container has to be walked first. GPS is asserted
/// alongside the date because it travels by the same indirection.
#[test]
fn a_heic_yields_its_exif_datetime_and_coordinates() {
    let tree = MediaTree::new().heic_with_exif(
        "IMG_0001.heic",
        naive(2024, 3, 15, 23, 30, 0),
        Some((48.8584, 2.2945)),
    );

    let meta = read_image(&tree, "IMG_0001.heic");
    assert_dated(&meta, "IMG_0001.heic", "2024-03-15 23:30:00 +00:00");
    assert_eq!(
        meta.timezone_source,
        Some(TimezoneSource::ExifOffsetTag),
        "the fixture writes an offset tag, so the run must report having read it"
    );

    let (lat, lon) = (
        meta.latitude.expect("latitude"),
        meta.longitude.expect("longitude"),
    );
    assert!(
        (lat - 48.8584).abs() < 0.0001 && (lon - 2.2945).abs() < 0.0001,
        "coordinates did not survive the HEIF item indirection: {lat}, {lon}"
    );
}

/// The same container under the brands real files actually carry.
///
/// A HEIC may announce itself as `heic` or as `mif1`, and an AVIF is the same
/// box structure with a different codec inside. Which brands are accepted is a
/// property of the parser, not of the EXIF, so it is worth pinning separately —
/// `.heif` and `.avif` are both in the scanner's extension list.
#[test]
fn the_heif_family_is_read_under_each_brand_the_scanner_claims() {
    let at = naive(2022, 7, 22, 21, 26, 32);
    let tree = MediaTree::new()
        .heif("brand-heic.heic", *b"heic", at, Some("+08:00"), None)
        .heif("brand-mif1.heif", *b"mif1", at, Some("+08:00"), None)
        .heif("brand-avif.avif", *b"avif", at, Some("+08:00"), None);

    for rel in ["brand-heic.heic", "brand-mif1.heif", "brand-avif.avif"] {
        let meta = read_image(&tree, rel);
        assert_dated(&meta, rel, "2022-07-22 21:26:32 +08:00");
    }
}

// --- TIFF-based RAW -------------------------------------------------------

/// The gap this phase set out to find, and the honest report of it.
///
/// DNG, NEF, ARW and CR2 are all TIFF underneath, with the date in an Exif
/// `SubIFD` exactly as a JPEG has it. `nom-exif` does not read a bare TIFF at
/// all — it recognises JPEG, HEIF, MOV and MP4 and nothing else — so every one
/// of these files falls back to its modification time. That is not fixable here
/// without a second parser; what *is* fixable, and is what this asserts, is that
/// the tool no longer passes the fallback off as an ordinary one.
#[test]
fn a_tiff_based_raw_is_reported_as_unsupported_rather_than_silently_degrading() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .tiff_raw("DSC_0001.nef", None, at, Some("+08:00"), None)
        .tiff_raw("DSC_0002.dng", None, at, Some("+08:00"), None)
        .tiff_raw("DSC_0003.arw", None, at, Some("+08:00"), None)
        .tiff_raw(
            "IMG_0004.cr2",
            Some(b"CR\x02\x00"),
            at,
            Some("+08:00"),
            None,
        );

    for rel in [
        "DSC_0001.nef",
        "DSC_0002.dng",
        "DSC_0003.arw",
        "IMG_0004.cr2",
    ] {
        let meta = read_image(&tree, rel);
        assert_eq!(
            meta.date_source,
            DateSource::Unsupported,
            "{rel}: a RAW file that cannot be parsed must say so, not report an \
             ordinary filesystem fallback"
        );
        assert!(
            meta.date.is_some(),
            "{rel}: the file still has to be organised somewhere"
        );
    }
}

/// The control that stops the test above being vacuous.
///
/// `Unsupported` would also be reported for a fixture that was simply malformed,
/// which would turn the RAW assertion into a test of the harness's own bugs. So:
/// the identical EXIF block, from the identical builder, is written into a JPEG
/// as well — and that one parses. The difference between the two files is the
/// container and nothing else, which is what makes "the container is the
/// blocker" a measurement rather than a story.
#[test]
fn the_raw_fixtures_carry_a_date_the_tool_would_read_in_any_other_container() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .tiff_raw("same.dng", None, at, Some("+08:00"), None)
        .jpeg_with_offset("same.jpg", at, Some("+08:00"), None);

    assert_dated(
        &read_image(&tree, "same.jpg"),
        "same.jpg",
        "2024-03-15 23:30:00 +08:00",
    );

    let raw = std::fs::read(tree.join("same.dng")).expect("reading the RAW fixture");
    assert_eq!(&raw[..4], b"II\x2a\x00", "the fixture is not a TIFF");
    assert!(
        raw.windows(19).any(|w| w == b"2024:03:15 23:30:00"),
        "the date is not in the RAW fixture at all, so its unreadability proves nothing"
    );
}

// --- MP4 / MOV ------------------------------------------------------------

/// A container creation time is a UTC instant, and is read as one.
///
/// `mvhd` counts seconds from 1904 and the specification pins that to UTC, so
/// unlike a naive EXIF stamp this date genuinely does move with the zone it is
/// read against — 15:30 UTC really is half past eleven at night in Singapore.
/// Both halves are asserted: the instant, which no zone can change, and the wall
/// clock, which every zone does.
#[test]
fn an_mp4_or_mov_container_clock_is_read_as_the_utc_instant_it_is() {
    let filmed = naive(2024, 3, 15, 15, 30, 0);
    let tree = MediaTree::new()
        .iso_video("clip.mp4", &VideoSpec::mp4(filmed))
        .iso_video("clip.mov", &VideoSpec::mov(filmed))
        // `.3gp` is in the scanner's list and takes the MP4 path, on a brand
        // that has to be spelled out separately to be accepted.
        .iso_video(
            "clip.3gp",
            &VideoSpec {
                brand: *b"3gp4",
                ..VideoSpec::mp4(filmed)
            },
        );

    for rel in ["clip.mp4", "clip.mov", "clip.3gp"] {
        let east = read_video(&tree, rel, "+08:00");
        assert_dated(&east, rel, "2024-03-15 23:30:00 +08:00");

        let west = read_video(&tree, rel, "America/Denver");
        assert_dated(&west, rel, "2024-03-15 09:30:00 -06:00");

        assert_eq!(
            east.date.map(|d| d.to_utc()),
            west.date.map(|d| d.to_utc()),
            "{rel}: the recorded instant moved when only the reading zone changed"
        );
    }
}

/// The defect this suite found, and its fix: an iPhone's own offset was being
/// thrown away.
///
/// `com.apple.quicktime.creationdate` is a string the phone writes with the
/// offset it was standing in, and it is the *only* thing in the file that knows
/// where that was. The extractor was converting it to an instant and re-reading
/// it against the run's zone, so the same video filed at half eleven at night in
/// Singapore and at half three in the afternoon in London — the phase's opening
/// defect, surviving in the video path.
///
/// The assertion is deliberately made under a policy of `America/Denver`: the
/// wall clock has to come out at 23:30+08:00 anyway, because the file said so
/// and the file outranks the configuration.
#[test]
fn an_apple_creationdate_is_believed_over_the_runs_own_timezone() {
    let tree = MediaTree::new().iso_video(
        "IMG_0007.mov",
        &VideoSpec {
            brand: *b"qt  ",
            // The two disagree by exactly the phone's distance from Greenwich,
            // as they do in every real iPhone video.
            mvhd_utc: Some(naive(2024, 3, 15, 15, 30, 0)),
            apple_creationdate: Some("2024-03-15T23:30:00+08:00"),
            location: Some("+48.8577+002.2950/"),
        },
    );

    let meta = read_video(&tree, "IMG_0007.mov", "America/Denver");
    assert_dated(&meta, "IMG_0007.mov", "2024-03-15 23:30:00 +08:00");
    assert_eq!(
        meta.timezone_source,
        Some(TimezoneSource::ExifOffsetTag),
        "the recording's own offset must be reported as the file's testimony, \
         not as a zone the run assumed"
    );

    let (lat, lon) = (
        meta.latitude.expect("latitude"),
        meta.longitude.expect("longitude"),
    );
    assert!(
        (lat - 48.8577).abs() < 0.0001 && (lon - 2.2950).abs() < 0.0001,
        "the ISO 6709 location did not survive: {lat}, {lon}"
    );
}

/// The other side of that rule: a video with no recording offset is still read
/// against the run's zone, because a container clock is all it has.
///
/// Worth its own test because the fix above works by noticing a *non-zero*
/// offset, and the obvious way to get that wrong is to start believing the
/// `+00:00` that `mvhd` is handed on the way out — which would file every video
/// ever made under a UTC wall clock.
#[test]
fn a_video_without_a_recording_offset_still_takes_the_runs_zone() {
    let tree =
        MediaTree::new().iso_video("clip.mp4", &VideoSpec::mp4(naive(2024, 3, 15, 15, 30, 0)));

    let meta = read_video(&tree, "clip.mp4", "Asia/Singapore");
    assert_dated(&meta, "clip.mp4", "2024-03-15 23:30:00 +08:00");
    assert_eq!(
        meta.timezone_source,
        Some(TimezoneSource::ConfiguredDefault),
        "a container clock says nothing about where the camera stood, so the run \
         has to admit the zone was its own choice"
    );
}

// --- The report -----------------------------------------------------------

/// End to end: an unreadable format is named on its own line and counted in the
/// summary, beside a file that worked.
///
/// The per-file tag and the tally are asserted together on purpose. A person
/// scanning three thousand lines will not notice one of them; a person reading
/// the summary will not learn *which* file; and the pair is what turns "some of
/// these dates are not what you think" into something actionable.
#[test]
fn a_format_the_parser_cannot_read_is_named_in_the_listing_and_the_summary() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .tiff_raw("shoot/DSC_0001.dng", None, at, Some("+08:00"), None)
        .jpeg_with_offset("shoot/snap.jpg", at, Some("+08:00"), None);

    let listing = preview_listing(tree.path(), &[]);

    let line = listing_line(&listing, "DSC_0001.dng");
    assert!(
        line.contains("[FS: UNSUPPORTED]"),
        "the RAW file was not flagged as an unreadable format: {line}"
    );
    assert!(
        listing.contains("Date from filesystem — format not supported: 1"),
        "the summary did not tally the unreadable format:\n{listing}"
    );
    assert!(
        listing.contains("Date from filesystem: 0"),
        "an unreadable format was folded into the ordinary filesystem tally, \
         which is the silence this line exists to break:\n{listing}"
    );
    assert!(
        listing.contains("Date from EXIF: 1"),
        "the JPEG beside it should still have been read from EXIF:\n{listing}"
    );
}

// ---------------------------------------------------------------------------
// Date-source honesty
// ---------------------------------------------------------------------------
//
// Three of the five `DateSource` variants describe the same visible outcome:
// the file is filed under its modification time. They are separate variants
// because the *reasons* are not interchangeable to whoever has to act on them.
// "This scan records no date" is a fact about the file; "the date in this file
// would not parse" and "this format is one mmm cannot read" are both admissions
// about the tool, and they point at different remedies — one at the file, one at
// us. Before this section they were one word, `Filesystem`, and a person with a
// corrupted card was told their photographs simply had no dates in them.
//
// Every fixture below is a *byte-valid JPEG*: same container, same synthesiser,
// differing only in what its EXIF block holds. That is what makes the three
// results a measurement of the extractor rather than of the harness.

/// The four ways a run can establish a date, told apart on four otherwise
/// identical files.
///
/// Asserted through [`extract_metadata`] rather than the binary because the
/// claim is about the classification itself; the two tests after this one carry
/// it the rest of the way to what a person actually reads.
#[test]
fn the_three_ways_of_falling_back_to_the_filesystem_are_told_apart() {
    let tree = MediaTree::new()
        .jpeg_with_exif("dated.jpg", naive(2024, 3, 15, 23, 30, 0), None)
        .jpeg_without_exif("scan.jpg")
        .jpeg_with_unreadable_date("garbled.jpg", "NOT-A-DATE-AT-ALL!!")
        .jpeg_with_corrupt_exif("corrupt.jpg")
        .tiff_raw(
            "DSC_0001.dng",
            None,
            naive(2024, 3, 15, 23, 30, 0),
            Some("+08:00"),
            None,
        );

    for (rel, expected) in [
        ("dated.jpg", DateSource::Exif),
        // A real JPEG with nothing in it to read. Nothing is wrong here.
        ("scan.jpg", DateSource::Filesystem),
        // The tag is present and its value is not a datetime.
        ("garbled.jpg", DateSource::Unreadable),
        // The EXIF block itself will not parse — the same admission, reached
        // down a different path, and the reason both are fixtures.
        ("corrupt.jpg", DateSource::Unreadable),
        // Not a container this tool reads at all.
        ("DSC_0001.dng", DateSource::Unsupported),
    ] {
        let meta = read_image(&tree, rel);
        assert_eq!(meta.date_source, expected, "{rel} was classified wrongly");
        assert!(
            meta.date.is_some(),
            "{rel}: however the date was established, the file still has to be \
             organised somewhere"
        );
    }
}

/// The control that stops the two `Unreadable` fixtures being vacuous.
///
/// `Unreadable` is what a merely *malformed* fixture would also produce, which
/// would make the test above an assertion about the harness's own bugs. So both
/// files are checked to be what they claim: a JPEG the parser recognises, whose
/// EXIF really does hold the nineteen bytes in question.
#[test]
fn the_unreadable_fixtures_are_real_jpegs_carrying_a_real_datetime_entry() {
    let tree = MediaTree::new()
        .jpeg_with_unreadable_date("garbled.jpg", "NOT-A-DATE-AT-ALL!!")
        .jpeg_with_corrupt_exif("corrupt.jpg");

    for rel in ["garbled.jpg", "corrupt.jpg"] {
        let bytes = std::fs::read(tree.join(rel)).expect("reading the fixture");
        assert_eq!(&bytes[..2], b"\xFF\xD8", "{rel} is not a JPEG");
        assert_eq!(&bytes[2..4], b"\xFF\xE1", "{rel} has no APP1 segment");
        assert!(
            bytes.windows(6).any(|w| w == b"Exif\0\0"),
            "{rel} does not declare an EXIF payload at all"
        );
    }

    let garbled = std::fs::read(tree.join("garbled.jpg")).expect("reading the fixture");
    assert!(
        garbled.windows(19).any(|w| w == b"NOT-A-DATE-AT-ALL!!"),
        "the unreadable stamp is not in the file, so its unreadability proves nothing"
    );
}

/// Each reason gets its own tag in the listing and its own line in the summary.
///
/// Both halves matter and they fail differently: a tally with no per-file tag
/// tells you that eleven files are suspect without saying which, and a tag with
/// no tally leaves the figure to be counted by hand out of three thousand lines.
#[test]
fn each_reason_for_a_filesystem_date_is_tagged_and_counted_separately() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .jpeg_with_offset("shoot/dated.jpg", at, Some("+08:00"), None)
        .jpeg_without_exif("shoot/scan.jpg")
        .jpeg_with_unreadable_date("shoot/garbled.jpg", "NOT-A-DATE-AT-ALL!!")
        .tiff_raw("shoot/DSC_0001.dng", None, at, Some("+08:00"), None);

    let listing = preview_listing(tree.path(), &[]);

    for (rel, tag) in [
        ("dated.jpg", "[EXIF]"),
        ("scan.jpg", "[FS]"),
        ("garbled.jpg", "[FS: UNREADABLE]"),
        ("DSC_0001.dng", "[FS: UNSUPPORTED]"),
    ] {
        let line = listing_line(&listing, rel);
        assert!(line.contains(tag), "{rel} was not tagged {tag}: {line}");
    }

    for expected in [
        "Date from EXIF: 1",
        "Date from filesystem: 1",
        "Date from filesystem — metadata unreadable: 1",
        "Date from filesystem — format not supported: 1",
    ] {
        assert!(
            listing.contains(expected),
            "the summary is missing `{expected}`:\n{listing}"
        );
    }
}

/// The counts appear on the committing path too, which is the whole point of
/// moving them into the closing summary.
///
/// They used to be printed only by the dry-run block, so the run that actually
/// moved somebody's library was the one that never said where its dates had come
/// from — the figure was visible only to whoever thought to preview first.
#[test]
fn a_committing_run_reports_where_its_dates_came_from() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .jpeg_with_offset("shoot/dated.jpg", at, Some("+08:00"), None)
        .jpeg_without_exif("shoot/scan.jpg");

    let out_dir = TempDir::new().expect("creating output TempDir");
    let result = mmm(tree.path())
        .arg("-o")
        .arg(out_dir.path().join("out"))
        .arg("--commit")
        .arg("--no-prompt")
        .output()
        .expect("running mmm in commit mode");
    assert!(result.status.success(), "the run failed");

    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("Date from EXIF: 1") && stdout.contains("Date from filesystem: 1"),
        "a committing run said nothing about where its dates came from:\n{stdout}"
    );
}

/// Above the threshold the run says so; below it, it stays quiet.
///
/// One test for both halves deliberately. A warning that never fires and a
/// warning that always fires are the same amount of use, and only asserting the
/// pair distinguishes a threshold that works from a line that is simply always
/// printed.
#[test]
fn a_run_mostly_dated_from_the_filesystem_says_so_above_the_configured_share() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    // Three of four files fall back — 75%.
    let tree = MediaTree::new()
        .jpeg_with_offset("shoot/dated.jpg", at, Some("+08:00"), None)
        .jpeg_without_exif("shoot/scan-a.jpg")
        .jpeg_without_exif("shoot/scan-b.jpg")
        .jpeg_without_exif("shoot/scan-c.jpg");

    let default = preview_listing(tree.path(), &[]);
    assert!(
        default.contains("WARNING:") && default.contains("75% of dated files (3 of 4)"),
        "a run three-quarters dated from the filesystem said nothing:\n{default}"
    );
    assert!(
        default.contains("--require-exif"),
        "the warning does not say what to do about it:\n{default}"
    );

    // Raised above the actual share, the same run stays quiet.
    let raised = mmm(tree.path())
        .env("MMM_FILESYSTEM_DATE_WARNING_PERCENT", "80")
        .output()
        .expect("running mmm with a raised threshold");
    assert!(raised.status.success(), "the run failed");
    let raised = String::from_utf8_lossy(&raised.stdout);
    assert!(
        !raised.contains("WARNING:"),
        "75% tripped a threshold of 80:\n{raised}"
    );
    assert!(
        raised.contains("Date from filesystem: 3"),
        "sanity: the same three files still fell back:\n{raised}"
    );
}

/// A threshold that is not a percentage is refused, rather than accepted as one
/// no run can ever cross.
#[test]
fn a_threshold_above_a_hundred_is_refused() {
    let tree = MediaTree::new().jpeg_without_exif("scan.jpg");

    let result = mmm(tree.path())
        .env("MMM_FILESYSTEM_DATE_WARNING_PERCENT", "500")
        .output()
        .expect("running mmm with an impossible threshold");

    assert!(!result.status.success(), "500 was accepted as a percentage");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("percentage between 0 and 100"),
        "the refusal does not say what a percentage is or how to turn the \
         warning off:\n{stderr}"
    );
}

// --- --require-exif -------------------------------------------------------

/// The conservative posture: nothing is filed under a date the file did not
/// record, and what is refused keeps its own name.
///
/// The name is half the point. `unsorted/` is otherwise all `unknown.jpg`,
/// because a file with no date has nothing to be filed by — but a file
/// `--require-exif` sent here has a perfectly good name and a date the run
/// merely declined to trust, and taking the name as well would make the safe
/// posture the lossy one.
#[test]
fn require_exif_sends_every_unverified_date_to_unsorted_under_its_own_name() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .jpeg_with_offset("shoot/dated.jpg", at, Some("+08:00"), None)
        .jpeg_without_exif("shoot/scan.jpg")
        .jpeg_with_unreadable_date("shoot/garbled.jpg", "NOT-A-DATE-AT-ALL!!")
        .tiff_raw("shoot/DSC_0001.dng", None, at, Some("+08:00"), None);

    let (_out, landed) = organise(tree.path(), &["--require-exif"]);

    assert_landed_at(
        &landed,
        "shoot/dated.jpg",
        "2024-03-15/2024-03-15-233000.jpg",
    );
    assert_landed_at(&landed, "shoot/scan.jpg", "unsorted/scan.jpg");
    assert_landed_at(&landed, "shoot/garbled.jpg", "unsorted/garbled.jpg");
    assert_landed_at(&landed, "shoot/DSC_0001.dng", "unsorted/DSC_0001.dng");
}

/// The control: without the flag the same four files are all filed by date.
///
/// Without this the test above would pass just as well against a tool that sent
/// everything to `unsorted/` all the time.
#[test]
fn the_same_files_are_all_filed_by_date_when_the_flag_is_not_passed() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .jpeg_with_offset("shoot/dated.jpg", at, Some("+08:00"), None)
        .jpeg_without_exif("shoot/scan.jpg")
        .tiff_raw("shoot/DSC_0001.dng", None, at, Some("+08:00"), None);

    let (_out, landed) = organise(tree.path(), &[]);

    assert_landed_at(
        &landed,
        "shoot/dated.jpg",
        "2024-03-15/2024-03-15-233000.jpg",
    );
    for marker in ["shoot/scan.jpg", "shoot/DSC_0001.dng"] {
        let where_it_went = landed
            .get(marker)
            .unwrap_or_else(|| panic!("{marker} vanished; the tree was {landed:#?}"));
        assert!(
            !where_it_went[0].starts_with("unsorted/"),
            "{marker} went to {} without --require-exif being passed",
            where_it_went[0]
        );
    }
}

/// A config file may ask for the conservative posture, and the command line may
/// take it back.
///
/// `require_exif` is settable from a file where `commit` is not, and the
/// difference is the direction each points: a file that made a run *more*
/// careful can only ever cost you a photograph staying where it was. The `=false`
/// spelling is what stops that from being a one-way door.
#[test]
fn require_exif_is_settable_from_a_config_file_and_answerable_from_the_command_line() {
    let at = naive(2024, 3, 15, 23, 30, 0);
    let tree = MediaTree::new()
        .jpeg_with_offset("shoot/dated.jpg", at, Some("+08:00"), None)
        .jpeg_without_exif("shoot/scan.jpg");

    let listing = preview_listing_with_env(tree.path(), &[], &[("MMM_REQUIRE_EXIF", "true")]);
    assert!(
        listing_line(&listing, "scan.jpg").contains("unsorted"),
        "a configured require_exif did not reach the plan:\n{listing}"
    );

    let listing = preview_listing_with_env(
        tree.path(),
        &["--require-exif=false"],
        &[("MMM_REQUIRE_EXIF", "true")],
    );
    assert!(
        !listing_line(&listing, "scan.jpg").contains("unsorted"),
        "--require-exif=false did not outrank the environment:\n{listing}"
    );
}
