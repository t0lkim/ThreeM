//! Property tests for path derivation.
//!
//! The rest of the suite pins behaviour at chosen points — a 2024 date, a
//! London GPS pair, two files colliding on one name. That is the right shape
//! for a defect you already know about, and the wrong shape for finding the
//! next one, because every example is a place somebody thought to look.
//!
//! These tests state the *invariants* instead and let `proptest` hunt for
//! inputs that break them. Five of them, the first four of which the phase's
//! earlier tasks assumed without ever asserting:
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
//! 5. **All of the above still hold once the layout is configurable.** Phase 04
//!    lets a config file choose the dated directory and the filename, so the
//!    inputs are no longer only the file — they include the *format*. The last
//!    section generates those too, quantifying properties 2 and 3 over every
//!    pattern the loader will accept rather than over the one it ships with.
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
use mmm::naming::{
    sanitise_for_filename, year_is_representable, DateDirectoryFormat, FilenameFormat, Layout,
    OutputSubdir, Scheme,
};
use mmm::organiser::{build_target_path, collision_candidate, execute_move, PlannedMove};
use mmm::scanner::ScanFilter;
use mmm::settings::Settings;
use mmm::timezone::{TimezonePolicy, TimezoneSource};
use proptest::prelude::*;
use tempfile::TempDir;

/// The geocoder loads the whole `GeoNames` dataset into a k-d tree, which is
/// far too expensive to do once per generated case.
fn geo() -> &'static GeoLookup {
    static GEO: OnceLock<GeoLookup> = OnceLock::new();
    GEO.get_or_init(GeoLookup::new)
}

/// The layout a run with no config file uses.
///
/// Every property above is stated about the *default* layout, which is what the
/// tool promises when nobody has configured it. The configured case has its own
/// section at the bottom of this file, where the formats and the two
/// directories are generated too.
fn scheme() -> &'static Layout {
    static LAYOUT: OnceLock<Layout> = OnceLock::new();
    LAYOUT.get_or_init(|| {
        Settings::default()
            .layout()
            .expect("the built-in default formats must be valid")
    })
}

/// A stand-in original filename, for the `{original_stem}` token the default
/// format does not use.
const STEM: &str = "IMG_0001";

fn dated(date: Option<DateTime<Utc>>, gps: Option<(f64, f64)>) -> FileMetadata {
    FileMetadata {
        // Generated as UTC and read as a local wall clock, which for these
        // properties is the same digits either way: every claim below is about
        // what the *renderer* does with a datetime, not about which datetime it
        // was handed. Timezone resolution has its own tests.
        date: date.map(|dt| dt.fixed_offset()),
        timezone_source: date.map(|_| TimezoneSource::ExifOffsetTag),
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
        let (dir, filename) = build_target_path(&dated(Some(dt), gps), &ext, STEM, geo(), scheme());
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

        let (dir, _) = build_target_path(&dated(Some(dt), gps), &ext, STEM, geo(), scheme());

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
        let (_, filename) = build_target_path(&dated(dt, gps), &ext, STEM, geo(), scheme());
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
        let (_, filename) = build_target_path(&dated(Some(dt), gps), raw, STEM, geo(), scheme());
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
        let (dir, filename) = build_target_path(&dated(dt, gps), &ext, STEM, geo(), scheme());
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

        let scan = mmm::scanner::scan_directories(std::slice::from_ref(&input), &ScanFilter::default());
        prop_assert_eq!(
            scan.files.len(),
            1,
            "the fixture {} should have been scanned",
            source.display()
        );

        let planned = mmm::organiser::plan_move(
            &scan.files[0],
            &output,
            geo(),
            scheme(),
            &TimezonePolicy::default(),
            None,
        )
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
                timezone_source: None,
                has_location: false,
                known_hash: None,
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

// ---------------------------------------------------------------------------
// 5. The invariants above survive *any* format a config file can supply
// ---------------------------------------------------------------------------
//
// Everything so far is stated about the built-in layout, because until Phase 04
// there was only one. `date_directory_format` and `filename_format` turn that
// single layout into a family of them, which moves the question: the tool no
// longer promises "files land in `YYYY-MM-DD`", it promises "files land where
// your pattern says, *inside the output tree*". The second half of that is the
// safety property, and it now has to hold for patterns nobody has read.
//
// So the formats are generated too. Both strategies build a pattern out of
// pieces and then push it through the real constructor, keeping what it
// accepts — which means these properties are quantified over exactly the set
// `mmm` will load from a config file, and no smaller set the test author
// happened to think of. `the_format_strategies_are_not_vacuous` asserts the
// filters do not simply throw everything away.

/// Fragments a generated `date_directory_format` is assembled from.
///
/// A mixture of the specifiers people actually write, the separators that give
/// the pattern its shape, and a few literals — including ones whose *rendered*
/// form carries characters the pattern itself does not show. `%c` expands to
/// spaces and colons, `%D` to slashes of its own, `%Z` to a timezone name.
const DATE_FRAGMENTS: &[&str] = &[
    "%Y", "%m", "%d", "%H", "%M", "%S", "%j", "%B", "%b", "%C", "%y", "%e", "%A", "%Z", "%c", "%D",
    "%s", "%%", "/", "-", "_", ".", "photos", " ", "",
];

/// Fragments a generated `filename_format` is assembled from.
const NAME_FRAGMENTS: &[&str] = &[
    "{date}",
    "{time}",
    "{location}",
    "{original_stem}",
    "-",
    "_",
    ".",
    "img",
    " ",
    "",
];

/// A `date_directory_format` the loader would accept.
fn date_format() -> impl Strategy<Value = DateDirectoryFormat> {
    prop::collection::vec(
        prop_oneof![
            8 => prop::sample::select(DATE_FRAGMENTS).prop_map(str::to_owned),
            1 => arbitrary_text(),
        ],
        1..6,
    )
    .prop_map(|pieces| pieces.concat())
    .prop_filter_map("not a pattern the loader accepts", |pattern| {
        DateDirectoryFormat::new(&pattern).ok()
    })
}

/// A `filename_format` the loader would accept.
///
/// `{ext}` is planted rather than hoped for: a pattern without it is refused,
/// and generating patterns that are thrown away nineteen times in twenty would
/// test the rejection path rather than the acceptance one.
fn filename_format() -> impl Strategy<Value = FilenameFormat> {
    (
        prop::collection::vec(
            prop_oneof![
                8 => prop::sample::select(NAME_FRAGMENTS).prop_map(str::to_owned),
                1 => arbitrary_text(),
            ],
            0..4,
        ),
        prop::collection::vec(
            prop::sample::select(NAME_FRAGMENTS).prop_map(str::to_owned),
            0..3,
        ),
    )
        .prop_map(|(before, after)| format!("{}{{ext}}{}", before.concat(), after.concat()))
        .prop_filter_map("not a pattern the loader accepts", |pattern| {
            FilenameFormat::new(&pattern).ok()
        })
}

/// Fragments a generated `unsorted_dir` is assembled from.
///
/// Deliberately hostile in the ways a directory name can be: separators that
/// make it nested, a dot that would hide it, characters no filename should
/// carry. Every one of these is *accepted* by the loader — the two shapes it
/// refuses (`/` first, and `..`) cannot be generated here, because a strategy
/// that mostly produced rejects would be testing the refusal rather than the
/// containment.
const SUBDIR_FRAGMENTS: &[&str] = &[
    "unsorted", "undated", "/", "-", "_", ".", " ", "%", "a", "", "0",
];

/// An `unsorted_dir` or `duplicates_dir` the loader would accept.
fn subdir(key: &'static str) -> impl Strategy<Value = OutputSubdir> {
    prop::collection::vec(
        prop_oneof![
            8 => prop::sample::select(SUBDIR_FRAGMENTS).prop_map(str::to_owned),
            1 => arbitrary_text(),
        ],
        1..4,
    )
    .prop_map(|pieces| pieces.concat())
    .prop_filter_map("not a directory the loader accepts", move |pattern| {
        OutputSubdir::new(key, &pattern).ok()
    })
}

/// The whole configured shape of an output tree, as a run holds it.
///
/// The two directories are generated alongside the formats because they are
/// subject to the same rule: an undated photograph has to land inside the tree
/// the run was pointed at, whatever the config file called the bucket it lands
/// in.
fn naming_scheme() -> impl Strategy<Value = Layout> {
    (
        date_format(),
        filename_format(),
        any::<bool>(),
        subdir("unsorted_dir"),
        subdir("duplicates_dir"),
    )
        .prop_filter_map(
            "not a layout the loader accepts",
            |(date, name, include_location, unsorted, duplicates)| {
                let scheme = Scheme::new(date.pattern(), name.pattern(), include_location).ok()?;
                Some(Layout::new(scheme, unsorted, duplicates))
            },
        )
}

proptest! {
    /// The filters above must be leaving something behind. A strategy that
    /// rejected everything would make every property below vacuously true, and
    /// proptest reports that as a pass.
    #[test]
    fn the_format_strategies_are_not_vacuous(scheme in naming_scheme()) {
        let dt = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap()
            .and_utc();
        prop_assert!(
            scheme.date_directory(&dt).is_some(),
            "an accepted format must render something for an ordinary date"
        );
    }

    /// Property 1's safety half, restated for a configured layout. The shape is
    /// the user's business — one directory per day, a nested tree, or a name
    /// with the month spelled out — but a relative path of ordinary components
    /// is not negotiable, whatever they wrote.
    #[test]
    fn any_accepted_date_format_renders_inside_the_output_tree(
        format in date_format(),
        dt in representable_datetime(),
    ) {
        let Some(dir) = format.render(&dt) else { return Ok(()) };

        prop_assert!(
            dir.is_relative(),
            "{:?} rendered {} for {dt:?}, which is not relative",
            format.pattern(),
            dir.display()
        );
        for component in dir.components() {
            prop_assert!(
                matches!(component, Component::Normal(_)),
                "{:?} rendered {} for {dt:?}, which contains {component:?}",
                format.pattern(),
                dir.display()
            );
        }
        prop_assert!(
            !dir.as_os_str().is_empty(),
            "{:?} rendered an empty directory for {dt:?}",
            format.pattern()
        );
    }

    /// Property 2, restated for a configured filename. `{location}` can be empty
    /// and `{ext}` can be empty, so a pattern that is one safe component when it
    /// is read is not necessarily one when it is rendered — which is the whole
    /// reason the render path has a guard of its own.
    #[test]
    fn any_accepted_filename_format_renders_one_safe_component(
        scheme in naming_scheme(),
        dt in representable_datetime(),
        ext in extension(),
        stem in extension(),
        gps in gps(),
    ) {
        let (_, filename) = build_target_path(&dated(Some(dt), gps), &ext, &stem, geo(), &scheme);
        assert_single_safe_component(&filename)?;
        prop_assert!(
            !filename.is_empty(),
            "{:?} rendered nothing at all",
            scheme
        );
    }

    /// Property 3, the one that matters most, restated over both formats at
    /// once: whatever the config file said, the file lands under the output
    /// directory the run was pointed at.
    #[test]
    fn a_configured_scheme_never_escapes_the_output_directory(
        scheme in naming_scheme(),
        output in prop::sample::select(&["/photos", "/tmp/out", "relative/out", "."][..]),
        dt in prop::option::of(any_datetime()),
        ext in extension(),
        stem in extension(),
        gps in gps(),
    ) {
        let output = Path::new(output);
        let (dir, filename) = build_target_path(&dated(dt, gps), &ext, &stem, geo(), &scheme);
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

    /// The sanitising half, asserted where it is applied rather than assumed:
    /// no component of a rendered directory is a name the filesystem reads as
    /// navigation, however the specifier expanded.
    #[test]
    fn a_rendered_directory_component_is_never_navigation(
        format in date_format(),
        dt in representable_datetime(),
    ) {
        let Some(dir) = format.render(&dt) else { return Ok(()) };
        for component in &dir {
            let component = component.to_string_lossy();
            prop_assert!(
                component != "." && component != ".." && !component.contains('\0'),
                "{:?} rendered the component {component:?} for {dt:?}",
                format.pattern()
            );
            prop_assert_eq!(
                &sanitise_for_filename(&component),
                &component.to_string(),
                "every component must already be sanitised"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The refusals, stated as properties rather than as examples
// ---------------------------------------------------------------------------

proptest! {
    /// Whatever else is in the pattern, a `..` is refused. Stated over generated
    /// surroundings because the danger is not the pattern `..` — nobody writes
    /// that — but `%Y/../..%m`, which looks like a typo and is a path traversal.
    #[test]
    fn a_date_format_containing_a_parent_reference_is_always_refused(
        before in prop::sample::select(DATE_FRAGMENTS),
        after in prop::sample::select(DATE_FRAGMENTS),
    ) {
        let pattern = format!("{before}..{after}");
        prop_assert!(
            DateDirectoryFormat::new(&pattern).is_err(),
            "{pattern:?} was accepted"
        );
    }

    /// And an absolute one, which would file photographs at the root of the
    /// filesystem rather than in the library.
    #[test]
    fn an_absolute_date_format_is_always_refused(tail in prop::sample::select(DATE_FRAGMENTS)) {
        let pattern = format!("/{tail}");
        prop_assert!(
            DateDirectoryFormat::new(&pattern).is_err(),
            "{pattern:?} was accepted"
        );
    }

    /// A filename pattern is one filename. A separator anywhere in it is a
    /// directory the organiser never made and `undo` never recorded.
    #[test]
    fn a_filename_format_containing_a_separator_is_always_refused(
        before in prop::sample::select(NAME_FRAGMENTS),
        separator in prop::sample::select(&["/", "\\"][..]),
        after in prop::sample::select(NAME_FRAGMENTS),
    ) {
        let pattern = format!("{before}{separator}{after}{{ext}}");
        prop_assert!(
            FilenameFormat::new(&pattern).is_err(),
            "{pattern:?} was accepted"
        );
    }

    /// A pattern with no `{ext}` strips every file's extension, which no other
    /// program on the machine will forgive.
    #[test]
    fn a_filename_format_without_an_extension_token_is_always_refused(
        pieces in prop::collection::vec(prop::sample::select(NAME_FRAGMENTS), 1..5),
    ) {
        let pattern = pieces.concat();
        prop_assume!(!pattern.is_empty() && !pattern.starts_with('.'));
        prop_assert!(
            FilenameFormat::new(&pattern).is_err(),
            "{pattern:?} has no {{ext}} and was accepted"
        );
    }
}
