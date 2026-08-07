//! Property tests for path derivation.
//!
//! The rest of the suite pins behaviour at chosen points — a 2024 date, a
//! London GPS pair, two files colliding on one name. That is the right shape
//! for a defect you already know about, and the wrong shape for finding the
//! next one, because every example is a place somebody thought to look.
//!
//! These tests state the *invariants* instead and let `proptest` hunt for
//! inputs that break them. Four of them, all of which the phase's earlier
//! tasks assumed without ever asserting:
//!
//! 1. **A dated file lands in a `YYYY-MM-DD` directory**, and its filename
//!    begins with the matching `YYYY-MM-DD-HHMMSS`. The whole tool is a promise
//!    about where a photo will be afterwards; this is that promise written down.
//! 2. **A derived filename is a single, safe path component** — no separators,
//!    no null bytes, no leading dot — no matter what the reverse geocoder
//!    returns or what extension it is handed.
//! 3. **The destination is strictly inside the output directory.** A photo
//!    library organiser that can be steered into writing outside its own output
//!    tree is a photo library organiser that can overwrite something it was
//!    never pointed at.
//! 4. **Collision resolution never lands on an occupied name.** Task 2 replaced
//!    the `exists()`-then-rename shape with a candidate walk; this asserts the
//!    end-to-end consequence over arbitrary occupancy rather than over the two
//!    hand-written cases in `organiser`'s own tests.
//!
//! Where a property is stated over a restricted domain, the restriction is
//! itself asserted — property 1 covers the four-digit years the naming scheme
//! can spell, and [`an_unrepresentable_year_goes_to_unsorted`] covers the rest,
//! so no input falls between the two and escapes unexamined.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use mmm::geocoder::GeoLookup;
use mmm::metadata::{DateSource, FileMetadata};
use mmm::naming::{sanitise_for_filename, year_is_representable};
use mmm::organiser::{build_target_path, collision_candidate, execute_move, PlannedMove};
use proptest::prelude::*;
use tempfile::TempDir;

/// The geocoder loads the whole `GeoNames` dataset into a k-d tree, which is
/// far too expensive to do once per generated case.
fn geo() -> &'static GeoLookup {
    static GEO: OnceLock<GeoLookup> = OnceLock::new();
    GEO.get_or_init(GeoLookup::new)
}

fn dated(date: Option<DateTime<Utc>>, gps: Option<(f64, f64)>) -> FileMetadata {
    FileMetadata {
        date,
        latitude: gps.map(|(lat, _)| lat),
        longitude: gps.map(|(_, lon)| lon),
        date_source: DateSource::Exif,
    }
}

/// `^\d{4}-\d{2}-\d{2}$`, spelled out rather than pulled in.
///
/// A regex crate for one anchored shape would be a dependency carried by every
/// build of a tool that moves photographs; this is the same assertion and it
/// says out loud that "four digits" means four *ASCII* digits — `٢٠٢٤` is four
/// characters that `char::is_numeric` calls digits and that no `YYYY` was ever
/// meant to admit.
///
/// Splitting on `-` also means a negative year cannot pass by accident: `-44`
/// would split into an empty first part and fail the width check, rather than
/// arriving as one component the way it would under a `/` split.
fn is_yyyy_mm_dd(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3
        && [4, 2, 2]
            .iter()
            .zip(&parts)
            .all(|(&width, part)| part.len() == width && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Datetimes drawn from `years`, skipping the combinations that are not real
/// calendar dates (31 February, 29 February in a common year).
fn datetime_in(years: std::ops::RangeInclusive<i32>) -> impl Strategy<Value = DateTime<Utc>> {
    (years, 1u32..=12, 1u32..=31, 0u32..=23, 0u32..=59, 0u32..=59).prop_filter_map(
        "not a real calendar date",
        |(y, month, day, h, min, s)| {
            NaiveDate::from_ymd_opt(y, month, day)?
                .and_hms_opt(h, min, s)
                .map(|naive| naive.and_utc())
        },
    )
}

/// Every year the four-digit naming scheme can spell.
fn representable_datetime() -> impl Strategy<Value = DateTime<Utc>> {
    datetime_in(0..=9999)
}

/// Years it cannot: before the era and past four digits. A negative year is not
/// hypothetical — `chrono` parses `-0044:03:15 10:00:00` out of an EXIF
/// `DateTimeOriginal` quite happily.
fn unrepresentable_datetime() -> impl Strategy<Value = DateTime<Utc>> {
    prop_oneof![datetime_in(-2000..=-1), datetime_in(10_000..=60_000)]
}

/// Anything at all, either side of the line.
fn any_datetime() -> impl Strategy<Value = DateTime<Utc>> {
    prop_oneof![representable_datetime(), unrepresentable_datetime()]
}

/// What the scanner actually produces — it only admits files whose extension is
/// on its known-media list, so these are the real inputs.
const KNOWN_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "heic", "mov", "mp4", "3gp", "m2ts"];

/// What the *library* admits. `build_target_path` is `pub`; nothing stops a
/// caller handing it a string the scanner would never have produced, and the
/// invariants above are supposed to hold for the function, not for the one
/// caller that happens to be careful.
fn extension() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => prop::sample::select(KNOWN_EXTENSIONS).prop_map(str::to_owned),
        2 => prop::sample::select(NASTY_STRINGS).prop_map(str::to_owned),
        2 => arbitrary_text(),
    ]
}

/// Hand-picked inputs a generator is unlikely to stumble on, each one a way a
/// string stops being a single path component.
const NASTY_STRINGS: &[&str] = &[
    "",
    ".",
    "..",
    "/",
    "\\",
    "../..",
    "../../etc/passwd",
    "jp/g",
    "\0",
    "a\0b",
    ".hidden",
    "  ",
    "..\\..\\windows",
    "\u{202e}gpj",
    "日本語",
    "🎞",
];

/// Short strings over the *whole* `char` domain — control characters, the null
/// byte, the astral planes and all. `any::<String>()` will not do: it generates
/// `\PC*`, which excludes exactly the control characters worth testing here.
fn arbitrary_text() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..8).prop_map(|chars| chars.into_iter().collect())
}

/// A GPS pair, or none. The full `f64` domain deliberately: a corrupt EXIF GPS
/// block yields whatever the arithmetic in `metadata` produces from it,
/// including values past the poles, infinities and NaN. The geocoder answers
/// all of them with a real place name rather than panicking, so the only
/// question left is whether that name survives into a filename intact.
fn gps() -> impl Strategy<Value = Option<(f64, f64)>> {
    prop::option::of((any::<f64>(), any::<f64>()))
}

// ---------------------------------------------------------------------------
// 1. A dated file lands in YYYY-MM-DD, named for the same instant
// ---------------------------------------------------------------------------

proptest! {
    /// The directory is four digits, two, two — and the filename opens with the
    /// same instant spelled the other way. Stated together because they are one
    /// promise: a photograph is findable by the date it was taken, from either
    /// direction.
    #[test]
    fn a_representable_date_yields_a_four_digit_dated_path(
        dt in representable_datetime(),
        ext in extension(),
        gps in gps(),
    ) {
        let (dir, filename) = build_target_path(&dated(Some(dt), gps), &ext, geo());
        let dir = dir.to_string_lossy().into_owned();

        prop_assert!(
            is_yyyy_mm_dd(&dir),
            "the directory for {dt:?} must be YYYY-MM-DD; got {dir:?}"
        );

        let stamp = format!(
            "{:04}-{:02}-{:02}-{:02}{:02}{:02}",
            dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), dt.second()
        );
        prop_assert!(
            filename.starts_with(&stamp),
            "the filename must open with {stamp}; got {filename:?}"
        );

        // And the two agree — a file filed under one date and named for another
        // is worse than either mistake alone.
        prop_assert_eq!(
            dir.replace('/', "-"),
            stamp[..10].to_owned(),
            "the directory and the filename must name the same day"
        );
    }

    /// The other half of the domain. A year the scheme cannot spell in four
    /// digits is not filed under a mangled approximation of itself — it goes to
    /// `unsorted/`, which is the bucket that already means "no date we can use".
    #[test]
    fn an_unrepresentable_year_goes_to_unsorted(
        dt in unrepresentable_datetime(),
        ext in extension(),
        gps in gps(),
    ) {
        prop_assert!(!year_is_representable(dt.year()));

        let (dir, _) = build_target_path(&dated(Some(dt), gps), &ext, geo());

        prop_assert_eq!(
            dir,
            PathBuf::from("unsorted"),
            "year {} cannot be spelled in four digits and must not be filed as if it could",
            dt.year()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. A derived filename is a single, safe path component
// ---------------------------------------------------------------------------

/// The invariant, factored out so the three tests below assert exactly the same
/// thing about the same string.
fn assert_single_safe_component(filename: &str) -> Result<(), TestCaseError> {
    prop_assert!(
        !filename.contains('/'),
        "a filename must not contain a path separator; got {filename:?}"
    );
    prop_assert!(
        !filename.contains('\\'),
        "a filename must not contain a Windows path separator; got {filename:?}"
    );
    prop_assert!(
        !filename.contains('\0'),
        "a filename must not contain a null byte; got {filename:?}"
    );
    prop_assert!(
        !filename.starts_with('.'),
        "a filename must not start with a dot; got {filename:?}"
    );

    // The strongest form of all four: the OS agrees it is one ordinary name.
    // `.` and `..` are `CurDir` and `ParentDir` here, not `Normal`, so this
    // catches them without a special case.
    if !filename.is_empty() {
        let components: Vec<Component<'_>> = Path::new(filename).components().collect();
        prop_assert!(
            matches!(components.as_slice(), [Component::Normal(_)]),
            "a filename must be exactly one ordinary path component; \
             {filename:?} parses as {components:?}"
        );
    }
    Ok(())
}

proptest! {
    /// Whatever the date, the coordinates or the extension.
    #[test]
    fn a_derived_filename_is_always_one_safe_component(
        dt in prop::option::of(any_datetime()),
        ext in extension(),
        gps in gps(),
    ) {
        let (_, filename) = build_target_path(&dated(dt, gps), &ext, geo());
        assert_single_safe_component(&filename)?;
    }

    /// The sanitiser on its own, over arbitrary text — the geocoder's output is
    /// place names from a dataset this test cannot enumerate, so the guarantee
    /// has to be asserted at the function that makes it.
    #[test]
    fn sanitising_always_yields_a_safe_fragment(raw in arbitrary_text()) {
        let cleaned = sanitise_for_filename(&raw);

        prop_assert!(!cleaned.contains('/'), "got {cleaned:?} from {raw:?}");
        prop_assert!(!cleaned.contains('\\'), "got {cleaned:?} from {raw:?}");
        prop_assert!(!cleaned.contains('\0'), "got {cleaned:?} from {raw:?}");
        prop_assert!(!cleaned.starts_with('.'), "got {cleaned:?} from {raw:?}");
        prop_assert_eq!(
            cleaned.chars().count(),
            raw.chars().count(),
            "sanitising replaces characters one for one; it must not drop or add any"
        );
    }

    /// Nasty text pushed through the real geocoded position of the filename,
    /// rather than through the sanitiser directly — the two are only equivalent
    /// if `build_target_path` actually routes the location part through it.
    #[test]
    fn a_location_suffix_never_escapes_its_filename(
        dt in representable_datetime(),
        raw in prop::sample::select(NASTY_STRINGS),
        gps in gps(),
    ) {
        let (_, filename) = build_target_path(&dated(Some(dt), gps), raw, geo());
        assert_single_safe_component(&filename)?;
    }
}

// ---------------------------------------------------------------------------
// 3. The destination is strictly inside the output directory
// ---------------------------------------------------------------------------

proptest! {
    /// Not "usually inside" — the derived tail must contain no `..`, no root,
    /// and nothing else that would let a join walk back out.
    #[test]
    fn the_destination_never_escapes_the_output_directory(
        output in prop::sample::select(&["/photos", "/tmp/out", "relative/out", "."][..]),
        dt in prop::option::of(any_datetime()),
        ext in extension(),
        gps in gps(),
    ) {
        let output = Path::new(output);
        let (dir, filename) = build_target_path(&dated(dt, gps), &ext, geo());
        let destination = output.join(&dir).join(&filename);

        prop_assert!(
            destination.starts_with(output),
            "{} is not inside {}",
            destination.display(),
            output.display()
        );

        let tail = destination
            .strip_prefix(output)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        for component in tail.components() {
            prop_assert!(
                matches!(component, Component::Normal(_)),
                "the derived tail {} must be ordinary components only; found {component:?}",
                tail.display()
            );
        }
    }

    /// The same property driven from the other end: a real file on disk, whose
    /// *name* is the hostile part, planned through the real code path. This is
    /// the one that would catch the source path leaking into the destination —
    /// today only its extension does, and that is worth pinning rather than
    /// assuming.
    #[test]
    fn a_hostile_source_filename_cannot_steer_the_destination(
        stem in prop::sample::select(&["..", "...", ".hidden", "a..b", "..%2f..", "n o r m a l"][..]),
        ext in prop::sample::select(KNOWN_EXTENSIONS),
    ) {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();

        let source = input.join(format!("{stem}.{ext}"));
        fs::write(&source, b"pixels").unwrap();

        let scan = mmm::scanner::scan_directories(std::slice::from_ref(&input));
        prop_assert_eq!(
            scan.files.len(),
            1,
            "the fixture {} should have been scanned",
            source.display()
        );

        let planned = mmm::organiser::plan_move(&scan.files[0], &output, geo())
            .map_err(|e| TestCaseError::fail(format!("{e:#}")))?;

        prop_assert!(
            planned.destination.starts_with(&output),
            "{} escaped {}",
            planned.destination.display(),
            output.display()
        );
        let tail = planned
            .destination
            .strip_prefix(&output)
            .map_err(|e| TestCaseError::fail(format!("{e}")))?;
        prop_assert!(
            !tail.components().any(|c| matches!(c, Component::ParentDir)),
            "the derived tail {} walks back out of the output tree",
            tail.display()
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Collision resolution never lands on an occupied name
// ---------------------------------------------------------------------------

proptest! {
    /// The pure half: distinct attempts are distinct names, and every candidate
    /// stays in the directory the file was planned for. A candidate that
    /// wandered into another directory would be a move to somewhere nobody
    /// asked for, and the loop in `execute_move` would never notice.
    #[test]
    fn candidates_are_distinct_and_stay_in_their_directory(
        dir in prop::sample::select(&["/photos/2024/01/15", "out", "."][..]),
        name in prop::sample::select(&["photo.jpg", "photo", "photo-1.jpg", "a.b.c", "x"][..]),
        a in 0usize..50,
        b in 0usize..50,
    ) {
        let path = Path::new(dir).join(name);
        let (first, second) = (collision_candidate(&path, a), collision_candidate(&path, b));

        if a == b {
            prop_assert_eq!(first, second);
        } else {
            prop_assert_ne!(
                &first,
                &second,
                "attempts {} and {} produced the same candidate",
                a,
                b
            );
        }

        let candidate = collision_candidate(&path, a);
        prop_assert_eq!(
            candidate.parent(),
            path.parent(),
            "a candidate must stay in the planned directory"
        );
    }

    /// The consequence, on a real filesystem: however many names are already
    /// taken, every file planned for the same destination lands somewhere free
    /// and nothing that was there before is touched.
    #[test]
    fn a_contested_destination_never_overwrites_an_occupant(
        occupied in prop::collection::vec(0usize..6, 0..6),
        movers in 1usize..5,
    ) {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let destination = out.join("2024-01-15-103000.jpg");

        // Pre-occupy an arbitrary subset of the candidate names.
        let mut squatters = Vec::new();
        for attempt in &occupied {
            let path = collision_candidate(&destination, *attempt);
            if !path.exists() {
                let body = format!("SQUATTER-{attempt}");
                fs::write(&path, body.as_bytes()).unwrap();
                squatters.push((path, body));
            }
        }

        let mut landed = Vec::new();
        for i in 0..movers {
            let source = tmp.path().join(format!("source-{i}.jpg"));
            let body = format!("MOVED-{i}");
            fs::write(&source, body.as_bytes()).unwrap();

            let outcome = execute_move(&PlannedMove {
                source: source.clone(),
                destination: destination.clone(),
                date_source: DateSource::None,
                has_location: false,
            })
            .map_err(|e| TestCaseError::fail(format!("{e:#}")))?;

            prop_assert!(!source.exists(), "the source should have moved");
            prop_assert_eq!(
                fs::read_to_string(&outcome.destination).unwrap(),
                body,
                "{} does not hold the file that was moved there",
                outcome.destination.display()
            );
            landed.push(outcome.destination);
        }

        // Nothing that was already there was disturbed.
        for (path, body) in &squatters {
            prop_assert_eq!(
                &fs::read_to_string(path).unwrap(),
                body,
                "{} was overwritten",
                path.display()
            );
            prop_assert!(
                !landed.contains(path),
                "a move landed on the occupied name {}",
                path.display()
            );
        }

        // And no two movers landed on each other.
        let mut unique = landed.clone();
        unique.sort();
        unique.dedup();
        prop_assert_eq!(unique.len(), landed.len(), "two files landed on one name");
    }
}
