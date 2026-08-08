//! The whole user journey, once, over a library big enough to be a library.
//!
//! Every other suite here holds one thing still and asks one question about it.
//! This one asks the only question a person moving their photographs actually
//! has: *if I run this on my library, do I get my library back?* It builds
//! several hundred files across nested directories in every shape the tool
//! claims to handle, previews, commits, checks the tree against a golden list,
//! and then undoes the whole thing and checks the input is byte-for-byte what it
//! was.
//!
//! ## What makes this different from `organise.rs`
//!
//! Scale and composition. A defect that only appears when a duplicate group's
//! sidecar travels while a filesystem-dated file collides with a geocoded one
//! cannot be reached by a two-file fixture, and no amount of two-file fixtures
//! adds up to it. The three properties asserted here are the ones that are
//! *only* meaningful at scale:
//!
//! 1. **Conservation.** Every byte that went in comes out — the multiset of
//!    content hashes across input-plus-output is the same before and after, and
//!    every media file's content is present in the output tree.
//! 2. **The plan is what happened.** The destinations printed by the dry run are
//!    exactly the paths the committing run produced. A preview that under-reports
//!    is worse than no preview.
//! 3. **Reversibility.** After `mmm undo --commit` the input tree is
//!    byte-identical to the tree that went in, and the thread count changes
//!    nothing about any of it.
//!
//! ## Where the expected destinations come from
//!
//! Written out in the fixture declaration, beside the file they belong to, from
//! the same declared datetime — *not* by calling [`mmm::organiser::build_target_path`].
//! A test that computed its expectation with the code under test would pass
//! against any layout at all. The layout restated here is the documented default
//! (`%Y-%m-%d/` directories, `{date}-{time}{location}.{ext}` filenames), so a
//! change to either default breaks this suite loudly, which is the point.
//!
//! ## What is deliberately *not* pinned
//!
//! Three things in a run of this shape are not predictable from the declaration,
//! and pretending otherwise would make this suite flake rather than fail:
//!
//! * **Which member of a duplicate group is relocated.** Scan order is the
//!   filesystem's, not ours. Every duplicate group here is therefore built from
//!   files sharing a leaf name, so the *name* under `duplicates/` is settled even
//!   though the identity of the physical file is not.
//! * **The `duplicates/NNN/` group number,** which follows content-hash order.
//!   Asserted as a set of group directories and a multiset of leaf names.
//! * **The date directory of a filesystem-dated file,** which is the day the
//!   fixture was built. Asserted as "a date directory that is none of the
//!   fixture's own", which is true at any hour including midnight. Their *leaf*
//!   names are pinned, because every filesystem-dated fixture here carries a
//!   distinct extension and so cannot collide with its siblings.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Output;

use assert_cmd::Command;
use chrono::NaiveDateTime;
use tempfile::TempDir;

use common::{
    file_contents_by_marker, metadata_snapshot, naive, snapshot_tree, snapshot_tree_hashed,
    MediaTree, VideoSpec, XmpForm,
};
use mmm::geocoder::GeoLookup;
use mmm::reporter::{COMMIT_BANNER, DRY_RUN_BANNER};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A scratch output directory, kept alive by the returned `TempDir`.
fn scratch_output() -> (TempDir, std::path::PathBuf) {
    let scratch = TempDir::new().expect("creating scratch TempDir");
    let out = scratch.path().join("organised");
    (scratch, out)
}

/// The arguments every run in this suite shares.
///
/// `--timezone UTC` pins the one thing that would otherwise vary by machine: a
/// filesystem-dated file is read against the run's zone, and a suite that filed
/// those under the runner's local zone would put them in a different directory in
/// Singapore than in CI. Nothing else here is affected — every dated fixture
/// carries its own offset, and a file that states its offset is believed over any
/// configuration.
const COMMON_ARGS: &[&str] = &["--timezone", "UTC", "--no-prompt"];

fn run_mmm(input: &Path, output: &Path, extra: &[&str]) -> Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(COMMON_ARGS)
        .args(extra)
        .output()
        .expect("running mmm")
}

/// Preview only — no `--commit`.
fn preview(input: &Path, output: &Path) -> Output {
    run_mmm(input, output, &[])
}

/// Move the files.
fn commit(input: &Path, output: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["--commit"];
    args.extend_from_slice(extra);
    run_mmm(input, output, &args)
}

/// Put them all back.
fn undo(library: &Path) -> Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg("undo")
        .arg(library)
        .arg("--commit")
        .output()
        .expect("running mmm undo")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        stdout_of(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// The fixture declaration
// ---------------------------------------------------------------------------

/// Where a declared file must end up.
enum Landing {
    /// One destination, relative to the output root, known in full.
    At(String),
    /// The file has byte-identical twins. One copy is filed under its date; the
    /// rest are relocated under `duplicates/`, where only their leaf names are
    /// predictable — see the module doc.
    Deduplicated { dated: String, copies: Vec<String> },
    /// Dated from the filesystem, so only the leaf name is predictable.
    FilesystemDated { leaf: String },
    /// Not media, or a sidecar belonging to nothing: the run must leave it
    /// exactly where it is.
    StaysPut,
}

/// One provenance marker, the files carrying it, and what must become of them.
///
/// `sources` is a list rather than a path because a byte-identical copy carries
/// the marker of the file it was copied from — two files, one marker, which is
/// the honest description of what a duplicate is.
struct Declared {
    marker: String,
    sources: Vec<String>,
    landing: Landing,
}

/// A synthesised library, and the full statement of where every file in it goes.
struct Scenario {
    tree: MediaTree,
    declared: Vec<Declared>,
}

impl Scenario {
    /// Every path the declaration says it built, sorted.
    ///
    /// Checked against the tree on disk before anything is run, so that a file
    /// added to the fixture and forgotten in the declaration fails here — where
    /// the message says so — rather than surviving as an unasserted passenger.
    fn declared_sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .declared
            .iter()
            .flat_map(|entry| entry.sources.iter().cloned())
            .collect();
        out.sort();
        out
    }

    /// Every date directory the declaration expects, so a filesystem-dated file
    /// can be asserted to have landed somewhere else.
    fn fixture_date_dirs(&self) -> BTreeSet<String> {
        let mut dirs = BTreeSet::new();
        for entry in &self.declared {
            let dest = match &entry.landing {
                Landing::At(dest) => dest,
                Landing::Deduplicated { dated, .. } => dated,
                Landing::FilesystemDated { .. } | Landing::StaysPut => continue,
            };
            if let Some((dir, _)) = dest.split_once('/') {
                dirs.insert(dir.to_string());
            }
        }
        dirs
    }

    /// Sorted destinations of every file with exactly one predictable landing.
    fn golden_tree(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .declared
            .iter()
            .filter_map(|entry| match &entry.landing {
                Landing::At(dest) => Some(dest.clone()),
                Landing::Deduplicated { dated, .. } => Some(dated.clone()),
                Landing::FilesystemDated { .. } | Landing::StaysPut => None,
            })
            .collect();
        out.sort();
        out
    }
}

// ---------------------------------------------------------------------------
// The documented default layout, restated
// ---------------------------------------------------------------------------

/// `%Y-%m-%d/{date}-{time}.{ext}` — the shipped default, written out rather than
/// computed by the code under test. See the module doc.
fn dated(dt: NaiveDateTime, ext: &str) -> String {
    format!(
        "{}/{}-{}.{ext}",
        dt.format("%Y-%m-%d"),
        dt.format("%Y-%m-%d"),
        dt.format("%H%M%S")
    )
}

/// The same, with the geocoded `{location}` suffix a file with coordinates gets.
fn dated_at(dt: NaiveDateTime, ext: &str, location: &str) -> String {
    format!(
        "{}/{}-{}-{location}.{ext}",
        dt.format("%Y-%m-%d"),
        dt.format("%Y-%m-%d"),
        dt.format("%H%M%S")
    )
}

/// A sidecar named for its parent's *stem* lands beside it under the parent's
/// new stem — `IMG_1234.xmp` beside `IMG_1234.cr2`.
fn beside_stem(parent_dest: &str, ext: &str) -> String {
    let (dir, leaf) = parent_dest.split_once('/').expect("a dated destination");
    let stem = leaf
        .rsplit_once('.')
        .expect("a dated leaf has an extension")
        .0;
    format!("{dir}/{stem}.{ext}")
}

/// A sidecar named for its parent's *whole filename* keeps that shape —
/// `IMG_1234.cr2.xmp` beside `IMG_1234.cr2`.
fn beside_full_name(parent_dest: &str, ext: &str) -> String {
    format!("{parent_dest}.{ext}")
}

/// An XMP date property value, offset and all.
fn xmp_stamp(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

// ---------------------------------------------------------------------------
// The library
// ---------------------------------------------------------------------------

/// Rolls of film — one per month of 2023, fifteen frames each.
const ROLLS: u32 = 12;
const FRAMES_PER_ROLL: u32 = 15;

/// Coordinates whose geocoded names the GPS fixtures are filed under. Four
/// continents, so a suffix that came out of the wrong hemisphere is visible.
const CITIES: [(f64, f64); 4] = [
    (48.8584, 2.2945),    // Paris
    (-33.8688, 151.2093), // Sydney
    (51.5074, -0.1278),   // London
    (40.7128, -74.0060),  // New York
];

/// Build the library and state, file by file, where every one of its files goes.
///
/// Each family gets its own year, so no two families can collide in the output
/// tree and a destination that appears twice is a defect rather than a fixture
/// accident.
#[allow(
    clippy::too_many_lines,
    reason = "this function is the fixture declaration, and a declaration reads better as one \
              list than as a dozen helpers that each hide half of a file's story"
)]
fn build_library(geo: &GeoLookup) -> Scenario {
    let mut tree = MediaTree::new();
    let mut declared: Vec<Declared> = Vec::new();

    let declare = |declared: &mut Vec<Declared>, source: &str, landing: Landing| {
        declared.push(Declared {
            marker: source.to_string(),
            sources: vec![source.to_string()],
            landing,
        });
    };

    // --- 2023: twelve rolls of ordinary EXIF-dated JPEGs, nested three deep ---
    for roll in 1..=ROLLS {
        for frame in 0..FRAMES_PER_ROLL {
            let dt = naive(2023, roll, 15, 9, frame, 0);
            let rel = format!("library/2023/roll-{roll:02}/img-{roll:02}{frame:02}.jpg");
            tree = tree.jpeg_with_exif(&rel, dt, None);
            declare(&mut declared, &rel, Landing::At(dated(dt, "jpg")));
        }

        // The first frame of every roll has a darktable sidecar named for its
        // stem; the second frame of the first four has one named for its whole
        // filename. Both conventions are in the wild and they land differently.
        let first = naive(2023, roll, 15, 9, 0, 0);
        let stem_side = format!("library/2023/roll-{roll:02}/img-{roll:02}00.xmp");
        tree = tree.sidecar(&stem_side, b"<!-- edit history -->");
        declare(
            &mut declared,
            &stem_side,
            Landing::At(beside_stem(&dated(first, "jpg"), "xmp")),
        );

        if roll <= 4 {
            let second = naive(2023, roll, 15, 9, 1, 0);
            let full_side = format!("library/2023/roll-{roll:02}/img-{roll:02}01.jpg.xmp");
            tree = tree.sidecar(&full_side, b"<!-- edit history -->");
            declare(
                &mut declared,
                &full_side,
                Landing::At(beside_full_name(&dated(second, "jpg"), "xmp")),
            );
        }
    }

    // --- 2022: HEIC stills, the container every iPhone since 2017 writes ---
    for i in 0..30_u32 {
        let dt = naive(2022, 6, 1 + i / 10, 10, i % 10, 0);
        let rel = format!("library/heic/IMG_{i:04}.heic");
        tree = tree.heic_with_exif(&rel, dt, None);
        declare(&mut declared, &rel, Landing::At(dated(dt, "heic")));
    }

    // --- 2021: TIFF-based RAW, dated by the XMP beside it ---
    //
    // `nom-exif` reads no bare TIFF, so the date in the file itself is
    // deliberately a different year from the sidecar's: a run that read the RAW
    // and ignored the sidecar would file these under 2019 and be caught here.
    for i in 0..24_u32 {
        let dt = naive(2021, 3, 1 + i / 8, 8, i % 8, 0);
        let (ext, signature): (&str, Option<&[u8; 4]>) = match i % 3 {
            0 => ("cr2", Some(b"CR\x02\x00")),
            1 => ("dng", None),
            _ => ("nef", None),
        };
        let rel = format!("library/raw/{ext}/DSC_{i:04}.{ext}");
        tree = tree.tiff_raw(
            &rel,
            signature,
            naive(2019, 1, 1, 0, 0, 0),
            Some("+00:00"),
            None,
        );
        declare(&mut declared, &rel, Landing::At(dated(dt, ext)));

        let side = format!("library/raw/{ext}/DSC_{i:04}.xmp");
        let form = if i % 2 == 0 {
            XmpForm::Attribute
        } else {
            XmpForm::Element
        };
        tree = tree.xmp(&side, form, &[("exif:DateTimeOriginal", &xmp_stamp(dt))]);
        declare(
            &mut declared,
            &side,
            Landing::At(beside_stem(&dated(dt, ext), "xmp")),
        );
    }

    // --- 2020: MP4 clips dated from the container's own UTC `mvhd` ---
    for i in 0..20_u32 {
        let dt = naive(2020, 4, 1 + i / 10, 12, i % 10, 30);
        let rel = format!("library/video/mp4/clip-{i:03}.mp4");
        tree = tree.iso_video(&rel, &VideoSpec::mp4(dt));
        declare(&mut declared, &rel, Landing::At(dated(dt, "mp4")));

        // A camcorder thumbnail rides along with the first four.
        if i < 4 {
            let thm = format!("library/video/mp4/clip-{i:03}.thm");
            tree = tree.sidecar(&thm, b"thumbnail bytes");
            declare(
                &mut declared,
                &thm,
                Landing::At(beside_stem(&dated(dt, "mp4"), "thm")),
            );
        }
    }

    // --- 2019: MOV clips carrying the Apple keys, offset and location ---
    for i in 0..6_u32 {
        let (lat, lon) = CITIES[(i % 4) as usize];
        let dt = naive(2019, 5, 4, 18, 22, i);
        let stamp = format!("{}+08:00", dt.format("%Y-%m-%dT%H:%M:%S"));
        let iso6709 = format!("{lat:+.4}{lon:+.4}/");
        let rel = format!("library/video/mov/MVI_{i:03}.mov");
        tree = tree.iso_video(
            &rel,
            &VideoSpec {
                brand: *b"qt  ",
                mvhd_utc: Some(naive(2016, 1, 1, 0, 0, 0)),
                apple_creationdate: Some(&stamp),
                location: Some(&iso6709),
            },
        );
        declare(
            &mut declared,
            &rel,
            Landing::At(dated_at(dt, "mov", &geo_part(geo, lat, lon))),
        );
    }

    // --- 2018: GPS-tagged JPEGs, whose filenames gain a place ---
    for i in 0..20_u32 {
        let (lat, lon) = CITIES[(i % 4) as usize];
        let dt = naive(2018, 8, 1 + i / 10, 7, i % 10, 0);
        let rel = format!("library/gps/geo-{i:02}.jpg");
        tree = tree.jpeg_with_exif(&rel, dt, Some((lat, lon)));
        declare(
            &mut declared,
            &rel,
            Landing::At(dated_at(dt, "jpg", &geo_part(geo, lat, lon))),
        );
    }

    // --- 2017: two imports of the same card, so ten pairs and one trio ---
    //
    // Both members of a pair share a leaf name on purpose: which of them the run
    // relocates depends on scan order, so only a shared name makes the
    // `duplicates/` entry predictable. See the module doc.
    for i in 0..10_u32 {
        let dt = naive(2017, 2, 1 + i / 5, 6, i % 5, 0);
        let leaf = format!("dup-{i:02}.jpg");
        let first = format!("library/import-a/{leaf}");
        let second = format!("library/import-b/{leaf}");
        tree = tree
            .jpeg_with_exif(&first, dt, None)
            .duplicate_of(&second, &first);
        declared.push(Declared {
            marker: first.clone(),
            sources: vec![first, second],
            landing: Landing::Deduplicated {
                dated: dated(dt, "jpg"),
                copies: vec![leaf],
            },
        });
    }

    let trio_dt = naive(2017, 3, 10, 6, 30, 0);
    let trio_a = "library/import-a/trio.jpg".to_string();
    let trio_b = "library/import-b/trio.jpg".to_string();
    let trio_c = "library/import-c/trio.jpg".to_string();
    tree = tree
        .jpeg_with_exif(&trio_a, trio_dt, None)
        .duplicate_of(&trio_b, &trio_a)
        .duplicate_of(&trio_c, &trio_a);
    declared.push(Declared {
        marker: trio_a.clone(),
        sources: vec![trio_a, trio_b, trio_c],
        // Two relocated copies of one name: the second takes the collision
        // suffix, which is a pure function of the name and so is predictable
        // even though which physical file gets it is not.
        landing: Landing::Deduplicated {
            dated: dated(trio_dt, "jpg"),
            copies: vec!["trio.jpg".to_string(), "trio-1.jpg".to_string()],
        },
    });

    // --- The undated: six files with no date of their own ---
    //
    // Every one carries a different extension, so their destinations differ even
    // though they were all created in the same second and are all filed under
    // today. Six ways of having no date, which the run reports differently and
    // files identically.
    tree = tree
        .jpeg_without_exif("library/nodate/scan-a.jpg")
        .jpeg_without_exif("library/nodate/scan-b.jpeg")
        .jpeg_with_exif_but_no_date("library/nodate/stripped.png", None)
        .jpeg_with_unreadable_date("library/nodate/flat-battery.tiff", "0000:00:00 00:00:00")
        .jpeg_with_corrupt_exif("library/nodate/corrupt.bmp")
        .video("library/nodate/mystery.avi", b"not a real AVI");
    for (rel, ext) in [
        ("library/nodate/scan-a.jpg", "jpg"),
        ("library/nodate/scan-b.jpeg", "jpeg"),
        ("library/nodate/stripped.png", "png"),
        ("library/nodate/flat-battery.tiff", "tiff"),
        ("library/nodate/corrupt.bmp", "bmp"),
        ("library/nodate/mystery.avi", "avi"),
    ] {
        declare(
            &mut declared,
            rel,
            Landing::FilesystemDated {
                leaf: ext.to_string(),
            },
        );
    }

    // --- Files the run must not touch ---
    for i in 0..3_u32 {
        let rel = format!("library/orphans/nothing-of-mine-{i}.xmp");
        tree = tree.sidecar(&rel, b"<!-- an edit to a photograph that is gone -->");
        declare(&mut declared, &rel, Landing::StaysPut);
    }
    for (rel, body) in [
        ("library/notes.txt", &b"where everything came from"[..]),
        ("library/2023/README.md", &b"# the 2023 shoot"[..]),
        (
            "library/import-a/checksums.sha256",
            &b"deadbeef  dup-00.jpg"[..],
        ),
        ("library/video/index.csv", &b"clip,notes"[..]),
    ] {
        tree = tree.non_media(rel, body);
        declare(&mut declared, rel, Landing::StaysPut);
    }

    // Directories with nothing in them: the scan walks straight past.
    tree = tree
        .empty_dir("library/2024-to-do")
        .empty_dir("library/video/mov/proxies");

    Scenario { tree, declared }
}

/// The `{location}` suffix the geocoder gives a coordinate.
///
/// Read from the geocoder rather than hard-coded, exactly as `organise.rs` does:
/// the city names are a property of the bundled `GeoNames` dataset, not of this
/// crate, and a dataset refresh must not turn this suite red.
fn geo_part(geo: &GeoLookup, lat: f64, lon: f64) -> String {
    geo.lookup(lat, lon)
        .unwrap_or_else(|| panic!("the geocoder returned nothing for {lat},{lon}"))
        .filename_part
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Split a hashed snapshot into a multiset of content hashes.
fn hash_multiset(root: &Path) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for line in snapshot_tree_hashed(root) {
        let (_path, hash) = line
            .rsplit_once("  ")
            .expect("a hashed snapshot entry is `<path>  <hash>`");
        *counts.entry(hash.to_string()).or_insert(0) += 1;
    }
    counts
}

/// A duplicate group's `manifest.txt` is written by the run, not carried in from
/// the fixture, so it is the one file in the output that has no counterpart in
/// the input. Everything else must be conserved exactly.
fn is_manifest(rel: &str) -> bool {
    rel.starts_with("duplicates/") && rel.ends_with("/manifest.txt")
}

/// Content hashes of the output tree, less the manifests the run itself wrote.
fn carried_hashes(output: &Path) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for line in snapshot_tree_hashed(output) {
        let (path, hash) = line
            .rsplit_once("  ")
            .expect("a hashed snapshot entry is `<path>  <hash>`");
        if is_manifest(path) {
            continue;
        }
        *counts.entry(hash.to_string()).or_insert(0) += 1;
    }
    counts
}

/// Nothing was lost, duplicated or invented: the bytes across input and output
/// are the bytes that went in.
///
/// Stated over the union rather than over the output alone, because the files the
/// run is *required* to leave behind — a non-media file, a sidecar belonging to
/// nothing — are still part of the library and still have to be there afterwards.
fn assert_conserved(input: &Path, output: &Path, before: &BTreeMap<String, usize>, step: &str) {
    let mut after = hash_multiset(input);
    for (hash, count) in carried_hashes(output) {
        *after.entry(hash).or_insert(0) += count;
    }

    assert_eq!(
        after.len(),
        before.len(),
        "after {step} the library holds {} distinct contents, not the {} it started with",
        after.len(),
        before.len()
    );
    assert_eq!(
        after, *before,
        "after {step} the content of the library is not the content that went into it"
    );

    let total_before: usize = before.values().sum();
    let total_after: usize = after.values().sum();
    assert_eq!(
        total_after, total_before,
        "after {step} the library holds {total_after} files, not the {total_before} it started with"
    );
}

/// Every file the run was meant to move is present, by content, in the output.
///
/// The counterpart to [`assert_conserved`], and not implied by it: a run that
/// left the whole library in place would conserve every byte and organise
/// nothing.
fn assert_media_reached_the_output(scenario: &Scenario, output: &Path) {
    let landed = file_contents_by_marker(output);
    for entry in &scenario.declared {
        if matches!(entry.landing, Landing::StaysPut) {
            continue;
        }
        assert!(
            landed.contains_key(&entry.marker),
            "{} is nowhere in the output tree",
            entry.marker
        );
    }
}

/// The whole output tree, against the declaration.
#[allow(
    clippy::too_many_lines,
    reason = "one tree, checked in four parts that only mean anything together"
)]
fn assert_golden_tree(scenario: &Scenario, output: &Path) {
    let actual = snapshot_tree(output);
    let fixture_dates = scenario.fixture_date_dirs();

    let mut in_duplicates: Vec<String> = Vec::new();
    let mut filesystem_dated: Vec<String> = Vec::new();
    let mut dated: Vec<String> = Vec::new();

    for rel in actual {
        if rel.starts_with("duplicates/") {
            in_duplicates.push(rel);
            continue;
        }
        let dir = rel.split_once('/').map_or("", |(dir, _)| dir);
        if fixture_dates.contains(dir) {
            dated.push(rel);
        } else {
            filesystem_dated.push(rel);
        }
    }

    // 1. Everything with a predictable destination, exactly.
    dated.sort();
    assert_eq!(
        dated,
        scenario.golden_tree(),
        "the organised tree is not the tree the fixture declared"
    );

    // 2. The undated, by leaf name. Their directory is the day the fixture was
    //    built, which cannot be written down here — but it must at least be a
    //    date directory, and not one of the fixture's own.
    let mut expected_undated: Vec<String> = scenario
        .declared
        .iter()
        .filter_map(|entry| match &entry.landing {
            Landing::FilesystemDated { leaf } => Some(leaf.clone()),
            _ => None,
        })
        .collect();
    expected_undated.sort();

    let mut actual_undated: Vec<String> = filesystem_dated
        .iter()
        .map(|rel| {
            let (dir, leaf) = rel.split_once('/').unwrap_or(("", rel));
            assert!(
                looks_like_a_date_directory(dir),
                "{rel} is neither under a fixture date nor under a date directory at all"
            );
            leaf.rsplit_once('.')
                .map_or_else(String::new, |(_, ext)| ext.to_string())
        })
        .collect();
    actual_undated.sort();
    assert_eq!(
        actual_undated, expected_undated,
        "the files with no date of their own did not all reach a dated directory"
    );

    // 3. The duplicate groups: one manifest each, and the leaf names the
    //    declaration expects — but not a fixed group number, which follows
    //    content-hash order.
    let mut manifests: BTreeSet<String> = BTreeSet::new();
    let mut relocated: Vec<String> = Vec::new();
    for rel in &in_duplicates {
        let group = rel
            .split('/')
            .nth(1)
            .unwrap_or_else(|| panic!("{rel} is directly inside duplicates/"));
        if is_manifest(rel) {
            assert!(
                manifests.insert(group.to_string()),
                "{group} has more than one manifest"
            );
        } else {
            relocated.push(
                rel.rsplit_once('/')
                    .map_or(rel.clone(), |(_, l)| l.to_string()),
            );
        }
    }

    let expected_groups: usize = scenario
        .declared
        .iter()
        .filter(|e| matches!(e.landing, Landing::Deduplicated { .. }))
        .count();
    assert_eq!(
        manifests.len(),
        expected_groups,
        "expected one manifest per duplicate group, found {manifests:?}"
    );

    let mut expected_relocated: Vec<String> = scenario
        .declared
        .iter()
        .filter_map(|entry| match &entry.landing {
            Landing::Deduplicated { copies, .. } => Some(copies.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    expected_relocated.sort();
    relocated.sort();
    assert_eq!(
        relocated, expected_relocated,
        "the files relocated under duplicates/ are not the ones the fixture declared"
    );

    // 4. And it is *those* files at those paths, not merely files of those names.
    let landed = file_contents_by_marker(output);
    for entry in &scenario.declared {
        match &entry.landing {
            Landing::At(dest) => assert_eq!(
                landed.get(&entry.marker).map(Vec::as_slice),
                Some([dest.clone()].as_slice()),
                "{} did not land at {dest}",
                entry.marker
            ),
            Landing::Deduplicated { dated, copies } => {
                let paths = landed
                    .get(&entry.marker)
                    .unwrap_or_else(|| panic!("{} is nowhere in the output", entry.marker));
                assert_eq!(
                    paths.len(),
                    1 + copies.len(),
                    "{} should have one filed copy and {} relocated, got {paths:?}",
                    entry.marker,
                    copies.len()
                );
                assert!(
                    paths.contains(dated),
                    "{} has no copy filed at {dated}; got {paths:?}",
                    entry.marker
                );
                let mut leaves: Vec<String> = paths
                    .iter()
                    .filter(|p| p.starts_with("duplicates/"))
                    .map(|p| p.rsplit_once('/').map_or(p.clone(), |(_, l)| l.to_string()))
                    .collect();
                leaves.sort();
                let mut expected = copies.clone();
                expected.sort();
                assert_eq!(
                    leaves, expected,
                    "{}'s relocated copies are misnamed",
                    entry.marker
                );
            }
            Landing::FilesystemDated { leaf: ext } => {
                let paths = landed
                    .get(&entry.marker)
                    .unwrap_or_else(|| panic!("{} is nowhere in the output", entry.marker));
                assert_eq!(paths.len(), 1, "{} landed more than once", entry.marker);
                assert!(
                    paths[0].ends_with(&format!(".{ext}")),
                    "{} lost its extension: {}",
                    entry.marker,
                    paths[0]
                );
            }
            Landing::StaysPut => assert!(
                !landed.contains_key(&entry.marker),
                "{} was moved into the output tree and should not have been",
                entry.marker
            ),
        }
    }
}

fn looks_like_a_date_directory(dir: &str) -> bool {
    let parts: Vec<&str> = dir.split('-').collect();
    parts.len() == 3
        && [4, 2, 2]
            .iter()
            .zip(&parts)
            .all(|(&width, part)| part.len() == width && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The declaration accounts for every file in the fixture, and for no others.
///
/// Without this an undeclared fixture file would simply not be asserted on, and
/// the suite would read as though it covered a library it only partly described.
fn assert_declaration_covers_the_tree(scenario: &Scenario, input: &Path) {
    assert_eq!(
        scenario.declared_sources(),
        snapshot_tree(input),
        "the declaration and the fixture on disk are not the same library"
    );
}

/// Everything the run said it would not move is still where it was.
fn assert_stayed_put(scenario: &Scenario, input: &Path) {
    let still_there = file_contents_by_marker(input);
    for entry in &scenario.declared {
        if !matches!(entry.landing, Landing::StaysPut) {
            continue;
        }
        assert_eq!(
            still_there.get(&entry.marker).map(Vec::as_slice),
            Some([entry.marker.clone()].as_slice()),
            "{} did not stay where it was",
            entry.marker
        );
    }
}

/// The destinations the dry run printed, relative to the output root.
///
/// Both the per-file lines and the indented `[sidecar]` lines under them, which
/// is the whole plan: a preview that listed the photographs and not the sidecars
/// travelling with them would be under-reporting exactly the files a user is
/// least likely to check.
fn planned_destinations(stdout: &str, output: &Path) -> Vec<String> {
    let prefix = format!("{}/", output.display());
    let mut out: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" → ")?;
            // `print_duplicates` also prints an arrow, with nothing to its left
            // and an *input* path to its right. Neither test below would be
            // meaningful if those were folded in.
            if left.trim().is_empty() {
                return None;
            }
            right.strip_prefix(&prefix).map(ToString::to_string)
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// The journey
// ---------------------------------------------------------------------------

#[test]
fn the_full_library_journey_is_reversible() {
    let geo = GeoLookup::new();
    let scenario = build_library(&geo);
    let input = scenario.tree.path().to_path_buf();
    let (_scratch, out_dir) = scratch_output();

    let before_tree = snapshot_tree_hashed(&input);
    let before_hashes = hash_multiset(&input);
    assert_declaration_covers_the_tree(&scenario, &input);
    assert!(
        before_tree.len() > 300,
        "the fixture is meant to be a library, not a handful of files: {} files",
        before_tree.len()
    );

    // --- Preview -----------------------------------------------------------
    let previewed = preview(&input, &out_dir);
    assert_ok(&previewed, "the dry run");
    let plan = stdout_of(&previewed);

    assert!(
        plan.contains(DRY_RUN_BANNER) && !plan.contains(COMMIT_BANNER),
        "the preview did not announce its posture"
    );
    assert_eq!(
        snapshot_tree_hashed(&input),
        before_tree,
        "the dry run modified the input tree"
    );
    assert!(
        !out_dir.exists(),
        "the dry run created the output directory it was only previewing into"
    );

    let planned = planned_destinations(&plan, &out_dir);
    assert!(
        !planned.is_empty(),
        "the dry run printed no plan at all:\n{plan}"
    );

    // --- Commit ------------------------------------------------------------
    let committed = commit(&input, &out_dir, &[]);
    assert_ok(&committed, "the committing run");
    assert!(
        stdout_of(&committed).contains(COMMIT_BANNER),
        "the committing run did not announce its posture"
    );

    // Conservation first, deliberately: "did anything of yours disappear" is a
    // more important question than "is the tree the shape we declared", and it
    // should be the first failure a reader of a broken run sees.
    assert_conserved(&input, &out_dir, &before_hashes, "the committing run");
    assert_media_reached_the_output(&scenario, &out_dir);
    assert_golden_tree(&scenario, &out_dir);
    assert_stayed_put(&scenario, &input);

    // The plan was the truth: every destination it printed is a file that now
    // exists, and every organised file was in it. Duplicates are excluded on
    // both sides — they are reported by `print_duplicates`, not planned as moves,
    // and `undo.rs` is where their journalling is proved.
    let organised: Vec<String> = snapshot_tree(&out_dir)
        .into_iter()
        .filter(|rel| !rel.starts_with("duplicates/"))
        .collect();
    assert_eq!(
        planned, organised,
        "the dry run's plan is not what the committing run did"
    );

    // --- Undo --------------------------------------------------------------
    let undone = undo(&out_dir);
    assert_ok(&undone, "the undo");

    assert_eq!(
        snapshot_tree_hashed(&input),
        before_tree,
        "after undo the input tree is not byte-for-byte the tree that went in"
    );
    assert_conserved(&input, &out_dir, &before_hashes, "the undo");

    // Nothing of the library is left behind in the output — only the run's own
    // record of what it did, and the per-group manifests, which are not moves and
    // were never journalled as any.
    let left_behind: Vec<String> = snapshot_tree(&out_dir)
        .into_iter()
        .filter(|rel| !is_manifest(rel))
        .collect();
    assert!(
        left_behind.is_empty(),
        "undo left files in the output tree: {left_behind:?}"
    );
    assert!(
        !metadata_snapshot(&out_dir).is_empty(),
        "the run left no journal, so nothing would have been reversible"
    );
}

/// The thread count is a throughput knob, not a behaviour one.
///
/// Run the same library twice against the same fixture — the parallel default,
/// then `--threads 1` — with an undo in between so the second run starts from the
/// tree the first one was given. Anything that differs between the two is a
/// concurrency defect, because nothing else changed.
#[test]
fn the_thread_count_does_not_change_the_outcome() {
    let geo = GeoLookup::new();
    let scenario = build_library(&geo);
    let input = scenario.tree.path().to_path_buf();

    let before_tree = snapshot_tree_hashed(&input);
    let before_hashes = hash_multiset(&input);
    assert_declaration_covers_the_tree(&scenario, &input);

    let (_scratch_parallel, parallel_dir) = scratch_output();
    assert_ok(
        &commit(&input, &parallel_dir, &[]),
        "the run at the default thread count",
    );
    let parallel_tree = snapshot_tree(&parallel_dir);
    let parallel_landed = file_contents_by_marker(&parallel_dir);
    assert_conserved(&input, &parallel_dir, &before_hashes, "the parallel run");

    assert_ok(&undo(&parallel_dir), "the undo after the parallel run");
    assert_eq!(
        snapshot_tree_hashed(&input),
        before_tree,
        "the parallel run was not fully reversed, so the serial run starts from a different tree"
    );

    let (_scratch_serial, serial_dir) = scratch_output();
    assert_ok(
        &commit(&input, &serial_dir, &["--threads", "1"]),
        "the run at --threads 1",
    );
    let serial_tree = snapshot_tree(&serial_dir);
    let serial_landed = file_contents_by_marker(&serial_dir);
    assert_conserved(&input, &serial_dir, &before_hashes, "the serial run");

    assert_eq!(
        serial_tree, parallel_tree,
        "--threads 1 produced a different tree from the parallel default"
    );
    assert_eq!(
        serial_landed, parallel_landed,
        "--threads 1 put different files at the same paths as the parallel default"
    );

    // Both trees satisfy the declaration, not merely each other: two runs that
    // agreed on the same wrong answer would pass the comparison above.
    assert_golden_tree(&scenario, &serial_dir);

    assert_ok(&undo(&serial_dir), "the undo after the serial run");
    assert_eq!(
        snapshot_tree_hashed(&input),
        before_tree,
        "after undo the input tree is not byte-for-byte the tree that went in"
    );
}
