//! Self-test for the fixture harness's synthetic EXIF.
//!
//! Every other integration suite in this crate asserts on where a file *lands*,
//! and where a file lands is decided by the date and coordinates the organiser
//! reads out of it. If the hand-built EXIF in `tests/common/mod.rs` did not
//! parse, those suites would still be green — the organiser would silently fall
//! back to filesystem timestamps and quietly assert on the wrong thing. This
//! file is the load-bearing check that stops that happening: it proves the
//! bytes we synthesise are read back by the *real* extractor, exactly as
//! declared, before any other test is entitled to depend on them.
//!
//! What is asserted:
//!
//! * `DateSource::Exif` — not `Filesystem`, i.e. the EXIF was genuinely parsed.
//! * The datetime round-trips to the exact second.
//! * GPS round-trips within 0.0001 degrees, in all four hemisphere quadrants.
//! * A declared `OffsetTimeOriginal` round-trips as the file's own testimony.
//! * The negative controls: a JPEG without GPS reports no coordinates, a
//!   fixture declared without an offset tag genuinely carries none, and a
//!   file that is *not* valid EXIF falls back to `Filesystem` — which is what
//!   makes the assertions above meaningful rather than vacuous.
//!
//! ## Timezone
//!
//! The datetime assertions are exact-equality against a UTC instant, so they
//! are also the regression guard for the `OffsetTimeOriginal` tag the harness
//! emits. `nom-exif` resolves a naive EXIF stamp against the machine's local
//! timezone; without that tag these assertions fail on any developer machine
//! not set to UTC, and the organiser's output directory becomes
//! machine-dependent. See the note in `tests/common/mod.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::str::FromStr as _;

use common::{naive, MediaTree};
use mmm::metadata::{extract_metadata, DateSource, FileMetadata};
use mmm::timezone::{Timezone, TimezonePolicy, TimezoneSource};

/// The tolerance the task specifies for a GPS round-trip, in decimal degrees.
/// The harness encodes seconds with a denominator of 10000, so the real error
/// is around 1e-7 — three orders of magnitude inside this.
const GPS_TOLERANCE: f64 = 0.0001;

/// Extract metadata from a fixture file, as an image (never as video).
fn read_image(tree: &MediaTree, rel: &str) -> FileMetadata {
    // The default policy — nothing configured — on purpose. The harness writes
    // an `OffsetTimeOriginal` tag, so a correct extractor never consults the
    // policy at all for these fixtures, and passing the *machine's* fallback is
    // what makes that a real assertion rather than a tautology.
    extract_metadata(&tree.join(rel), false, &TimezonePolicy::default())
        .unwrap_or_else(|e| panic!("extracting metadata from fixture {rel}: {e}"))
}

#[test]
fn synthetic_exif_datetime_round_trips_exactly() {
    let tree = MediaTree::new().jpeg_with_exif("beach.jpg", naive(2024, 1, 15, 14, 30, 0), None);

    let meta = read_image(&tree, "beach.jpg");

    assert_eq!(
        meta.date_source,
        DateSource::Exif,
        "the synthesised EXIF did not parse — the extractor fell back to {:?}, \
         which would make every downstream suite assert on filesystem timestamps",
        meta.date_source
    );
    assert_eq!(
        meta.date,
        Some(naive(2024, 1, 15, 14, 30, 0).and_utc().fixed_offset()),
        "EXIF datetime did not round-trip; if this is off by a whole-hour offset \
         the OffsetTimeOriginal tag has regressed and the machine's timezone is leaking in"
    );
    assert_eq!(
        meta.timezone_source,
        Some(TimezoneSource::ExifOffsetTag),
        "the harness writes an OffsetTimeOriginal tag, so the run must report having read \
         it rather than having assumed a zone — the assertion above is only \
         machine-independent because of that"
    );
}

#[test]
fn synthetic_exif_datetime_round_trips_across_a_range_of_dates() {
    // Spread across boundaries that byte-level date encoding tends to trip on:
    // midnight, end of year, a leap day, and a two-digit-free single-digit month.
    let cases = [
        ("midnight.jpg", naive(2019, 3, 4, 0, 0, 0)),
        ("newyears-eve.jpg", naive(2021, 12, 31, 23, 59, 59)),
        ("leap-day.jpg", naive(2020, 2, 29, 12, 0, 1)),
        ("single-digits.jpg", naive(2005, 9, 8, 7, 6, 5)),
    ];

    let mut tree = MediaTree::new();
    for (rel, dt) in cases {
        tree = tree.jpeg_with_exif(rel, dt, None);
    }

    for (rel, dt) in cases {
        let meta = read_image(&tree, rel);
        assert_eq!(
            meta.date_source,
            DateSource::Exif,
            "{rel}: not read as EXIF"
        );
        assert_eq!(
            meta.date,
            Some(dt.and_utc().fixed_offset()),
            "{rel}: datetime mismatch"
        );
    }
}

/// The offset tag says what the caller asked it to say.
///
/// [`MediaTree::jpeg_with_offset`] writes `OffsetTimeOriginal` verbatim, and the
/// timezone suite's assertions all rest on the extractor reading back the exact
/// zone the fixture declared rather than a zone it happened to be built with.
#[test]
fn a_declared_offset_tag_round_trips_as_the_files_own_testimony() {
    let cases = [
        ("east.jpg", "+08:00", 8 * 3600),
        ("west.jpg", "-05:30", -(5 * 3600 + 30 * 60)),
    ];

    let mut tree = MediaTree::new();
    for (rel, offset, _) in cases {
        tree = tree.jpeg_with_offset(rel, naive(2024, 3, 15, 23, 30, 0), Some(offset), None);
    }

    for (rel, offset, seconds) in cases {
        let meta = read_image(&tree, rel);

        assert_eq!(
            meta.date_source,
            DateSource::Exif,
            "{rel}: not read as EXIF"
        );
        assert_eq!(
            meta.timezone_source,
            Some(TimezoneSource::ExifOffsetTag),
            "{rel}: declared {offset}, so the run must report having read it"
        );

        let date = meta
            .date
            .unwrap_or_else(|| panic!("{rel}: no date read back"));
        assert_eq!(
            date.offset().local_minus_utc(),
            seconds,
            "{rel}: read back an offset other than the declared {offset}"
        );
        assert_eq!(
            date.naive_local(),
            naive(2024, 3, 15, 23, 30, 0),
            "{rel}: attaching the offset moved the wall clock the camera wrote"
        );
    }
}

/// Omitting the tag genuinely omits it.
///
/// The negative control for the test above, and the load-bearing one for the
/// timezone suite: if a fixture declared without an offset still carried one,
/// every fallback assertion in `metadata_formats.rs` would be asserting the
/// `ExifOffsetTag` path under another name and the resolution order would be
/// untested.
#[test]
fn a_fixture_declared_without_an_offset_tag_carries_none() {
    let tree =
        MediaTree::new().jpeg_with_offset("bare.jpg", naive(2024, 3, 15, 23, 30, 0), None, None);

    // A configured policy, so the answer does not depend on the machine — and
    // so that "the file said nothing" is visible as the policy being consulted.
    let policy = TimezonePolicy::new(Some(
        Timezone::from_str("Asia/Singapore").expect("Asia/Singapore is a zone"),
    ));
    let meta = extract_metadata(&tree.join("bare.jpg"), false, &policy)
        .expect("extracting metadata from the bare fixture");

    assert_eq!(
        meta.date_source,
        DateSource::Exif,
        "the EXIF must still parse — dropping the offset tag must not break the block"
    );
    assert_eq!(
        meta.timezone_source,
        Some(TimezoneSource::ConfiguredDefault),
        "a fixture declared without an offset tag was read as having one"
    );
    assert_eq!(
        meta.date.map(|dt| dt.naive_local()),
        Some(naive(2024, 3, 15, 23, 30, 0)),
        "resolving a bare wall clock against a policy must not move the wall clock"
    );
}

#[test]
fn synthetic_exif_gps_round_trips_within_tolerance() {
    // All four hemisphere quadrants, so both the GPSLatitudeRef and
    // GPSLongitudeRef sign paths are genuinely exercised, plus Null Island as
    // the zero case.
    let cases = [
        ("paris.jpg", 48.8584, 2.2945),       // N/E
        ("sydney.jpg", -33.8688, 151.2093),   // S/E
        ("quito.jpg", -0.1807, -78.4678),     // S/W
        ("reykjavik.jpg", 64.1466, -21.9426), // N/W
        ("null-island.jpg", 0.0, 0.0),
    ];

    let mut tree = MediaTree::new();
    for (rel, lat, lon) in cases {
        tree = tree.jpeg_with_exif(rel, naive(2024, 6, 1, 9, 15, 30), Some((lat, lon)));
    }

    for (rel, lat, lon) in cases {
        let meta = read_image(&tree, rel);

        assert_eq!(
            meta.date_source,
            DateSource::Exif,
            "{rel}: not read as EXIF"
        );
        assert_eq!(
            meta.date,
            Some(naive(2024, 6, 1, 9, 15, 30).and_utc().fixed_offset()),
            "{rel}: adding a GPS IFD disturbed the datetime"
        );

        let got_lat = meta
            .latitude
            .unwrap_or_else(|| panic!("{rel}: no latitude read back"));
        let got_lon = meta
            .longitude
            .unwrap_or_else(|| panic!("{rel}: no longitude read back"));

        assert!(
            (got_lat - lat).abs() < GPS_TOLERANCE,
            "{rel}: latitude {got_lat} is more than {GPS_TOLERANCE} from the declared {lat}"
        );
        assert!(
            (got_lon - lon).abs() < GPS_TOLERANCE,
            "{rel}: longitude {got_lon} is more than {GPS_TOLERANCE} from the declared {lon}"
        );
    }
}

#[test]
fn jpeg_without_gps_reports_no_coordinates() {
    let tree = MediaTree::new().jpeg_with_exif("no-gps.jpg", naive(2023, 7, 4, 18, 0, 0), None);

    let meta = read_image(&tree, "no-gps.jpg");

    assert_eq!(meta.date_source, DateSource::Exif);
    assert_eq!(
        meta.latitude, None,
        "a fixture declared without GPS must not report a latitude"
    );
    assert_eq!(
        meta.longitude, None,
        "a fixture declared without GPS must not report a longitude"
    );
}

/// The negative control, in both of its forms.
///
/// Without this, `DateSource::Exif` everywhere above could be asserting nothing.
/// It proves the extractor really does fall back when there is no EXIF to find,
/// so the `Exif` results are a signal and not the default.
///
/// The two fixtures are the two *different* fallbacks, and telling them apart is
/// the point rather than a detail: `photo.jpg` is a real JPEG that simply has no
/// metadata — a scan, or an export that stripped it — and there is nothing the
/// tool could have done differently. `garbage.jpg` is not a container the tool
/// can read at all, and its date being wrong is a limitation of this program.
/// The output tree cannot show the difference; the run has to.
#[test]
fn a_file_with_no_readable_exif_falls_back_to_the_filesystem_and_says_which_kind() {
    let tree = MediaTree::new()
        .jpeg_without_exif("photo.jpg")
        .jpeg_raw("garbage.jpg", b"this is not a JPEG");

    for (rel, expected) in [
        ("photo.jpg", DateSource::Filesystem),
        ("garbage.jpg", DateSource::Unsupported),
    ] {
        let meta = read_image(&tree, rel);

        assert_eq!(meta.date_source, expected, "{rel}: wrong fallback reported");
        assert_eq!(
            meta.timezone_source,
            Some(TimezoneSource::SystemLocal),
            "{rel}: a filesystem timestamp is a real instant, but it still has to be read \
             against some wall clock, and with nothing configured that is the machine's"
        );
        assert_eq!(meta.latitude, None, "{rel}");
        assert_eq!(meta.longitude, None, "{rel}");
    }
}

#[test]
fn a_duplicated_fixture_carries_the_same_exif_as_its_source() {
    // `duplicate_of` copies bytes verbatim, so the dedup suite's fixtures must
    // read back identically to their source. If they did not, an assertion
    // about which of a duplicate pair is retained would be meaningless.
    let tree = MediaTree::new()
        .jpeg_with_exif(
            "original.jpg",
            naive(2022, 11, 9, 5, 45, 12),
            Some((51.5074, -0.1278)),
        )
        .duplicate_of("copy.jpg", "original.jpg");

    let original = read_image(&tree, "original.jpg");
    let copy = read_image(&tree, "copy.jpg");

    assert_eq!(copy.date_source, DateSource::Exif);
    assert_eq!(copy.date, original.date);
    assert_eq!(copy.latitude, original.latitude);
    assert_eq!(copy.longitude, original.longitude);
}
