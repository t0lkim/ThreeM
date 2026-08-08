//! Integration suite for XMP/AAE/THM sidecar co-movement, driven through the
//! real `mmm` binary.
//!
//! ## What is actually at stake
//!
//! A sidecar is bound to its parent by *filename* and by nothing else. There is
//! no identifier inside an `.xmp` naming the RAW file it describes, no checksum,
//! no back-reference — the pairing is `IMG_1234.cr2` next to `IMG_1234.xmp`, and
//! that is the whole of it. So an organiser that renames the photograph has
//! already broken the link by the time it decides what to do with the sidecar,
//! and the only question left is whether it re-establishes it.
//!
//! That makes the interesting assertion here **not** "the sidecar moved". It is
//! "the sidecar arrived under a name that still pairs with its parent's new
//! one", and those two come apart in every way that matters: a sidecar moved to
//! the right directory under its old name is as detached as one left behind, and
//! a sidecar renamed to the parent's *planned* name is detached whenever
//! collision resolution gave the parent a suffix instead.
//!
//! Every test below therefore pins both paths — parent and sidecar — as one
//! pair, and reads them out of the tree by their embedded markers rather than by
//! checking that a file of the expected name exists. A file of the expected name
//! is exactly what a broken implementation also produces.
//!
//! ## Why the binary and not the library
//!
//! `sidecar.rs` unit-tests the pairing rules and the name derivation against
//! constructed paths, and covers them thoroughly. None of that establishes what
//! this suite is for: that the scanner collects sidecars at all, that the index
//! reaches the planner, that the moves are journalled, and that `undo` puts both
//! files back. Those are four separate wirings between a directory entry and a
//! destination, and only a run through `main` crosses all of them.
//!
//! ## The environment is an input, so it is controlled
//!
//! Every command runs `--no-config` with the inherited `MMM_` variables
//! stripped. A developer's own `sidecars = false` — in
//! `~/.config/mmm/config.toml` or exported in the shell — would otherwise
//! silently turn off the feature these tests are about, and the whole suite
//! would pass by asserting nothing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use assert_cmd::Command;
use tempfile::TempDir;

use common::{file_contents_by_marker, journals_in, naive, snapshot_tree, MediaTree, XmpForm};
use mmm::journal::{IntentKind, JournalEntry};
use mmm::reporter::{
    ORPHAN_SIDECAR_HEADING, SIDECARS_FOUND_LABEL, SIDECARS_MOVED_LABEL, SIDECAR_ORPHANS_LABEL,
    SIDECAR_TAG,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

/// Assert the process exited 0, printing both streams if it did not.
fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Preview `input`, returning what the run printed.
fn preview(input: &Path, extra: &[&str]) -> String {
    let out = mmm(input).args(extra).output().expect("previewing");
    assert_ok(&out, "a preview");
    stdout_of(&out)
}

/// What a committing run produced: where each marked fixture landed, and what
/// the run said.
struct Organised {
    /// Kept alive so the output tree outlives the assertions.
    _dir: TempDir,
    output: std::path::PathBuf,
    landed: BTreeMap<String, Vec<String>>,
    stdout: String,
}

impl Organised {
    /// Where the fixture declared at `rel` now is, relative to the output tree.
    ///
    /// Panics naming the whole tree when it is not there, because "the sidecar
    /// vanished" and "the sidecar landed somewhere else" are the two failures
    /// this suite exists to tell apart and a bare `None` tells neither.
    fn at(&self, rel: &str) -> &str {
        let landed = self
            .landed
            .get(rel)
            .unwrap_or_else(|| panic!("{rel} is not in the output tree: {:#?}", self.landed));
        assert_eq!(landed.len(), 1, "{rel} landed more than once: {landed:?}");
        &landed[0]
    }
}

/// Organise `input` into a fresh output directory.
///
/// This *moves* the fixtures out of `input`, so a preview of the same tree has
/// to be taken before this is called, not after.
fn organise(input: &Path, extra: &[&str]) -> Organised {
    let dir = TempDir::new().expect("creating output TempDir");
    let output = dir.path().join("out");

    let out = mmm(input)
        .arg("-o")
        .arg(&output)
        .arg("--commit")
        .arg("--no-prompt")
        .args(extra)
        .output()
        .expect("organising");
    assert_ok(&out, "a committing run");

    Organised {
        landed: file_contents_by_marker(&output),
        stdout: stdout_of(&out),
        output,
        _dir: dir,
    }
}

/// The stem of a path — `2024-03-15/2024-03-15-143000.jpg` → `2024-03-15-143000`.
fn stem_of(rel: &str) -> &str {
    Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| panic!("{rel} has no stem"))
}

/// The directory of a path, `/`-separated, or the empty string at the root.
fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

// ---------------------------------------------------------------------------
// The stem convention — IMG_1234.jpg + IMG_1234.xmp
// ---------------------------------------------------------------------------

/// The headline case, and the one assertion that is the whole feature: both
/// files land in the same directory, under the same *new* stem, with the
/// sidecar keeping its own extension.
///
/// Derived from the parent's actual destination rather than compared against a
/// literal, so this stays an assertion about the pairing rather than about
/// today's default filename format.
#[test]
fn a_stem_sidecar_lands_beside_its_parent_under_the_parents_new_name() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_1234.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);
    let parent = run.at("IMG_1234.jpg");
    let sidecar = run.at("IMG_1234.xmp");

    assert_eq!(
        dir_of(sidecar),
        dir_of(parent),
        "the sidecar must land in its parent's directory; parent {parent}, sidecar {sidecar}"
    );
    assert_eq!(
        stem_of(sidecar),
        stem_of(parent),
        "the sidecar must take the parent's new stem, or the pair is severed; \
         parent {parent}, sidecar {sidecar}"
    );
    assert_eq!(
        Path::new(sidecar).extension().and_then(|e| e.to_str()),
        Some("xmp"),
        "the sidecar must keep its own extension; got {sidecar}"
    );

    // And nothing is left in the source tree — a copy in each place would pair
    // in both and be the worst of the three outcomes.
    assert!(
        snapshot_tree(tree.path()).is_empty(),
        "the source tree should be empty; got {:?}",
        snapshot_tree(tree.path())
    );
}

/// The Apple and camcorder spellings, and the case where one photograph has
/// more than one companion. All three are the same category and must be
/// handled identically — which is exactly what a per-format implementation
/// would get wrong.
#[test]
fn aae_and_thm_sidecars_travel_on_the_same_terms_as_xmp() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_1234.AAE", b"<plist/>")
        .sidecar("IMG_1234.thm", b"thumbnail bytes");

    let run = organise(tree.path(), &[]);
    let parent = run.at("IMG_1234.jpg");

    for (declared, extension) in [("IMG_1234.AAE", "aae"), ("IMG_1234.thm", "thm")] {
        let landed = run.at(declared);
        assert_eq!(stem_of(landed), stem_of(parent), "{declared} → {landed}");
        assert_eq!(
            Path::new(landed).extension().and_then(|e| e.to_str()),
            Some(extension),
            "{declared} must keep its extension, lowercased like every other one the tool \
             writes; got {landed}"
        );
    }
}

/// A photograph whose destination name is already taken lands under a `-1`
/// suffix, and its sidecar has to take the *same* suffix.
///
/// This is the case that separates "derived from where the parent was planned to
/// go" from "derived from where it actually went", and every other test here is
/// blind to the difference. Get it wrong and the suffix that saved the
/// photograph is the thing that unpairs it from its edits.
///
/// The occupant is a photograph's name and nothing else, so only the *parent*
/// contends for a name. Two contending photographs, each with a sidecar, would
/// not do: their sidecars would contend too, resolve to the same suffixes by
/// coincidence, and the test would pass against the defect. Verified by
/// mutation — with the parent's *planned* destination passed to the sidecar
/// pass, that arrangement stays green and this one fails.
#[test]
fn a_sidecar_follows_its_parent_through_collision_resolution() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_0001.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_0001.xmp", b"<x:xmpmeta/>");

    let dir = TempDir::new().unwrap();
    let output = dir.path().join("out");
    let taken = output.join("2024-03-15").join("2024-03-15-143000.jpg");
    std::fs::create_dir_all(taken.parent().unwrap()).unwrap();
    std::fs::write(&taken, b"somebody else's photograph").unwrap();

    let out = mmm(tree.path())
        .arg("-o")
        .arg(&output)
        .arg("--commit")
        .arg("--no-prompt")
        .output()
        .expect("organising into an output tree that already holds that name");
    assert_ok(&out, "a run whose destination was occupied");

    let landed = file_contents_by_marker(&output);
    let parent = &landed["IMG_0001.jpg"][0];
    let sidecar = &landed["IMG_0001.xmp"][0];

    assert_eq!(
        stem_of(parent),
        "2024-03-15-143000-1",
        "the occupant should have pushed the photograph onto a suffix; got {parent}"
    );
    assert_eq!(
        stem_of(sidecar),
        stem_of(parent),
        "the sidecar must carry whatever suffix its parent got; parent {parent}, \
         sidecar {sidecar}"
    );
}

/// The same file is `IMG_1234.CR2` on one volume and `img_1234.cr2` on another.
#[test]
fn pairing_survives_a_case_difference_between_the_two_names() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.JPG", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("img_1234.XMP", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);
    assert_eq!(
        stem_of(run.at("img_1234.XMP")),
        stem_of(run.at("IMG_1234.JPG"))
    );
}

// ---------------------------------------------------------------------------
// The full-filename convention — IMG_1234.CR2 + IMG_1234.CR2.xmp
// ---------------------------------------------------------------------------

/// darktable's convention, and the one that has to be *preserved* rather than
/// normalised: the tool that wrote the sidecar is the tool that will next go
/// looking for it, and it will look for `<parent filename>.xmp`.
///
/// The parent here is a `.dng` rather than a `.jpg` because that is the shape
/// this convention appears in — a RAW file that must never be written into, and
/// therefore the case where losing the sidecar loses the only copy of the edits.
#[test]
fn a_full_filename_sidecar_lands_under_the_parents_whole_new_filename() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.dng",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .sidecar("IMG_1234.dng.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);
    let parent = run.at("IMG_1234.dng");
    let sidecar = run.at("IMG_1234.dng.xmp");

    let parent_name = Path::new(parent).file_name().unwrap().to_string_lossy();
    assert_eq!(
        Path::new(sidecar).file_name().unwrap().to_string_lossy(),
        format!("{parent_name}.xmp"),
        "the darktable convention must survive the move; parent {parent}, sidecar {sidecar}"
    );
    assert_eq!(dir_of(sidecar), dir_of(parent));
}

// ---------------------------------------------------------------------------
// A sidecar is not a media file
// ---------------------------------------------------------------------------

/// The scan totals count photographs. A sidecar that reached them would report
/// a library half again as large as it is, be offered for deduplication against
/// real media, and be dated from its own filesystem timestamp.
#[test]
fn a_sidecar_is_never_counted_among_the_files_scanned() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .jpeg_with_exif("b.jpg", naive(2024, 3, 16, 9, 0, 0), None)
        .sidecar("a.xmp", b"<x:xmpmeta/>")
        .sidecar("b.xmp", b"<x:xmpmeta/>")
        .sidecar("b.thm", b"thumb");

    let stdout = preview(tree.path(), &[]);

    assert!(
        stdout.contains("Files scanned:      2"),
        "two photographs and three sidecars is two files scanned; got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("{SIDECARS_FOUND_LABEL:<20}3")),
        "the three sidecars must be reported as sidecars; got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("{SIDECAR_ORPHANS_LABEL:<20}0")),
        "none of them is an orphan; got:\n{stdout}"
    );
}

/// A preview must say what it would do with a sidecar. It moves nothing, so the
/// listing is the only place the derived name can be seen before it is real.
#[test]
fn a_preview_lists_each_sidecar_under_its_parent_and_moves_nothing() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_1234.xmp", b"<x:xmpmeta/>");
    let before = snapshot_tree(tree.path());

    let stdout = preview(tree.path(), &[]);

    assert!(
        stdout.contains(SIDECAR_TAG),
        "the listing must mark the sidecar; got:\n{stdout}"
    );
    assert!(
        stdout.contains("2024-03-15-143000.xmp"),
        "the listing must show the name the sidecar would take; got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("{SIDECARS_MOVED_LABEL:<20}0")),
        "a preview moves nothing, sidecars included; got:\n{stdout}"
    );
    assert_eq!(
        snapshot_tree(tree.path()),
        before,
        "a preview must leave the tree exactly as it found it"
    );
}

// ---------------------------------------------------------------------------
// Orphans
// ---------------------------------------------------------------------------

/// A sidecar with nothing beside it is left where it is — *not* swept into
/// `unsorted/`, which is where files with no usable date go and would tell the
/// operator the wrong thing entirely.
#[test]
fn an_orphaned_sidecar_stays_where_it_is_and_is_reported() {
    let tree = MediaTree::new()
        .jpeg_with_exif("kept.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("gone.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);

    assert_eq!(
        snapshot_tree(tree.path()),
        ["gone.xmp"],
        "the orphan must be left exactly where it was"
    );
    assert!(
        !run.landed.contains_key("gone.xmp"),
        "the orphan must not have been moved into the output tree: {:#?}",
        run.landed
    );
    assert!(
        !run.output.join("unsorted").exists(),
        "an orphaned sidecar is not an undated photograph and must not be filed as one"
    );
    assert!(
        run.stdout.contains(ORPHAN_SIDECAR_HEADING) && run.stdout.contains("gone.xmp"),
        "the run must name the sidecar it left behind; got:\n{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(&format!("{SIDECAR_ORPHANS_LABEL:<20}1")),
        "and count it; got:\n{}",
        run.stdout
    );
}

/// The RAW+JPEG case. `IMG_1234.xmp` beside both `IMG_1234.jpg` and
/// `IMG_1234.dng` says nothing about which it belongs to, and picking one would
/// attach somebody's edits to the wrong photograph — a silent, plausible-looking
/// wrong answer, which is worse than an obvious refusal.
#[test]
fn a_sidecar_with_two_candidate_parents_is_left_alone_and_reported() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .tiff_raw(
            "IMG_1234.dng",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .sidecar("IMG_1234.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);

    assert_eq!(snapshot_tree(tree.path()), ["IMG_1234.xmp"]);
    assert!(
        run.stdout.contains(ORPHAN_SIDECAR_HEADING) && run.stdout.contains("more than one"),
        "the reason has to be the ambiguity, not a missing parent; got:\n{}",
        run.stdout
    );
    // Both photographs still moved. An ambiguous companion is not a reason to
    // leave the pictures where they were.
    assert_ne!(run.at("IMG_1234.jpg"), "IMG_1234.jpg");
    assert_ne!(run.at("IMG_1234.dng"), "IMG_1234.dng");
}

// ---------------------------------------------------------------------------
// Duplicates
// ---------------------------------------------------------------------------

/// A duplicate's sidecar follows it into `duplicates/NNN/`. The argument is the
/// same as for an organised file and it bites harder: a photograph in a numbered
/// duplicates directory is the copy nobody is looking at, so an `.xmp` stranded
/// in the source tree is the one that gets swept up in the next tidy-up.
#[test]
fn a_duplicates_sidecar_follows_it_into_the_duplicates_directory() {
    let tree = MediaTree::new()
        .jpeg_with_exif("one/photo.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .duplicate_of("two/photo.jpg", "one/photo.jpg")
        .sidecar("two/photo.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);
    let sidecar = run.at("two/photo.xmp");

    assert!(
        sidecar.starts_with("duplicates/"),
        "the sidecar must follow its parent into duplicates/; got {sidecar}"
    );
    // The duplicate keeps its own name there, so its sidecar does too.
    assert_eq!(sidecar, "duplicates/000/photo.xmp");

    let manifest = std::fs::read_to_string(run.output.join("duplicates/000/manifest.txt")).unwrap();
    assert!(
        manifest.contains("# sidecar:") && manifest.contains("photo.xmp"),
        "the manifest is the record a person reads, and a directory holding more files than \
         it accounts for is one nobody can act on; got:\n{manifest}"
    );
}

// ---------------------------------------------------------------------------
// Journalling and undo
// ---------------------------------------------------------------------------

/// A sidecar move is journalled as an entry in its own right, tagged as one.
/// This is what makes it reversible with no special case in `undo` — and what
/// lets a person reading the journal see why it holds twice as many lines as the
/// run moved photographs.
#[test]
fn a_sidecar_move_is_journalled_as_its_own_entry() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_1234.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &[]);

    let journals = journals_in(&run.output);
    assert_eq!(journals.len(), 1, "one run, one journal");
    let text = std::fs::read_to_string(&journals[0]).unwrap();

    let intents: Vec<JournalEntry> = text
        .lines()
        .skip(1) // the header
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|entry| matches!(entry, JournalEntry::MoveIntent { .. }))
        .collect();

    assert_eq!(
        intents.len(),
        2,
        "one photograph, one sidecar: {intents:#?}"
    );
    let kinds: Vec<IntentKind> = intents
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::MoveIntent { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [IntentKind::Organise, IntentKind::Sidecar],
        "the photograph first, then its companion — a sidecar recorded before its parent \
         would describe a move that had not happened yet"
    );
}

/// The whole point of journalling them separately: `undo` puts both files back
/// under their original names, with no knowledge of sidecars at all.
#[test]
fn undo_restores_a_photograph_and_its_sidecar_to_their_original_names() {
    let tree = MediaTree::new()
        .jpeg_with_exif("holiday/IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("holiday/IMG_1234.xmp", b"<x:xmpmeta/>");
    let before = snapshot_tree(tree.path());

    let run = organise(tree.path(), &[]);
    assert!(
        snapshot_tree(tree.path()).is_empty(),
        "the run should have emptied the source"
    );

    let undone = mmm(&run.output)
        .arg("undo")
        .arg("--commit")
        .arg("--journal-dir")
        .arg(run.output.join(".mmm").join("journal"))
        .output()
        .expect("undoing");
    assert_ok(&undone, "an undo");

    assert_eq!(
        snapshot_tree(tree.path()),
        before,
        "both files must be back under their original names, in their original directory"
    );
}

// ---------------------------------------------------------------------------
// --no-sidecars
// ---------------------------------------------------------------------------

/// The escape hatch restores exactly the behaviour of the version before
/// sidecars existed: they are not collected, not moved, not reported. Not
/// "reported as skipped" — a run told not to look at them has nothing to say
/// about them.
#[test]
fn no_sidecars_leaves_them_untouched_and_says_nothing_about_them() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_1234.xmp", b"<x:xmpmeta/>");

    let run = organise(tree.path(), &["--no-sidecars"]);

    assert_eq!(
        snapshot_tree(tree.path()),
        ["IMG_1234.xmp"],
        "the sidecar must be left exactly where it was"
    );
    assert!(
        !run.stdout.contains(SIDECARS_FOUND_LABEL),
        "a run told not to look has nothing to report; got:\n{}",
        run.stdout
    );
    // The photograph still moved, so this is the flag doing one thing and not
    // switching off the run.
    assert_ne!(run.at("IMG_1234.jpg"), "IMG_1234.jpg");
}

/// `--no-sidecars=false` answers a config file that switched them off. Without
/// the tri-state the setting would be one a file could set and the command line
/// could not unset, which is not a precedence rule.
#[test]
fn no_sidecars_false_overrides_a_config_that_switched_them_off() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .sidecar("IMG_1234.xmp", b"<x:xmpmeta/>");

    // Written directly rather than through the fixture builder: every file the
    // builder creates carries an embedded provenance marker, and a marker
    // appended to a TOML file is a syntax error in it.
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("mmm.toml");
    std::fs::write(&config, b"sidecars = false\n").unwrap();

    let output = dir.path().join("out");
    let out = Command::cargo_bin("mmm")
        .unwrap()
        .arg(tree.path())
        .arg("--config")
        .arg(&config)
        .arg("-o")
        .arg(&output)
        .arg("--commit")
        .arg("--no-prompt")
        .arg("--no-sidecars=false")
        .output()
        .expect("organising with the flag answering the file");
    assert_ok(&out, "a run whose flag overrides its config");

    let landed = file_contents_by_marker(&output);
    assert!(
        landed.contains_key("IMG_1234.xmp"),
        "the flag must outrank the file: {landed:#?}"
    );
}

// ---------------------------------------------------------------------------
// A sidecar as a witness, not just a passenger
// ---------------------------------------------------------------------------
//
// Everything above is about a sidecar arriving where its parent did. These are
// about a sidecar deciding where its parent goes.
//
// The fixture throughout is a TIFF-based RAW, and that is the point rather than
// a convenience. `nom-exif` recognises four containers and no RAW is one of
// them (`docs/reference/format-support.md`), so a `.cr2` has *never* had a
// readable date in this tool — the whole family files under filesystem
// timestamps. It is also the family that always has an `.xmp` beside it, because
// a RAW file must never be written into. So this is not an edge case bolted on
// to the date logic; it is the only route by which the largest family of files
// the scanner claims to handle can be filed under the date it was taken.
//
// `code/src/xmp.rs` unit-tests the parsing itself — both serialisations, the
// namespace rules, every date spelling, the malformed cases. None of that
// establishes that the index reaches the planner, that the date it yields
// survives into a destination path, or that `--require-exif` treats it as
// recorded. Only a run through `main` crosses those.

/// The headline case: a RAW whose container this tool cannot read, filed under
/// the date sitting in the text file beside it.
#[test]
fn a_raw_takes_its_date_from_the_xmp_beside_it() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.cr2",
            Some(b"CR\x02\x00"),
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .xmp(
            "IMG_1234.xmp",
            XmpForm::Attribute,
            &[("xmp:CreateDate", "2019-07-04T23:30:00+08:00")],
        );

    let stdout = preview(tree.path(), &[]);
    assert!(
        stdout.contains("[SIDECAR]"),
        "the listing must say the date came from the sidecar; got:\n{stdout}"
    );
    assert!(
        stdout.contains("[tz:sidecar]"),
        "and must not report an offset read from a text file as the file's own EXIF tag; \
         got:\n{stdout}"
    );

    let run = organise(tree.path(), &[]);
    let parent = run.at("IMG_1234.cr2");

    assert_eq!(
        dir_of(parent),
        "2019-07-04",
        "the RAW must be filed under the sidecar's date, not the filesystem's; got {parent}"
    );
    assert_eq!(
        parent, "2019-07-04/2019-07-04-233000.cr2",
        "and named after the sidecar's wall clock — 23:30 stays 23:30, as it does for EXIF"
    );
    // The sidecar is still a passenger as well as a witness.
    assert_eq!(stem_of(run.at("IMG_1234.xmp")), stem_of(parent));
    assert!(
        run.stdout.contains("Date from XMP sidecar: 1"),
        "the summary must count it apart from EXIF; got:\n{}",
        run.stdout
    );
}

/// The other serialisation, end to end. darktable writes this one, and it is
/// darktable's users who have the RAW libraries this feature is for.
#[test]
fn the_element_serialisation_dates_a_file_too() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .xmp(
            "IMG_1234.cr2.xmp",
            XmpForm::Element,
            &[("exif:DateTimeOriginal", "2019-07-04T23:30:00+08:00")],
        );

    let run = organise(tree.path(), &[]);

    assert_eq!(
        run.at("IMG_1234.cr2"),
        "2019-07-04/2019-07-04-233000.cr2",
        "an element-form date must file the same as an attribute-form one"
    );
    assert_eq!(
        run.at("IMG_1234.cr2.xmp"),
        "2019-07-04/2019-07-04-233000.cr2.xmp",
        "and the darktable naming convention still travels intact"
    );
}

/// A malformed sidecar is a warning and a skip. The run finishes, the
/// photograph is filed under the date it does have, and the sidecar still
/// travels — losing the file because we could not read it would be worse than
/// never having read it.
#[test]
fn a_malformed_sidecar_does_not_stop_the_run() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .tiff_raw(
            "IMG_5678.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .sidecar(
            "IMG_5678.xmp",
            b"<x:xmpmeta><rdf:RDF><rdf:Description xmp:CreateDate=\"2019-",
        );

    let run = organise(tree.path(), &[]);

    assert_eq!(
        run.at("IMG_1234.jpg"),
        "2024-03-15/2024-03-15-143000.jpg",
        "the rest of the run must be untouched by one unreadable text file"
    );
    assert_eq!(
        stem_of(run.at("IMG_5678.xmp")),
        stem_of(run.at("IMG_5678.cr2")),
        "the sidecar we could not read still belongs to its parent and still travels"
    );
    assert!(
        run.stdout.contains("Date from XMP sidecar: 0"),
        "nothing was dated from a sidecar; got:\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("format not supported: 1"),
        "the RAW must still report why it took a filesystem date; got:\n{}",
        run.stdout
    );
}

/// The file is the primary witness. An editor that rewrites `xmp:CreateDate` on
/// export would otherwise silently re-file a photograph under the date somebody
/// edited it.
#[test]
fn a_date_the_photograph_recorded_is_not_overridden_by_its_sidecar() {
    let tree = MediaTree::new()
        .jpeg_with_exif("IMG_1234.jpg", naive(2024, 3, 15, 14, 30, 0), None)
        .xmp(
            "IMG_1234.xmp",
            XmpForm::Attribute,
            &[("xmp:CreateDate", "2019-07-04T23:30:00+00:00")],
        );

    let run = organise(tree.path(), &[]);

    assert_eq!(
        run.at("IMG_1234.jpg"),
        "2024-03-15/2024-03-15-143000.jpg",
        "the JPEG's own EXIF must win over the sidecar's date"
    );
    assert!(
        run.stdout.contains("Date from XMP sidecar: 0"),
        "and the run must not claim to have used the sidecar; got:\n{}",
        run.stdout
    );
}

/// `--require-exif` admits a sidecar date, and this is the decision worth
/// pinning: the flag refuses *filesystem timestamps*, and an `xmp:CreateDate` is
/// not one. Excluding it would send an entire RAW library to `unsorted/` while
/// the date sat in the file beside every frame — defeating the flag for exactly
/// the people most likely to reach for it.
#[test]
fn require_exif_admits_a_sidecar_date_and_still_refuses_a_filesystem_one() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .xmp(
            "IMG_1234.xmp",
            XmpForm::Attribute,
            &[("xmp:CreateDate", "2019-07-04T23:30:00+08:00")],
        )
        .tiff_raw(
            "IMG_5678.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        );

    let run = organise(tree.path(), &["--require-exif"]);

    assert_eq!(
        run.at("IMG_1234.cr2"),
        "2019-07-04/2019-07-04-233000.cr2",
        "a sidecar-dated file is a recorded date, and --require-exif must admit it"
    );
    assert_eq!(
        dir_of(run.at("IMG_5678.cr2")),
        "unsorted",
        "and the one with no sidecar still has only a filesystem date, which it refuses"
    );
}

/// `--no-sidecars` switches off the whole of it, dates included. The flag is
/// implemented by handing the scan an empty sidecar list, so there is no stage
/// at which a sidecar exists to be read — and this is what proves that the date
/// path did not quietly acquire its own route to the file.
#[test]
fn no_sidecars_switches_off_the_date_as_well_as_the_move() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .xmp(
            "IMG_1234.xmp",
            XmpForm::Attribute,
            &[("xmp:CreateDate", "2019-07-04T23:30:00+08:00")],
        );

    let run = organise(tree.path(), &["--no-sidecars"]);

    assert_ne!(
        dir_of(run.at("IMG_1234.cr2")),
        "2019-07-04",
        "a run told not to look at sidecars must not read a date out of one"
    );
    assert_eq!(
        snapshot_tree(tree.path()),
        ["IMG_1234.xmp"],
        "and the sidecar stays exactly where it was"
    );
}

/// A `.aae` and a `.thm` are sidecars for moving, not for reading. An Apple
/// `.aae` is a binary property list and a `.thm` is a thumbnail; putting an XML
/// parser on either would produce nothing but log noise, and a `.thm` that
/// happened to parse would be reporting its *own* metadata as its parent's.
#[test]
fn only_an_xmp_is_read_for_a_date() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        // The same packet an .xmp would be believed for, under the wrong
        // extension. If the extension were not the gate, this would file the
        // RAW under 2019.
        .xmp(
            "IMG_1234.aae",
            XmpForm::Attribute,
            &[("xmp:CreateDate", "2019-07-04T23:30:00+08:00")],
        );

    let run = organise(tree.path(), &[]);

    assert_ne!(
        dir_of(run.at("IMG_1234.cr2")),
        "2019-07-04",
        "only a .xmp is read for a date"
    );
    // It still travels, which is the half that is not being switched off.
    assert_eq!(
        stem_of(run.at("IMG_1234.aae")),
        stem_of(run.at("IMG_1234.cr2"))
    );
}

/// A sidecar date with no offset is a bare wall clock, and goes through the
/// run's resolution order exactly as a naive EXIF `DateTimeOriginal` does.
///
/// Both halves matter. The wall clock must survive untouched — 23:30 files
/// under the 4th, not the 5th, which is the defect this whole phase exists to
/// fix — and the run must say `config` rather than `sidecar`, because nothing in
/// the sidecar said which zone it was.
#[test]
fn a_sidecar_date_with_no_offset_resolves_through_the_run_policy() {
    let tree = MediaTree::new()
        .tiff_raw(
            "IMG_1234.cr2",
            None,
            naive(2024, 3, 15, 14, 30, 0),
            Some("+00:00"),
            None,
        )
        .xmp(
            "IMG_1234.xmp",
            XmpForm::Attribute,
            &[("xmp:CreateDate", "2019-07-04T23:30:00")],
        );

    let stdout = preview(tree.path(), &["--timezone", "+08:00"]);
    assert!(
        stdout.contains("[tz:config]"),
        "an offset nothing stated must be reported as the run's own choice; got:\n{stdout}"
    );

    let run = organise(tree.path(), &["--timezone", "+08:00"]);
    assert_eq!(
        run.at("IMG_1234.cr2"),
        "2019-07-04/2019-07-04-233000.cr2",
        "attaching an offset must not move the wall clock, here as everywhere else"
    );
}
