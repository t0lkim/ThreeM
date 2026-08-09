//! A seeded synthetic photo library, and a written statement of what `mmm`
//! should do with it.
//!
//! [`crate::fixtures`] builds one file at a time and says nothing about what
//! any of them means. This module composes those primitives into a *library* —
//! a few hundred files with the shapes a real import has, plus the shapes that
//! have historically broken this tool — and, crucially, emits an `EXPECTED.md`
//! alongside them saying where each file ought to end up and why.
//!
//! That second half is the point. A pile of synthetic photographs with no
//! statement of intent lets a user watch `mmm` do *something* and gives them no
//! way to tell whether it was the right something. The expectations are what
//! turn "it ran" into "it was correct", and `tests/generated_library.rs` holds
//! the two to each other: it generates a library, organises it, and asserts the
//! result matches this module's own predictions. If they ever disagree, one of
//! them is wrong and the suite fails rather than the document quietly lying.
//!
//! ## What the predictions are allowed to say
//!
//! Every expectation here is either something the generator *chose* (it wrote
//! `2023-04-17T14:30:00+00:00` into the EXIF, so the file belongs under
//! `2023-04-17/`) or something it *measured* after the fact (it stat'd the file
//! it just wrote, so it knows the filesystem date the tool will fall back to).
//! None of it re-implements the organiser's naming, because a prediction
//! derived by copying the code under test proves only that the copy matches.
//!
//! Where an outcome genuinely is not predictable — a file with no timezone
//! offset, whose filed date depends on the zone the run resolves — the
//! expectation says so instead of inventing a directory. An honest "this
//! depends on `--timezone`" is a useful thing to hand a user; a confident wrong
//! answer is not.

use std::fmt::{self, Write as _};
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Datelike, NaiveDateTime, Utc};

use crate::fixtures::{naive, MediaTree, VideoSpec, XmpForm};

/// How large a library to build and which shapes to put in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// A couple of dozen files. Enough to read the whole `EXPECTED.md` in one
    /// sitting and follow every case by hand, which is what somebody
    /// evaluating the tool for the first time actually wants.
    Minimal,
    /// An ordinary import: dated JPEGs and HEICs from several cameras, video,
    /// RAW with sidecars, some duplicates, a scattering of GPS. Nothing
    /// deliberately malformed. This is the profile that answers "would it file
    /// my library correctly".
    Realistic,
    /// Every awkward shape known to have broken this tool or a tool like it —
    /// zero-byte files, truncated headers, EXIF that will not parse, a date
    /// nothing can read, coordinates outside the ISO 6709 bounds, sidecars
    /// with no photograph, names full of characters a filesystem tolerates and
    /// a naive string-handler does not.
    ///
    /// **These files are meant to be wrong.** A run over this profile is
    /// expected to emit warnings and to route files to `unsorted/`; that is the
    /// correct result, not a defect. `EXPECTED.md` marks every one of them.
    Awkward,
    /// [`Self::Realistic`] scaled up, with large duplicate groups — for watching the
    /// three-phase hash cascade work and for timing a run over something that
    /// takes more than an instant.
    Stress,
}

impl Profile {
    /// Parse the name as it is spelled on the command line.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "realistic" => Some(Self::Realistic),
            "awkward" => Some(Self::Awkward),
            "stress" => Some(Self::Stress),
            _ => None,
        }
    }

    /// The names `parse` accepts, in the order they are worth trying.
    pub const ALL: &'static [&'static str] = &["minimal", "realistic", "awkward", "stress"];

    /// One line describing the profile, for `--help` and for `EXPECTED.md`.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Minimal => "a couple of dozen well-formed files, small enough to check by hand",
            Self::Realistic => "an ordinary import — several cameras, video, RAW, duplicates, GPS",
            Self::Awkward => "the malformed and the ambiguous: files that are meant to be wrong",
            Self::Stress => "a large well-formed library with big duplicate groups",
        }
    }

    /// Roughly how many ordinary dated photographs to lay down.
    const fn bulk(self) -> u32 {
        match self {
            Self::Minimal => 12,
            Self::Realistic => 90,
            Self::Awkward => 20,
            Self::Stress => 600,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Minimal => "minimal",
            Self::Realistic => "realistic",
            Self::Awkward => "awkward",
            Self::Stress => "stress",
        })
    }
}

// ---------------------------------------------------------------------------
// Randomness
// ---------------------------------------------------------------------------

/// A seeded xorshift64\* generator.
///
/// Deliberately not a dependency. This does not need to be a good PRNG — it
/// needs to be a *reproducible* one, so that a user who reports "the library
/// from seed 7 files wrongly" hands over everything required to reproduce it,
/// and so that the generator's own test can assert two runs at one seed are
/// byte-identical. Twenty lines of well-known arithmetic buys that without
/// putting `rand` and its tree into every user's build of a photo organiser.
pub struct Rng(u64);

impl Rng {
    /// Seeded. Zero is remapped, since xorshift is absorbing at zero and a
    /// `--seed 0` that silently produced the same constant forever would be a
    /// nasty little surprise.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-enough in `0..n`. The modulo bias over ranges this small is
    /// irrelevant to generating photographs.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// Uniform-enough in `lo..hi`, both `u32` because everything drawn from it
    /// is a calendar field.
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        debug_assert!(hi > lo);
        lo + u32::try_from(self.below(u64::from(hi - lo))).unwrap_or(0)
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[usize::try_from(self.below(xs.len() as u64)).unwrap_or(0)]
    }

    /// True `percent` of the time.
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// A capture time somewhere in the last several years, at an hour a person
    /// would plausibly be holding a camera.
    fn moment(&mut self) -> NaiveDateTime {
        let year = i32::try_from(self.range(2019, 2026)).unwrap_or(2020);
        let month = self.range(1, 13);
        // 28 keeps every month legal without a calendar lookup, and a library
        // that never contains a 30th is not a library that files differently.
        let day = self.range(1, 29);
        let hour = self.range(6, 23);
        let minute = self.range(0, 60);
        let second = self.range(0, 60);
        naive(year, month, day, hour, minute, second)
    }
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// What the generator asserts should become of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// Filed under this date directory, e.g. `2023-04-17`. The generator knows
    /// this because it wrote the timestamp and the UTC offset itself.
    Filed(String),
    /// Filed under this date directory, which the generator obtained by
    /// stat'ing the file after writing it — the file carries no date of its
    /// own and the tool will fall back to the filesystem.
    ///
    /// Under `--require-exif` these go to `unsorted/` instead, keeping their
    /// own names. `EXPECTED.md` says so.
    FiledFromFilesystem(String),
    /// The wall clock is known but the file records no UTC offset, so which
    /// directory it lands in depends on the zone the run resolves — `--timezone`,
    /// then the machine's own. Stated rather than predicted.
    TimezoneDependent(String),
    /// Routed to `unsorted/`, for the stated reason.
    Unsorted(&'static str),
    /// Byte-identical to another generated file. Exactly one member of the
    /// group is filed normally and the rest are moved under `duplicates/`;
    /// which one stays is a matter of scan order and is not predicted here.
    DuplicateOf(String),
    /// A sidecar, which travels with the photograph it belongs to rather than
    /// being filed in its own right.
    TravelsWith(String),
    /// Not a media file. Left exactly where it is, untouched.
    Untouched,
}

/// One generated file and the claim made about it.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path relative to the generated library root.
    pub rel: String,
    /// What the file is, in a sentence.
    pub what: String,
    /// What `mmm` should do with it.
    pub expect: Expect,
    /// Deliberately malformed — a warning about this file is the correct
    /// outcome, not a bug.
    pub malformed: bool,
}

/// Everything the generator built, and everything it claims.
#[derive(Debug, Clone)]
pub struct Plan {
    pub seed: u64,
    pub profile: Profile,
    pub entries: Vec<Entry>,
}

impl Plan {
    /// Entries whose expectation names a concrete date directory — the ones a
    /// test can assert on without qualification.
    #[must_use]
    pub fn definite(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.expect, Expect::Filed(_) | Expect::FiledFromFilesystem(_)))
            .collect()
    }

    /// How many files were laid down.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Coordinates inside populated places the bundled `GeoNames` dataset knows, so a
/// run with location naming turned on produces a recognisable name rather than
/// a blank.
const PLACES: &[(&str, f64, f64)] = &[
    ("Paris", 48.8584, 2.2945),
    ("Tokyo", 35.6895, 139.6917),
    ("New York", 40.7128, -74.0060),
    ("Sydney", -33.8688, 151.2093),
    ("Singapore", 1.3521, 103.8198),
    ("Reykjavik", 64.1466, -21.9426),
    ("Lisbon", 38.7223, -9.1393),
];

/// Directories a real import tends to arrive in.
const SOURCES: &[&str] = &[
    "DCIM/100CANON",
    "DCIM/101APPLE",
    "iPhone Backup",
    "Camera Roll",
    "Downloads",
];

/// Build a library under `tree` and return the claims made about it.
///
/// The `MediaTree` is taken and returned because its builders consume `self`;
/// the caller gets it back so the root stays alive and the directory is not
/// swept away before anything can look at it.
#[must_use]
pub fn generate(tree: MediaTree, profile: Profile, seed: u64) -> (MediaTree, Plan) {
    let mut rng = Rng::new(seed);
    let mut entries: Vec<Entry> = Vec::new();
    let mut tree = tree;

    tree = bulk_photographs(tree, profile, &mut rng, &mut entries);
    tree = videos(tree, profile, &mut rng, &mut entries);
    tree = raw_and_sidecars(tree, profile, &mut rng, &mut entries);
    tree = undated(tree, &mut rng, &mut entries);
    tree = duplicates(tree, profile, &mut rng, &mut entries);
    tree = bystanders(tree, &mut entries);

    if profile == Profile::Awkward {
        tree = awkward(tree, &mut rng, &mut entries);
    }

    // The filesystem-dated entries could only be settled once the files
    // existed. Do it in one pass now rather than stat'ing inside each builder.
    resolve_filesystem_dates(tree.path(), &mut entries);

    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    let plan = Plan {
        seed,
        profile,
        entries,
    };
    (tree, plan)
}

/// The backbone: ordinary dated photographs, each with an explicit `+00:00`
/// offset so its filed directory is a fact rather than a guess.
fn bulk_photographs(
    mut tree: MediaTree,
    profile: Profile,
    rng: &mut Rng,
    out: &mut Vec<Entry>,
) -> MediaTree {
    for i in 0..profile.bulk() {
        let when = rng.moment();
        let source = rng.pick(SOURCES);
        let heic = rng.chance(25);
        let ext = if heic { "heic" } else { "jpg" };
        let rel = format!("{source}/IMG_{:04}.{ext}", 1000 + i);

        let place = rng.chance(40).then(|| *rng.pick(PLACES));
        let gps = place.map(|(_, lat, lon)| (lat, lon));

        tree = if heic {
            tree.heic_with_exif(&rel, when, gps)
        } else {
            tree.jpeg_with_exif(&rel, when, gps)
        };

        let located = place.map_or_else(String::new, |(name, _, _)| format!(", taken in {name}"));
        out.push(Entry {
            what: format!(
                "{} carrying EXIF DateTimeOriginal {} with a +00:00 offset{located}",
                if heic { "HEIC" } else { "JPEG" },
                when.format("%Y-%m-%d %H:%M:%S")
            ),
            expect: Expect::Filed(directory_of(when)),
            rel,
            malformed: false,
        });
    }
    tree
}

/// MP4 and MOV, both container date sources.
fn videos(mut tree: MediaTree, profile: Profile, rng: &mut Rng, out: &mut Vec<Entry>) -> MediaTree {
    let count = (profile.bulk() / 8).max(2);
    for i in 0..count {
        let when = rng.moment();
        let mov = rng.chance(40);
        let rel = format!(
            "Videos/VID_{:04}.{}",
            2000 + i,
            if mov { "mov" } else { "mp4" }
        );

        let spec = if mov {
            VideoSpec::mov(when)
        } else {
            VideoSpec::mp4(when)
        };
        tree = tree.iso_video(&rel, &spec);

        out.push(Entry {
            what: format!(
                "{} whose moov/mvhd records {} UTC",
                if mov { "QuickTime .mov" } else { "MP4" },
                when.format("%Y-%m-%d %H:%M:%S")
            ),
            expect: Expect::Filed(directory_of(when)),
            rel,
            malformed: false,
        });
    }
    tree
}

/// TIFF-based RAW, which carries no date this tool can read on its own, paired
/// with the XMP sidecar that gives it one.
///
/// This is the single most valuable case in the whole library: a RAW library is
/// exactly the shape that gets filed under the wrong date by a tool that reads
/// the filesystem and calls it a capture time, and the sidecar path is the only
/// way `mmm` gets it right.
fn raw_and_sidecars(
    mut tree: MediaTree,
    profile: Profile,
    rng: &mut Rng,
    out: &mut Vec<Entry>,
) -> MediaTree {
    let count = (profile.bulk() / 10).max(2);
    for i in 0..count {
        let when = rng.moment();
        let ext = rng.pick(&["dng", "nef", "arw", "cr2"]);
        let stem = format!("DCIM/100CANON/RAW_{:04}", 3000 + i);
        let raw = format!("{stem}.{ext}");
        let sidecar = format!("{stem}.xmp");

        // No offset in the RAW itself: nothing reads its EXIF anyway, and the
        // date that matters is the sidecar's.
        tree = tree.tiff_raw(&raw, None, when, None, None);
        tree = tree.xmp(
            &sidecar,
            if rng.chance(50) {
                XmpForm::Attribute
            } else {
                XmpForm::Element
            },
            &[(
                "exif:DateTimeOriginal",
                &when.format("%Y-%m-%dT%H:%M:%S").to_string(),
            )],
        );

        out.push(Entry {
            what: format!(
                "TIFF-based RAW (.{ext}) with no date `mmm` can read from the file itself"
            ),
            expect: Expect::Filed(directory_of(when)),
            rel: raw.clone(),
            malformed: false,
        });
        out.push(Entry {
            what: format!(
                "XMP sidecar declaring exif:DateTimeOriginal {} — this is where the RAW's date comes from",
                when.format("%Y-%m-%d %H:%M:%S")
            ),
            expect: Expect::TravelsWith(raw),
            rel: sidecar,
            malformed: false,
        });
    }
    tree
}

/// Files with no usable date at all, which fall back to the filesystem.
fn undated(mut tree: MediaTree, rng: &mut Rng, out: &mut Vec<Entry>) -> MediaTree {
    for i in 0..3u32 {
        let rel = format!("Scans/scan_{i:03}.jpg");
        tree = tree.jpeg_without_exif(&rel);
        out.push(Entry {
            what: "JPEG with no EXIF segment at all — a scan, a screenshot, a stripped export"
                .to_owned(),
            // Settled by stat after writing.
            expect: Expect::FiledFromFilesystem(String::new()),
            rel,
            malformed: false,
        });
    }

    // A file that has a wall clock and refuses to say what zone it is in. The
    // one shape whose destination genuinely cannot be predicted from here.
    let when = rng.moment();
    let rel = "Camera Roll/no_offset.jpg".to_owned();
    tree = tree.jpeg_with_offset(&rel, when, None, None);
    out.push(Entry {
        what: format!(
            "JPEG recording the wall clock {} and no UTC offset",
            when.format("%Y-%m-%d %H:%M:%S")
        ),
        expect: Expect::TimezoneDependent(directory_of(when)),
        rel,
        malformed: false,
    });

    tree
}

/// Byte-identical copies, in groups, in different directories and under
/// different names — which is what a duplicate actually looks like after two
/// phone backups and a re-import.
fn duplicates(
    mut tree: MediaTree,
    profile: Profile,
    rng: &mut Rng,
    out: &mut Vec<Entry>,
) -> MediaTree {
    let (groups, per_group) = match profile {
        Profile::Minimal => (1, 2),
        Profile::Realistic | Profile::Awkward => (3, 3),
        Profile::Stress => (8, 6),
    };

    for g in 0..groups {
        let when = rng.moment();
        let original = format!("DCIM/100CANON/DUP_{g}_original.jpg");
        tree = tree.jpeg_with_exif(&original, when, None);
        out.push(Entry {
            what: format!(
                "first member of duplicate group {g} — {} identical copies exist",
                per_group + 1
            ),
            expect: Expect::Filed(directory_of(when)),
            rel: original.clone(),
            malformed: false,
        });

        for c in 0..per_group {
            let rel = format!("{}/DUP_{g}_copy_{c}.jpg", rng.pick(SOURCES));
            tree = tree.duplicate_of(&rel, &original);
            out.push(Entry {
                what: format!("byte-identical copy of `{original}`"),
                expect: Expect::DuplicateOf(original.clone()),
                rel,
                malformed: false,
            });
        }
    }
    tree
}

/// Things that are not photographs and must survive untouched. A tool that
/// moves these is a tool that has rearranged somebody's whole disk.
fn bystanders(mut tree: MediaTree, out: &mut Vec<Entry>) -> MediaTree {
    let files: &[(&str, &[u8])] = &[
        ("Downloads/receipt.pdf", b"%PDF-1.4 not a photograph"),
        ("notes.txt", b"do not touch me"),
        ("Camera Roll/.hidden_state", b"private"),
    ];
    for (rel, bytes) in files {
        tree = tree.non_media(rel, bytes);
        out.push(Entry {
            rel: (*rel).to_owned(),
            what: "not a media file — must be left exactly where it is".to_owned(),
            expect: Expect::Untouched,
            malformed: false,
        });
    }
    tree
}

/// The malformed half. Every one of these is deliberate.
fn awkward(mut tree: MediaTree, rng: &mut Rng, out: &mut Vec<Entry>) -> MediaTree {
    let push = |rel: &str, what: &str, expect: Expect, out: &mut Vec<Entry>| {
        out.push(Entry {
            rel: rel.to_owned(),
            what: what.to_owned(),
            expect,
            malformed: true,
        });
    };

    // Sub-two-byte files. These crashed the tool before v0.3.0: the EXIF parser
    // asserts on a buffer it can read a marker out of, and a run over a library
    // containing one aborted before moving anything.
    tree = tree.jpeg_raw("Awkward/zero_bytes.jpg", b"");
    push(
        "Awkward/zero_bytes.jpg",
        "a zero-byte file with a .jpg extension — this shape panicked the tool before v0.3.0",
        Expect::FiledFromFilesystem(String::new()),
        out,
    );
    tree = tree.jpeg_raw("Awkward/one_byte.jpg", b"\xFF");
    push(
        "Awkward/one_byte.jpg",
        "a one-byte file — too short for any parser to read a container marker from",
        Expect::FiledFromFilesystem(String::new()),
        out,
    );

    // EXIF that is present and will not parse, as distinct from absent.
    tree = tree.jpeg_with_corrupt_exif("Awkward/corrupt_exif.jpg");
    push(
        "Awkward/corrupt_exif.jpg",
        "a decodable JPEG whose APP1 segment is not a TIFF block — the container reads, the metadata does not",
        Expect::FiledFromFilesystem(String::new()),
        out,
    );

    // A date that is present and unreadable, as distinct from no date.
    tree = tree.jpeg_with_unreadable_date("Awkward/bad_date_text.jpg", "0000:00:00 00:00:00");
    push(
        "Awkward/bad_date_text.jpg",
        "EXIF DateTimeOriginal containing `0000:00:00 00:00:00` — the all-zeroes stamp a camera with a flat clock battery writes: a date that is present, well-formed and means nothing",
        Expect::FiledFromFilesystem(String::new()),
        out,
    );

    // GPS with no date. This lost the coordinates entirely until v0.3.1.
    let place = *rng.pick(PLACES);
    tree = tree.jpeg_with_exif_but_no_date("Awkward/gps_no_date.jpg", Some((place.1, place.2)));
    push(
        "Awkward/gps_no_date.jpg",
        &format!(
            "EXIF with coordinates for {} and no DateTimeOriginal — the location must survive into the filename even though the date does not",
            place.0
        ),
        Expect::FiledFromFilesystem(String::new()),
        out,
    );

    // Coordinates outside the ISO 6709 bounds. Must be refused, not filed under
    // whatever place a wrapped lookup lands on.
    let when = rng.moment();
    tree = tree.jpeg_with_exif("Awkward/impossible_gps.jpg", when, Some((91.5, 181.5)));
    push(
        "Awkward/impossible_gps.jpg",
        "latitude 91.5, longitude 181.5 — outside the ISO 6709 bounds, so no place name may be invented for it",
        Expect::Filed(directory_of(when)),
        out,
    );

    // Names that break naive string handling.
    let awkward_names: &[(&str, &str)] = &[
        (
            "Awkward/a name with spaces.jpg",
            "spaces throughout the filename",
        ),
        (
            "Awkward/émoji-📷-café.jpg",
            "non-ASCII and an emoji in the filename",
        ),
        (
            "Awkward/quote'and\"double.jpg",
            "single and double quotes in the filename",
        ),
        (
            "Awkward/trailing.dots...jpg",
            "consecutive dots before the extension",
        ),
        ("Awkward/UPPERCASE.JPG", "an uppercase extension"),
    ];
    for (rel, why) in awkward_names {
        let when = rng.moment();
        tree = tree.jpeg_with_exif(rel, when, None);
        push(
            rel,
            &format!("a well-formed JPEG with {why}"),
            Expect::Filed(directory_of(when)),
            out,
        );
    }

    // A sidecar with no photograph. It is not media, so it must not be filed on
    // its own; it must also not be silently deleted.
    tree = tree.sidecar("Awkward/orphan.xmp", b"<x:xmpmeta/>");
    push(
        "Awkward/orphan.xmp",
        "an XMP sidecar whose photograph does not exist — it belongs to nothing and must not be filed alone",
        Expect::Untouched,
        out,
    );

    // An empty directory, which must not become a destination.
    tree = tree.empty_dir("Awkward/empty");
    push(
        "Awkward/empty/",
        "an empty directory",
        Expect::Untouched,
        out,
    );

    tree
}

// ---------------------------------------------------------------------------
// Settling the filesystem-dated entries
// ---------------------------------------------------------------------------

/// Fill in the date directory for every entry that will be filed on its
/// filesystem timestamp, by reading the timestamp the filesystem actually
/// recorded.
///
/// Measured rather than assumed: the mtime is whatever the OS wrote a moment
/// ago, read back in the document's declared zone. Predicting "today" from the
/// clock would be right nearly always and wrong across a midnight boundary,
/// which is precisely the class of bug this library exists to catch.
fn resolve_filesystem_dates(root: &Path, entries: &mut [Entry]) {
    for entry in entries {
        if !matches!(entry.expect, Expect::FiledFromFilesystem(_)) {
            continue;
        }
        let dir = std::fs::metadata(root.join(&entry.rel))
            .and_then(|m| m.modified())
            .map_or_else(|_| "unknown".to_owned(), directory_of_system_time);
        entry.expect = Expect::FiledFromFilesystem(dir);
    }
}

/// The date directory a wall clock belongs in, in the default `YYYY-MM-DD`
/// layout.
fn directory_of(dt: NaiveDateTime) -> String {
    format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day())
}

/// The same, for a filesystem timestamp.
///
/// Read in **UTC**, because UTC is the one zone `EXPECTED.md` is written in.
/// The dated fixtures record an explicit `+00:00` and their predicted
/// directories are the wall clocks as written, which is what the tool produces
/// under `--timezone UTC` and nothing else — so a filesystem timestamp read in
/// the machine's zone would put the two halves of the same document in two
/// different frames. They agreed only on a machine already running UTC: east of
/// Greenwich the document contradicted itself for the eight hours after local
/// midnight, and `every_definite_expectation_is_met` failed for exactly that
/// window. `expected_markdown` tells the reader to pass `--timezone UTC` for
/// the same reason.
fn directory_of_system_time(t: SystemTime) -> String {
    let utc: DateTime<Utc> = t.into();
    format!("{:04}-{:02}-{:02}", utc.year(), utc.month(), utc.day())
}

// ---------------------------------------------------------------------------
// EXPECTED.md
// ---------------------------------------------------------------------------

/// Render the plan as the `EXPECTED.md` that ships beside the library.
///
/// Written for somebody who has just run the generator and wants to know
/// whether `mmm` got it right — so it leads with how to check, groups by
/// outcome rather than by directory, and is explicit about which files are
/// meant to be wrong.
#[must_use]
pub fn expected_markdown(plan: &Plan) -> String {
    let mut s = String::new();

    s.push_str("# What `mmm` should do with this library\n\n");
    let _ = write!(
        s,
        "Generated by `mmm-fixtures` — profile **{}** ({}), seed **{}**, {} files.\n\n",
        plan.profile,
        plan.profile.summary(),
        plan.seed,
        plan.len()
    );
    let _ = write!(
        s,
        "The same seed reproduces this library exactly. If you find a case where \
         `mmm` behaves wrongly, `--seed {}` and this profile are the whole \
         reproduction.\n\n",
        plan.seed
    );

    s.push_str("## How to check\n\n");
    s.push_str(
        "```bash\n\
         mmm <this directory> -o /tmp/organised --timezone UTC           # preview — moves nothing\n\
         mmm <this directory> -o /tmp/organised --timezone UTC --commit  # do it\n\
         mmm undo /tmp/organised --commit                                # put it all back\n\
         ```\n\n\
         **`--timezone UTC` is not decoration.** Every date in this document is \
         stated in UTC, because that is the only zone in which they are a \
         property of the library rather than of the machine reading it: the \
         dated fixtures record their times with an explicit `+00:00`, so a run \
         in your own zone files them somewhere else and every table below would \
         look wrong when nothing was. Drop the flag and the tool is behaving \
         correctly — this document simply stops describing it.\n\n\
         Compare `/tmp/organised` against the tables below. Directory names \
         assume the default `YYYY-MM-DD` layout; if you have configured \
         `date_directory_format`, the dates are the same and the spelling is \
         yours.\n\n",
    );

    let malformed = plan.entries.iter().filter(|e| e.malformed).count();
    if malformed > 0 {
        let _ = write!(
            s,
            "> **{malformed} of these files are deliberately malformed.** A run over this \
             library is *expected* to print warnings and to route files to \
             `unsorted/`. That is the correct result. Every such file is marked \
             ⚠ below, with what is wrong with it.\n\n"
        );
    }

    section(
        &mut s,
        plan,
        "Filed under a date the file itself records",
        "The tool read a capture time out of the file. These are the unambiguous cases: \
         if one of them lands anywhere else, that is a defect.",
        |e| matches!(e.expect, Expect::Filed(_)),
        |e| match &e.expect {
            Expect::Filed(d) => format!("`{d}/`"),
            _ => String::new(),
        },
    );

    section(
        &mut s,
        plan,
        "Filed on the filesystem timestamp",
        "These files carry no capture time the tool can read, so it falls back to the \
         file's own modification time — which is when the generator wrote it, a moment \
         ago. The run says so per file rather than passing the fallback off as a real \
         date. **Under `--require-exif` these go to `unsorted/` instead**, keeping their \
         own names.",
        |e| matches!(e.expect, Expect::FiledFromFilesystem(_)),
        |e| match &e.expect {
            Expect::FiledFromFilesystem(d) => format!("`{d}/`"),
            _ => String::new(),
        },
    );

    section(
        &mut s,
        plan,
        "Depends on the timezone the run resolves",
        "This file records a wall clock and refuses to say what zone it was in. Which \
         directory it lands in depends on `--timezone`, or failing that on the machine's \
         own zone. The date below is the wall clock as written; that is what you get \
         with `--timezone UTC`.",
        |e| matches!(e.expect, Expect::TimezoneDependent(_)),
        |e| match &e.expect {
            Expect::TimezoneDependent(d) => format!("`{d}/` at UTC"),
            _ => String::new(),
        },
    );

    section(
        &mut s,
        plan,
        "Duplicates",
        "Byte-identical copies. Exactly one member of each group is filed normally and \
         the rest are moved under `duplicates/`, each group with a `manifest.txt` \
         recording where every file came from. **Which member stays put is a matter of \
         scan order and is not predicted here** — what matters is that one survives in \
         the dated tree and no original is ever deleted. Verify independently with \
         `mmm-dedup-verifier` before deleting anything.",
        |e| matches!(e.expect, Expect::DuplicateOf(_)),
        |e| match &e.expect {
            Expect::DuplicateOf(o) => format!("`duplicates/` (copy of `{o}`)"),
            _ => String::new(),
        },
    );

    section(
        &mut s,
        plan,
        "Sidecars",
        "These travel with the photograph they belong to rather than being filed in \
         their own right, and they are renamed to match it. Pass `--no-sidecars` to \
         leave them where they are.",
        |e| matches!(e.expect, Expect::TravelsWith(_)),
        |e| match &e.expect {
            Expect::TravelsWith(o) => format!("wherever `{o}` goes"),
            _ => String::new(),
        },
    );

    section(
        &mut s,
        plan,
        "Untouched",
        "Not media. A tool that moves any of these has rearranged somebody's disk.",
        |e| matches!(e.expect, Expect::Untouched),
        |_| "left exactly where it is".to_owned(),
    );

    section(
        &mut s,
        plan,
        "Unsorted",
        "Routed to `unsorted/` for the stated reason.",
        |e| matches!(e.expect, Expect::Unsorted(_)),
        |e| match &e.expect {
            Expect::Unsorted(why) => format!("`unsorted/` — {why}"),
            _ => String::new(),
        },
    );

    s.push_str("---\n\n");
    s.push_str(
        "Nothing here asserts a *filename*. The date directory is the claim worth \
         checking by hand; the filename pattern is configurable and stating one here \
         would only be restating the default back at you.\n",
    );

    s
}

/// One table, omitted entirely when it has no rows.
fn section(
    s: &mut String,
    plan: &Plan,
    title: &str,
    blurb: &str,
    matches: impl Fn(&Entry) -> bool,
    destination: impl Fn(&Entry) -> String,
) {
    let rows: Vec<&Entry> = plan.entries.iter().filter(|e| matches(e)).collect();
    if rows.is_empty() {
        return;
    }

    let _ = write!(s, "## {title} ({} files)\n\n{blurb}\n\n", rows.len());
    s.push_str("| File | Should end up | What it is |\n|---|---|---|\n");
    for e in rows {
        let _ = writeln!(
            s,
            "| `{}` | {} | {}{} |",
            e.rel,
            destination(e),
            if e.malformed { "⚠ " } else { "" },
            e.what.replace('|', "\\|")
        );
    }
    s.push('\n');
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a test that unwraps is a test that fails loudly"
    )]

    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn the_same_seed_produces_the_same_plan() {
        let (_a_tree, a) = generate(MediaTree::new(), Profile::Realistic, 42);
        let (_b_tree, b) = generate(MediaTree::new(), Profile::Realistic, 42);

        let names = |p: &Plan| p.entries.iter().map(|e| e.rel.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&a),
            names(&b),
            "one seed must reproduce one library, or a bug report citing a seed is worthless"
        );
    }

    #[test]
    fn different_seeds_produce_different_libraries() {
        let (_a_tree, a) = generate(MediaTree::new(), Profile::Realistic, 1);
        let (_b_tree, b) = generate(MediaTree::new(), Profile::Realistic, 2);
        assert_ne!(
            a.entries
                .iter()
                .map(|e| format!("{}{:?}", e.rel, e.expect))
                .collect::<Vec<_>>(),
            b.entries
                .iter()
                .map(|e| format!("{}{:?}", e.rel, e.expect))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn seed_zero_is_not_absorbing() {
        let mut rng = Rng::new(0);
        let first = rng.next_u64();
        let second = rng.next_u64();
        assert_ne!(first, 0);
        assert_ne!(
            first, second,
            "xorshift is absorbing at zero; new() remaps it"
        );
    }

    #[test]
    fn every_file_the_plan_names_actually_exists() {
        let (tree, plan) = generate(MediaTree::new(), Profile::Awkward, 7);
        for entry in &plan.entries {
            let path = tree.path().join(&entry.rel);
            assert!(
                path.exists(),
                "EXPECTED.md would name `{}`, which was never written",
                entry.rel
            );
        }
    }

    #[test]
    fn filesystem_dated_entries_are_measured_not_left_blank() {
        let (_tree, plan) = generate(MediaTree::new(), Profile::Awkward, 3);
        let fs_dated: Vec<_> = plan
            .entries
            .iter()
            .filter_map(|e| match &e.expect {
                Expect::FiledFromFilesystem(d) => Some((&e.rel, d)),
                _ => None,
            })
            .collect();
        assert!(
            !fs_dated.is_empty(),
            "the awkward profile has undated files"
        );
        for (rel, dir) in fs_dated {
            assert!(
                NaiveDate::parse_from_str(dir, "%Y-%m-%d").is_ok(),
                "`{rel}` was left with an unresolved directory {dir:?}"
            );
        }
    }

    /// The filesystem fallback is predicted in UTC, the same zone every other
    /// date in `EXPECTED.md` is stated in.
    ///
    /// Both instants are chosen to fall on a different calendar day somewhere
    /// the CI matrix actually runs: the first is the day after in
    /// `Asia/Singapore`, the second the day before in `America/New_York`. So
    /// reading the mtime in the machine's zone — which is what this did until
    /// it was found contradicting the rest of the document — fails one of these
    /// two on either non-UTC leg. On the UTC leg it cannot fail, and that is
    /// the point of running the suite under three zones rather than one.
    #[test]
    fn the_filesystem_fallback_is_predicted_in_utc() {
        for (epoch, expected) in [(1_786_311_329, "2026-08-09"), (1_786_242_600, "2026-08-09")] {
            let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(epoch);
            assert_eq!(
                directory_of_system_time(t),
                expected,
                "a filesystem timestamp must be read in UTC; reading it in the machine's \
                 zone puts EXPECTED.md's two halves on different clocks, and the document \
                 then calls a correct run a defect"
            );
        }
    }

    #[test]
    fn the_awkward_profile_marks_its_malformed_files() {
        let (_tree, plan) = generate(MediaTree::new(), Profile::Awkward, 11);
        let marked = plan.entries.iter().filter(|e| e.malformed).count();
        assert!(
            marked >= 8,
            "a user who does not know these files are meant to be wrong will report a \
             correct result as a bug; only {marked} were marked"
        );
    }

    #[test]
    fn the_realistic_profile_is_entirely_well_formed() {
        let (_tree, plan) = generate(MediaTree::new(), Profile::Realistic, 5);
        let marked: Vec<_> = plan
            .entries
            .iter()
            .filter(|e| e.malformed)
            .map(|e| &e.rel)
            .collect();
        assert!(
            marked.is_empty(),
            "the realistic profile answers `would it file my library correctly`; \
             deliberately broken files do not belong in it: {marked:?}"
        );
    }

    #[test]
    fn expected_markdown_names_every_generated_file() {
        let (_tree, plan) = generate(MediaTree::new(), Profile::Awkward, 13);
        let doc = expected_markdown(&plan);
        for entry in &plan.entries {
            assert!(
                doc.contains(&entry.rel),
                "`{}` was generated and left out of EXPECTED.md",
                entry.rel
            );
        }
    }

    #[test]
    fn expected_markdown_warns_about_the_deliberately_broken() {
        let (_tree, plan) = generate(MediaTree::new(), Profile::Awkward, 17);
        let doc = expected_markdown(&plan);
        assert!(doc.contains("deliberately malformed"));
        assert!(doc.contains('⚠'));
    }

    #[test]
    fn profiles_round_trip_through_their_names() {
        for name in Profile::ALL {
            let parsed = Profile::parse(name).expect("ALL lists only parseable names");
            assert_eq!(&parsed.to_string(), name);
        }
        assert!(Profile::parse("REALISTIC").is_some(), "case-insensitive");
        assert!(Profile::parse("nonsense").is_none());
    }

    #[test]
    fn every_profile_generates_something() {
        for name in Profile::ALL {
            let profile = Profile::parse(name).unwrap();
            let (_tree, plan) = generate(MediaTree::new(), profile, 23);
            assert!(!plan.is_empty(), "{profile} generated nothing");
            assert!(
                !plan.definite().is_empty(),
                "{profile} produced no assertable expectation, so nothing could check it"
            );
        }
    }
}
