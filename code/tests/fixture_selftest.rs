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
//! * The negative controls: a JPEG without GPS reports no coordinates, and a
//!   file that is *not* valid EXIF falls back to `Filesystem` — which is what
//!   makes the `Exif` assertions above meaningful rather than vacuous.
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

use common::{naive, MediaTree};
use mmm::metadata::{extract_metadata, DateSource, FileMetadata};
use mmm::timezone::{TimezonePolicy, TimezoneSource};

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

#[test]
fn a_file_that_is_not_valid_exif_falls_back_to_the_filesystem() {
    // The negative control. Without this, `DateSource::Exif` above could be
    // asserting nothing — this proves the extractor really does report
    // `Filesystem` when there is no EXIF to find, so the `Exif` results are a
    // signal and not the default.
    let tree = MediaTree::new().jpeg_raw("garbage.jpg", b"this is not a JPEG");

    let meta = read_image(&tree, "garbage.jpg");

    assert_eq!(
        meta.date_source,
        DateSource::Filesystem,
        "expected the fallback path for a file with no parseable EXIF"
    );
    assert_eq!(
        meta.timezone_source,
        Some(TimezoneSource::SystemLocal),
        "a filesystem timestamp is a real instant, but it still has to be read against \
         some wall clock, and with nothing configured that is the machine's"
    );
    assert_eq!(meta.latitude, None);
    assert_eq!(meta.longitude, None);
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
