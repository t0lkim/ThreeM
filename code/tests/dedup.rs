//! Integration suite for deduplication, driven through the real `mmm` binary.
//!
//! Dedup is the most destructive thing `ThreeM` does: it decides that two of
//! someone's files are the same file, keeps one, and moves the other somewhere
//! else. A false positive here silently buries a photo that was never a
//! duplicate. So these tests exercise the whole cascade end to end — scan,
//! three-phase hash, `duplicates/NNN/` move, manifest — against synthetic
//! trees built by the fixture harness.
//!
//! ## Three things measured, not assumed
//!
//! Each of these was established with a throwaway probe before anything was
//! asserted, and each one would have produced a plausible-looking but wrong
//! test if it had been guessed instead:
//!
//! 1. **Which copy is kept is a documented rule, and it is not scan order.**
//!    `WalkDir` returns whatever the filesystem hands it — on APFS a tree
//!    declared `a.jpg` then `b.jpg` scans back as `b.jpg`, `a.jpg`, and ext4
//!    disagrees with APFS. These tests used to derive their expectations from
//!    [`mmm::scanner::scan_directories`] for exactly that reason. They no
//!    longer need to: the retained original is now **the shallowest path, then
//!    the lexicographically smallest**, decided after hashing, so `a.jpg` is
//!    kept over `b.jpg` and a top-level file is kept over a nested one on every
//!    platform. The expectations below are therefore written out by name — and
//!    a test that names a file is a test that would catch the rule silently
//!    reverting to a coin toss.
//! 2. **`duplicates/NNN` numbering follows content-hash order.** The groups
//!    used to be accumulated by iterating a `HashMap`, whose order is randomly
//!    seeded per process: two groups in one tree came back `000`/`001` in one
//!    run and swapped in the next, over six consecutive runs. They are now
//!    sorted by their BLAKE3 digest, so the numbering is stable between runs —
//!    but which group lands on `000` still depends on how the fixtures' bytes
//!    happen to hash, which is nothing a test should predict. `000` is asserted
//!    only where the tree contains exactly one group; the multi-group test
//!    stays order-agnostic.
//! 3. **The manifest records *input* paths, so it goes stale the moment the
//!    run finishes.** See
//!    [`the_manifest_records_input_paths_which_go_stale_when_the_run_moves_them`].
//!
//! ## Proving *which* file landed where
//!
//! Byte-identical fixtures necessarily carry the same embedded marker, so
//! [`file_contents_by_marker`] maps one marker to several paths here. Where a
//! test needs to tell two duplicates apart it uses their filenames, which the
//! organiser preserves when it moves a duplicate aside.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

use common::{file_contents_by_marker, naive, snapshot_tree, snapshot_tree_hashed, MediaTree};
use mmm::scanner::ScanFilter;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Run `mmm` against `input`, previewing only — no `--commit`.
///
/// Passes no `-o`, so the output directory defaults to the input directory.
/// That is the shape in which a stray `duplicates/` would appear inside the
/// user's own library, which is what the preview test is looking for.
fn run_preview(input: &Path) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .output()
        .expect("running mmm in preview mode")
}

/// Everything a run printed except the line naming its thread count.
///
/// That line is the one place two runs at different thread counts may legitimately
/// differ, so it is removed here and asserted separately by the caller — dropping
/// it without also checking it would leave a comparison that passed just as
/// happily if `--threads` were being ignored altogether.
fn plan_without_thread_count(stdout: &str) -> String {
    stdout
        .lines()
        .filter(|line| !line.starts_with("Analysing for duplicates"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `mmm` against `input` in preview mode, with extra arguments appended.
fn run_preview_with(input: &Path, extra: &[&str]) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .args(extra)
        .output()
        .expect("running mmm in preview mode")
}

/// Run `mmm --commit` against `input`, organising into `output`.
fn run_commit(input: &Path, output: &Path) -> std::process::Output {
    Command::cargo_bin("mmm")
        .unwrap()
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--commit")
        .arg("--no-prompt")
        .output()
        .expect("running mmm in commit mode")
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

/// A scratch directory whose `out` child does not yet exist, so a test can
/// assert that the binary did not create it.
fn scratch_output() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("creating output TempDir");
    let out = dir.path().join("out");
    assert!(!out.exists(), "the scratch output path must start absent");
    (dir, out)
}

/// The media files in `input`, as the binary's own scanner admits them.
///
/// Used to assert the *set* a fixture produced — which is still worth checking,
/// since a file the scan quietly passed over would otherwise look like a file
/// the dedup pass correctly ignored. It is deliberately unsorted: nothing
/// downstream is defined against this order any more.
fn scanned_files(input: &Path) -> Vec<PathBuf> {
    mmm::scanner::scan_directories(&[input.to_path_buf()], &ScanFilter::default())
        .files
        .into_iter()
        .map(|f| f.path)
        .collect()
}

/// The parsed contents of a `duplicates/NNN/manifest.txt`.
struct Manifest {
    hash: String,
    size: u64,
    /// Path recorded for the file that was *not* moved aside, as the dedup pass
    /// saw it — before the organise pass moved it.
    original: String,
    /// Where the organise pass reported the original finally landed, if it got
    /// that far. This is the path `mmm-dedup-verifier` actually resolves.
    original_moved_to: Option<String>,
    /// Paths recorded for the files that were moved into this group.
    duplicates: Vec<String>,
}

/// Parse a group manifest the same way `mmm-dedup-verifier` does.
///
/// Re-implemented here rather than reused, so that a change to the manifest
/// format has to be made deliberately in two places instead of silently
/// agreeing with itself.
fn read_manifest(path: &Path) -> Manifest {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading manifest {}: {e}", path.display()));

    let mut hash = None;
    let mut size = None;
    let mut original = None;
    let mut original_moved_to = None;
    let mut duplicates = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("# BLAKE3 hash: ") {
            hash = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("# File size: ") {
            size = v.strip_suffix(" bytes").and_then(|n| n.parse::<u64>().ok());
        } else if let Some(v) = line.strip_prefix("# Original moved to: ") {
            original_moved_to = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("# Original kept at: ") {
            original = Some(v.to_string());
        } else if !line.starts_with('#') {
            duplicates.push(line.to_string());
        }
    }

    Manifest {
        hash: hash.unwrap_or_else(|| panic!("no hash line in {}", path.display())),
        size: size.unwrap_or_else(|| panic!("no size line in {}", path.display())),
        original: original.unwrap_or_else(|| panic!("no original line in {}", path.display())),
        original_moved_to,
        duplicates,
    }
}

/// Every `duplicates/NNN/` directory under `output`, sorted by name.
///
/// Empty (rather than panicking) when no `duplicates/` directory exists, so a
/// test can assert its absence.
fn duplicate_group_dirs(output: &Path) -> Vec<PathBuf> {
    let base = output.join("duplicates");
    if !base.is_dir() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("reading {}: {e}", base.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Sorted filenames of the duplicates set aside in a group directory, with the
/// bookkeeping `manifest.txt` excluded.
fn files_in_group(group_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(group_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", group_dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "manifest.txt")
        .collect();
    names.sort();
    names
}

/// The multiset of content hashes under `root`, keyed by hash, excluding the
/// manifests the organiser writes.
///
/// Counting paths proves nothing was *dropped*; counting content hashes also
/// proves nothing was corrupted or quietly substituted along the way.
fn content_hash_counts(root: &Path) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for line in snapshot_tree_hashed(root) {
        let (path, hash) = line
            .rsplit_once("  ")
            .unwrap_or_else(|| panic!("malformed snapshot line: {line}"));
        if path.ends_with("manifest.txt") {
            continue;
        }
        *counts.entry(hash.to_string()).or_insert(0) += 1;
    }
    counts
}

fn leaf(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Two identical files
// ---------------------------------------------------------------------------

#[test]
fn two_identical_jpegs_make_one_group_keeping_the_lower_path_and_setting_the_other_aside() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg");

    // Same directory, so same depth: the tie-break decides, and `a.jpg` is
    // kept on every platform regardless of which one the filesystem hands
    // back first. See the module note.
    assert_eq!(scanned_files(tree.path()).len(), 2, "fixture setup");
    let (expected_kept, expected_moved) = (tree.path().join("a.jpg"), tree.path().join("b.jpg"));

    let (_scratch, out_dir) = scratch_output();
    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run");

    // Exactly one group, and it is 000 — reliable here only because there is
    // precisely one of them.
    let groups = duplicate_group_dirs(&out_dir);
    assert_eq!(
        groups.len(),
        1,
        "expected exactly one duplicate group, got {groups:?}"
    );
    assert_eq!(leaf(&groups[0]), "000");

    // The second file was set aside under its own name...
    assert_eq!(files_in_group(&groups[0]), vec![leaf(&expected_moved)]);

    // ...and the first was organised into the date tree instead, at the path
    // its EXIF datetime dictates.
    assert_eq!(
        snapshot_tree(&out_dir),
        vec![
            "2024-01-15/2024-01-15-143000.jpg".to_string(),
            format!("duplicates/000/{}", leaf(&expected_moved)),
            "duplicates/000/manifest.txt".to_string(),
        ]
    );

    // The manifest agrees about which file was kept. Naming it is what pins
    // the retention rule; without this, a change that retained the other copy
    // would still satisfy everything above.
    let manifest = read_manifest(&groups[0].join("manifest.txt"));
    assert_eq!(
        manifest.original,
        expected_kept.display().to_string(),
        "the retained original was not the lexicographically smallest path"
    );

    // Both files left the input tree; neither was copied, both were moved.
    assert!(snapshot_tree(tree.path()).is_empty());
}

#[test]
fn the_manifest_names_both_the_retained_original_and_the_moved_duplicate() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg");

    let (kept, moved) = (tree.path().join("a.jpg"), tree.path().join("b.jpg"));

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let manifest_path = out_dir.join("duplicates/000/manifest.txt");
    assert!(
        manifest_path.is_file(),
        "no manifest at {}",
        manifest_path.display()
    );

    let manifest = read_manifest(&manifest_path);
    assert_eq!(manifest.original, kept.display().to_string());
    assert_eq!(manifest.duplicates, vec![moved.display().to_string()]);

    // The manifest is the audit trail a user consults before deleting
    // anything, so its hash and size have to describe the actual bytes rather
    // than being decorative.
    let bytes = fs::read(out_dir.join("duplicates/000").join(leaf(&moved))).unwrap();
    assert_eq!(manifest.size, bytes.len() as u64);
    assert_eq!(manifest.hash, blake3::hash(&bytes).to_hex().to_string());
}

#[test]
fn the_manifest_records_where_the_original_actually_ended_up() {
    // The dedup pass runs *before* the organise pass, so the header line
    // `# Original kept at:` names the original's location in the *input* tree —
    // a path the organise pass then empties. That was the whole of the record
    // until 0.2.2, and it made `mmm-dedup-verifier` vacuous: it resolved that
    // path, found nothing, recorded `OriginalMissing`, confirmed zero groups
    // and still exited 0 printing "All verified groups are confirmed
    // duplicates" — the independent second opinion somebody runs *before*
    // deleting a `duplicates/` directory.
    //
    // The organise pass now appends `# Original moved to:` once it knows. The
    // header is left alone deliberately: appending keeps the crash-safety the
    // manifest was designed around, where nothing already flushed is ever
    // rewritten.
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let manifest = read_manifest(&out_dir.join("duplicates/000/manifest.txt"));

    // The header still records where the file was when the group was formed.
    // That is history, not a lie, and it is why the second line exists.
    assert!(
        !Path::new(&manifest.original).exists(),
        "the input path in the header should no longer resolve — the organise \
         pass moved the file"
    );

    let moved_to = manifest.original_moved_to.as_ref().unwrap_or_else(|| {
        panic!(
            "the manifest records no `# Original moved to:` line, so the \
             verifier has nothing resolvable to hash and will report this \
             group as missing"
        )
    });
    assert!(
        Path::new(moved_to).exists(),
        "the recorded destination {moved_to} does not resolve"
    );
    assert!(
        Path::new(moved_to).starts_with(&out_dir),
        "the original should have landed inside the output tree, not at {moved_to}"
    );

    // The duplicates' own recorded source paths are still input paths, and
    // still dangle. They are the record of where each file *came from*, which
    // is what an interrupted run needs; `# moved:` outcome lines say where they
    // went.
    for dup in &manifest.duplicates {
        assert!(
            !Path::new(dup).exists(),
            "recorded duplicate source {dup} still resolves"
        );
    }

    assert_eq!(
        content_hash_counts(&out_dir).values().sum::<usize>(),
        2,
        "both copies must still exist somewhere in the output tree"
    );
}

/// The point of the manifest fix, stated as the tool's own verdict: the
/// verifier confirms the group and exits 0 against a tree `mmm` produced.
///
/// Driven through the real binary because the defect was never in the hashing —
/// it was in whether the path the verifier resolves is the path the file is at,
/// which only a whole run establishes.
#[test]
fn the_verifier_confirms_a_tree_mmm_produced() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let out = Command::cargo_bin("mmm-dedup-verifier")
        .unwrap()
        .arg(out_dir.join("duplicates"))
        .output()
        .expect("running mmm-dedup-verifier");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(
        out.status.success(),
        "the verifier failed against a tree mmm itself produced:\n{stdout}"
    );
    assert!(
        stdout.contains("Confirmed duplicates: 1"),
        "the group was not confirmed:\n{stdout}"
    );
    assert!(
        stdout.contains("Original missing: 0"),
        "the original was not found where the manifest said:\n{stdout}"
    );
}

/// A verifier that confirmed nothing must not exit 0.
///
/// This is the other half of the defect, and it survives independently of the
/// manifest: an all-clear printed over zero confirmed groups is a false
/// all-clear whatever caused the groups to go unconfirmed. Reproduced by
/// deleting the original after the run, which is the state the stale manifest
/// used to produce on every run.
#[test]
fn the_verifier_refuses_to_report_an_all_clear_having_confirmed_nothing() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let manifest = read_manifest(&out_dir.join("duplicates/000/manifest.txt"));
    let moved_to = manifest
        .original_moved_to
        .expect("the manifest must name where the original landed");
    fs::remove_file(&moved_to).expect("removing the original");

    let out = Command::cargo_bin("mmm-dedup-verifier")
        .unwrap()
        .arg(out_dir.join("duplicates"))
        .output()
        .expect("running mmm-dedup-verifier");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(
        !out.status.success(),
        "the verifier exited 0 having confirmed nothing:\n{stdout}"
    );
    assert!(
        !stdout.contains("All 0 verified"),
        "an all-clear was printed over zero confirmed groups:\n{stdout}"
    );
    assert!(
        stdout.contains("NOT verified"),
        "the verifier did not say the group went unverified:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// The cascade must not produce false positives
// ---------------------------------------------------------------------------

#[test]
fn two_files_of_identical_size_but_different_content_are_not_grouped() {
    // Phase 1 groups by size alone, so these two collide there and only the
    // partial/full hash phases can separate them. If the cascade ever short
    // circuits after phase 1, this is the test that catches it — and the cost
    // of that bug is a user's photo buried in `duplicates/` for no reason.
    //
    // Equal size is arranged by giving both fixtures same-length declared
    // paths (the embedded marker carries that path) and an EXIF timestamp,
    // which is a fixed-width field. Different dates keep their destinations
    // apart so a filename collision does not muddy the result.
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("b.jpg", naive(2024, 5, 6, 7, 8, 9), None);

    let scanned =
        mmm::scanner::scan_directories(&[tree.path().to_path_buf()], &ScanFilter::default()).files;
    assert_eq!(scanned.len(), 2);
    assert_eq!(
        scanned[0].size, scanned[1].size,
        "fixture setup: the two files must be the same size for this test to \
         exercise the hash phases at all"
    );

    let (_scratch, out_dir) = scratch_output();
    let out = run_commit(tree.path(), &out_dir);
    assert_ok(&out, "commit run");

    assert!(
        !out_dir.join("duplicates").exists(),
        "same-size, different-content files were treated as duplicates"
    );
    assert_eq!(
        snapshot_tree(&out_dir),
        vec![
            "2024-01-15/2024-01-15-143000.jpg".to_string(),
            "2024-05-06/2024-05-06-070809.jpg".to_string(),
        ]
    );
    assert!(
        stdout_of(&out).contains("No duplicates found."),
        "the run should have reported no duplicates:\n{}",
        stdout_of(&out)
    );
}

// ---------------------------------------------------------------------------
// More than two copies
// ---------------------------------------------------------------------------

#[test]
fn three_identical_files_make_one_group_with_two_files_moved_into_it() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg")
        .duplicate_of("nested/c.jpg", "a.jpg");

    // `a.jpg` and `b.jpg` sit one component deep, `nested/c.jpg` two — so the
    // depth rule keeps a top-level copy, and the tie-break between the two
    // top-level ones keeps `a.jpg`. Previously this was whichever the
    // filesystem walked first, which is why ext4 and APFS disagreed here.
    assert_eq!(scanned_files(tree.path()).len(), 3, "fixture setup");
    let kept = tree.path().join("a.jpg");
    let moved: Vec<String> = vec!["b.jpg".to_string(), "c.jpg".to_string()];

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let groups = duplicate_group_dirs(&out_dir);
    assert_eq!(groups.len(), 1, "expected one group, got {groups:?}");
    assert_eq!(
        files_in_group(&groups[0]),
        moved,
        "both surplus copies should sit in the one group"
    );

    let manifest = read_manifest(&groups[0].join("manifest.txt"));
    assert_eq!(manifest.original, kept.display().to_string());
    assert_eq!(manifest.duplicates.len(), 2);

    // One copy — and only one — reaches the date tree.
    let landed = file_contents_by_marker(&out_dir);
    let places = landed.get("a.jpg").expect("no fixture file landed at all");
    let in_date_tree: Vec<&String> = places
        .iter()
        .filter(|p| !p.starts_with("duplicates/"))
        .collect();
    assert_eq!(
        in_date_tree,
        vec![&"2024-01-15/2024-01-15-143000.jpg".to_string()],
        "exactly one copy belongs in the date tree; whole tree was {:?}",
        snapshot_tree(&out_dir)
    );

    // Every surplus copy is a flat *file* directly inside the group directory,
    // under its bare leaf name — including `nested/c.jpg`, which came from a
    // subdirectory.
    //
    // This used to be derived from scan order and could not name `c.jpg`:
    // asserting `duplicates/000/c.jpg` passed on macOS and failed on Linux,
    // where `nested/c.jpg` was scanned first and so became the *retained*
    // copy, never entering the duplicates set at all. The depth rule settles
    // it — a nested copy is never kept over a top-level one — so the nested
    // file's flattening is now pinned here as well as in
    // `duplicates_sharing_a_leaf_name_do_not_overwrite_each_other` below.
    for name in &moved {
        assert!(
            groups[0].join(name).is_file(),
            "surplus copy {name} is not a flat file in the group directory; \
             group holds {:?}",
            files_in_group(&groups[0])
        );
    }
}

#[test]
fn duplicates_sharing_a_leaf_name_do_not_overwrite_each_other() {
    // Three identical files whose filenames are also identical. They all
    // flatten into one group directory, so without collision handling the
    // second write would destroy the first — silent data loss in the very
    // directory a user is told to check before deleting anything.
    let tree = MediaTree::new()
        .jpeg_with_exif("one/photo.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("two/photo.jpg", "one/photo.jpg")
        .duplicate_of("three/photo.jpg", "one/photo.jpg");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    assert_eq!(
        files_in_group(&out_dir.join("duplicates/000")),
        vec!["photo-1.jpg".to_string(), "photo.jpg".to_string()],
        "a same-named duplicate overwrote another instead of being suffixed"
    );

    // Three copies went in; three copies are still on disk.
    assert_eq!(
        content_hash_counts(&out_dir)
            .values()
            .copied()
            .sum::<usize>(),
        3
    );
}

// ---------------------------------------------------------------------------
// No duplicates at all
// ---------------------------------------------------------------------------

#[test]
fn a_tree_with_no_duplicates_creates_no_duplicates_directory() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .jpeg_with_exif("sub/b.jpg", naive(2023, 6, 7, 8, 9, 10), None)
        .video("clip.mov", b"a video that is not a duplicate")
        .non_media("notes.txt", b"not media at all");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    assert!(
        !out_dir.join("duplicates").exists(),
        "a duplicate-free run created {}",
        out_dir.join("duplicates").display()
    );
    // Not merely absent as a directory — nothing anywhere in the output tree
    // is filed under a duplicates path.
    assert!(
        snapshot_tree(&out_dir)
            .iter()
            .all(|p| !p.starts_with("duplicates/")),
        "output tree: {:?}",
        snapshot_tree(&out_dir)
    );
}

// ---------------------------------------------------------------------------
// Conservation
// ---------------------------------------------------------------------------

#[test]
fn nothing_is_lost_the_output_holds_every_file_that_went_in() {
    // A tree mixing every case the suite covers: a pair, a triple, a lone
    // file, and a same-size-different-content pair that must stay separate.
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg")
        .jpeg_with_exif("x/p.jpg", naive(2023, 7, 4, 8, 9, 10), Some((51.5, -0.12)))
        .duplicate_of("y/p.jpg", "x/p.jpg")
        .duplicate_of("z/p.jpg", "x/p.jpg")
        .jpeg_with_exif("solo.jpg", naive(2022, 11, 2, 3, 4, 5), None)
        .video("clip.mov", b"a lone video");

    let before = content_hash_counts(tree.path());
    let input_count: usize = before.values().sum();
    assert_eq!(input_count, 7, "fixture setup: {before:?}");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    // Every file must be accounted for across *both* trees — counting only
    // the output would let a file stranded in the input read as a pass.
    let mut after = content_hash_counts(&out_dir);
    for (hash, n) in content_hash_counts(tree.path()) {
        *after.entry(hash).or_insert(0) += n;
    }

    assert_eq!(
        after.values().sum::<usize>(),
        input_count,
        "file count changed: {input_count} in, {} out",
        after.values().sum::<usize>()
    );
    // Stronger than a count: the same bytes, in the same multiplicities. A run
    // that lost one photo and duplicated another would keep the count intact.
    assert_eq!(
        after, before,
        "the set of file contents changed between input and output"
    );
    assert!(
        snapshot_tree(tree.path()).is_empty(),
        "media was left behind in the input tree: {:?}",
        snapshot_tree(tree.path())
    );
}

#[test]
fn every_duplicate_group_gets_its_own_directory_and_manifest() {
    // Deliberately order-agnostic: group numbering comes from `HashMap`
    // iteration and is randomly seeded per process, so `000` and `001` swapped
    // between consecutive runs during development. Asserting "group 000 is the
    // a/b pair" would pass locally about half the time and fail in CI the
    // other half. What is genuinely guaranteed is that each group is
    // self-consistent, so that is what this checks.
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg")
        .jpeg_with_exif("x/p.jpg", naive(2023, 7, 4, 8, 9, 10), Some((51.5, -0.12)))
        .duplicate_of("y/p.jpg", "x/p.jpg");

    let (_scratch, out_dir) = scratch_output();
    assert_ok(&run_commit(tree.path(), &out_dir), "commit run");

    let groups = duplicate_group_dirs(&out_dir);
    assert_eq!(groups.len(), 2, "expected two groups, got {groups:?}");

    let mut names: Vec<String> = groups.iter().map(|g| leaf(g)).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["000".to_string(), "001".to_string()],
        "group directories should be numbered from zero without gaps"
    );

    let mut hashes = Vec::new();
    for group in &groups {
        let manifest = read_manifest(&group.join("manifest.txt"));
        let set_aside = files_in_group(group);
        assert_eq!(
            set_aside.len(),
            1,
            "each pair should set exactly one file aside, group {} had {set_aside:?}",
            group.display()
        );

        // The manifest's hash must actually describe the file sitting next to
        // it, or the two groups' bookkeeping has been crossed over.
        let bytes = fs::read(group.join(&set_aside[0])).unwrap();
        assert_eq!(manifest.hash, blake3::hash(&bytes).to_hex().to_string());
        assert_eq!(manifest.size, bytes.len() as u64);
        hashes.push(manifest.hash);
    }

    hashes.sort();
    hashes.dedup();
    assert_eq!(hashes.len(), 2, "the two groups share a content hash");
}

// ---------------------------------------------------------------------------
// The default posture applies to dedup too
// ---------------------------------------------------------------------------

#[test]
fn a_preview_run_reports_duplicates_without_creating_or_moving_anything() {
    // Dedup is destructive in its own right — it relocates files the user
    // never asked about. `--commit` has to gate it just as it gates the date
    // tree, and the preview still has to say what it found.
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("b.jpg", "a.jpg")
        .non_media("notes.txt", b"leave me alone");

    let before = snapshot_tree_hashed(tree.path());
    let out = run_preview(tree.path());
    assert_ok(&out, "preview run");

    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "a run without --commit moved a duplicate"
    );
    assert!(
        !tree.join("duplicates").exists(),
        "a preview created duplicates/ inside the input tree"
    );

    // A preview that silently declines to mention the duplicates it found is
    // useless — the listing is the whole product of the run.
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("Duplicate Groups"),
        "the preview did not report the duplicate group it found:\n{stdout}"
    );
    assert!(
        stdout.contains("Duplicate files:    1"),
        "the preview did not count the duplicate it found:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// The concurrency bound changes the pace and nothing else
// ---------------------------------------------------------------------------

/// The plan a run prints must not depend on how many threads hashed it.
///
/// `--threads 1` is the setting somebody on a spinning disk or a network share
/// reaches for, and the trade they are making is speed for kindness to their
/// storage. They must not also be trading *which copy of their photograph gets
/// kept* — so the whole plan is compared, not merely the number of duplicate
/// groups: the group numbering, the retained original of each, the destination
/// name of every unique file and every count in the summary.
///
/// Compared in preview rather than after two committed runs, because two commits
/// need two input trees and the manifests would then differ by their temp-dir
/// prefix alone — a difference that would have to be normalised away, and
/// normalisation is where a real difference hides. One tree, twice, moving
/// nothing.
///
/// The one line that legitimately differs is the phase's own thread count, and
/// it is asserted rather than merely skipped: a test that filtered it out
/// without checking it would still pass if `--threads` were silently ignored,
/// which is the failure it most needs to catch.
#[test]
fn one_thread_plans_exactly_what_the_parallel_default_plans() {
    let tree = MediaTree::new()
        .jpeg_with_exif("a.jpg", naive(2024, 1, 15, 14, 30, 0), None)
        .duplicate_of("nested/b.jpg", "a.jpg")
        .duplicate_of("nested/deeper/c.jpg", "a.jpg")
        .jpeg_with_exif("x/p.jpg", naive(2023, 7, 4, 8, 9, 10), Some((51.5, -0.12)))
        .duplicate_of("y/p.jpg", "x/p.jpg")
        .jpeg_with_exif("solo.jpg", naive(2022, 3, 1, 6, 0, 0), None)
        .non_media("notes.txt", b"leave me alone");

    let before = snapshot_tree_hashed(tree.path());

    let serial = run_preview_with(tree.path(), &["--threads", "1"]);
    assert_ok(&serial, "preview with --threads 1");
    let parallel = run_preview_with(tree.path(), &[]);
    assert_ok(&parallel, "preview with the default thread count");

    let serial = stdout_of(&serial);
    let parallel = stdout_of(&parallel);

    assert!(
        serial.contains("Analysing for duplicates (1 thread)..."),
        "--threads 1 did not reach the hashing pool:\n{serial}"
    );

    assert_eq!(
        plan_without_thread_count(&serial),
        plan_without_thread_count(&parallel),
        "the thread count bounds the concurrency and must not reach the plan"
    );

    assert_eq!(
        snapshot_tree_hashed(tree.path()),
        before,
        "neither preview may move anything"
    );
}
