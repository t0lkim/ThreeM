//! Regression suite for the destructive-path defects in `organiser`.
//!
//! Every test here was written *before* the fix it describes, and each one
//! encodes a way `ThreeM` can currently lose someone's photos. They are
//! deliberately library-level rather than binary-level: the defects live in
//! [`mmm::organiser::execute_move`] and the private `cross_volume_move` below
//! it, and driving them through the CLI would add a scan, a plan and a
//! progress bar between the assertion and the thing being asserted.
//!
//! ## The four defects
//!
//! 1. **Overwrite on rename.** `resolve_collision` asks `Path::exists()` and
//!    then hands the answer to `fs::rename`, which replaces whatever is at the
//!    destination without complaint. `exists()` is not a lock and it is not
//!    even a complete question — it follows symlinks, so a dangling symlink
//!    reads as "nothing here" while the filesystem entry is very much there.
//! 2. **Cross-volume misclassification.** `execute_move` matches `Err(_)` on
//!    the rename and treats *every* failure as "must be a different volume,
//!    fall through to copy-and-delete". A missing source, a permission denial
//!    and a read-only filesystem all take the copy path they have no business
//!    taking, and the error the caller finally sees describes the copy that
//!    failed rather than the thing that was actually wrong.
//! 3. **Size-only copy verification.** `cross_volume_move` proves the copy
//!    worked by comparing `metadata().len()` of source and temp file, and then
//!    deletes the source. A copy that is the right length and the wrong bytes
//!    passes, and the original is destroyed.
//! 4. **No destination in the error.** A failure writing into the destination
//!    directory reports the source path only, so the operator is told which
//!    photo failed but not where it was going or why.
//!
//! ## What passes today, and why that is still worth asserting
//!
//! Not all of these fail against the current implementation, and the honest
//! record of which do is in
//! `.maestro/playbooks/Initiation/Phase-02-Destructive-Path-Hardening.md`.
//! The ones that pass today pass *by luck of ordering* — `resolve_collision`
//! happens to run immediately before the rename in a single-threaded test, so
//! the TOCTOU window never opens. They are here to stay green through the
//! rewrite in the next task, which is exactly what a regression test is for.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mmm::metadata::DateSource;
use mmm::organiser::{execute_move, PlannedMove};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A planned move from `source` to `destination`.
///
/// The metadata fields are inert here — nothing in the move path reads them —
/// so they are pinned to the "no date, no location" shape to keep the tests
/// about the filesystem behaviour and nothing else.
fn plan(source: &Path, destination: &Path) -> PlannedMove {
    PlannedMove {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        date_source: DateSource::None,
        has_location: false,
    }
}

/// Every regular file under `root`, mapped filename → contents.
///
/// Used instead of "assert the expected path exists" so a test can prove that
/// *both* the pre-existing file and the moved file survived, and say what the
/// tree actually looks like when they did not.
fn files_by_name(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            let body = fs::read_to_string(entry.path()).unwrap_or_default();
            out.insert(name, body);
        }
    }
    out
}

/// The full anyhow chain as a single string, for asserting on context.
fn chain(err: &anyhow::Error) -> String {
    format!("{err:#}")
}

/// Restores a directory's permissions when dropped.
///
/// The read-only test has to leave the directory writable again or `TempDir`
/// cannot clean it up, and it has to do so even when the assertion in the
/// middle panics — which is the normal outcome while the defect is unfixed.
#[cfg(unix)]
struct RestorePerms {
    dir: PathBuf,
    mode: u32,
}

#[cfg(unix)]
impl Drop for RestorePerms {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(self.mode));
    }
}

/// Make `dir` read-only, returning a guard that restores it — or `None` when
/// the mode does not actually deny writes.
///
/// Running as root (which some container-based CI images do) ignores the
/// permission bits entirely, and a test that silently asserts nothing is worse
/// than a test that says why it stood down. The probe below is a measurement,
/// not an assumption about the runner.
#[cfg(unix)]
fn deny_writes(dir: &Path) -> Option<RestorePerms> {
    use std::os::unix::fs::PermissionsExt as _;

    let original = fs::metadata(dir).unwrap().permissions().mode();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let guard = RestorePerms {
        dir: dir.to_path_buf(),
        mode: original,
    };

    let probe = dir.join(".write-probe");
    if fs::write(&probe, b"probe").is_ok() {
        let _ = fs::remove_file(&probe);
        return None;
    }
    Some(guard)
}

// ---------------------------------------------------------------------------
// Defect 1 — an existing destination must never be overwritten
// ---------------------------------------------------------------------------

/// A file already sitting at the exact planned destination must survive the
/// move, and so must the file being moved.
///
/// This is the plain-file form of the no-clobber contract. It passes today
/// because `resolve_collision` runs immediately before the rename in a
/// single-threaded test and sees the marker, so the `-1` suffix path is taken.
/// It is here to stay green once `execute_move` stops trusting that check.
#[test]
fn an_existing_destination_file_is_never_clobbered() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("input/holiday.jpg");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "MOVED-FILE").unwrap();

    let dest_dir = tmp.path().join("output/2024/01/15");
    fs::create_dir_all(&dest_dir).unwrap();
    let destination = dest_dir.join("2024-01-15-103000.jpg");
    fs::write(&destination, "PRE-EXISTING").unwrap();

    execute_move(&plan(&src, &destination)).expect("the move itself should succeed");

    let survivors = files_by_name(tmp.path());
    let bodies: Vec<&str> = survivors.values().map(String::as_str).collect();

    assert!(
        bodies.contains(&"PRE-EXISTING"),
        "the pre-existing file was clobbered; tree is {survivors:#?}"
    );
    assert!(
        bodies.contains(&"MOVED-FILE"),
        "the moved file did not arrive; tree is {survivors:#?}"
    );
    assert_eq!(
        survivors.len(),
        2,
        "expected both files to survive under distinct names; tree is {survivors:#?}"
    );
}

/// A dangling symlink at the destination is an existing filesystem entry, and
/// it must not be silently replaced either.
///
/// This is the deterministic form of the TOCTOU defect. `Path::exists()`
/// follows symlinks and therefore answers "no" for a link whose target is
/// missing, so `resolve_collision` hands the path straight through and
/// `fs::rename` destroys the link. No race, no timing, no second thread —
/// just a check that asks the wrong question. A `link`-based no-clobber move
/// fails with `AlreadyExists` here, which is the behaviour this pins.
#[cfg(unix)]
#[test]
fn a_dangling_symlink_at_the_destination_is_never_clobbered() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("input/holiday.jpg");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "MOVED-FILE").unwrap();

    let dest_dir = tmp.path().join("output/2024/01/15");
    fs::create_dir_all(&dest_dir).unwrap();
    let destination = dest_dir.join("2024-01-15-103000.jpg");
    std::os::unix::fs::symlink("./target-that-does-not-exist.jpg", &destination).unwrap();

    // Whether the move reports success is not what is under test — surviving
    // the attempt is.
    let _ = execute_move(&plan(&src, &destination));

    assert!(
        fs::symlink_metadata(&destination).is_ok(),
        "the pre-existing symlink at {} was replaced by the move",
        destination.display()
    );
    assert!(
        fs::symlink_metadata(&destination).unwrap().is_symlink(),
        "the entry at {} is no longer a symlink — the move overwrote it",
        destination.display()
    );
}

// ---------------------------------------------------------------------------
// Defect 2 — a vanished source is an error, not a copy attempt
// ---------------------------------------------------------------------------

/// A source deleted between planning and execution must produce an error that
/// names the missing source, and must not create anything at the destination.
///
/// Plans are built during the scan and executed much later, so this is a real
/// window in a long run over a large library, not a contrived one.
#[test]
fn a_source_deleted_after_planning_returns_an_error() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("input/holiday.jpg");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "MOVED-FILE").unwrap();

    let destination = tmp.path().join("output/2024/01/15/2024-01-15-103000.jpg");
    let planned = plan(&src, &destination);

    // The window: the plan is held, the file goes away.
    fs::remove_file(&src).unwrap();

    let err = execute_move(&planned)
        .expect_err("moving a source that no longer exists must not report success");

    assert!(
        chain(&err).contains(&src.display().to_string()),
        "the error should name the missing source; got: {}",
        chain(&err)
    );
    assert!(
        !destination.exists(),
        "nothing should have been created at {}",
        destination.display()
    );
}

// ---------------------------------------------------------------------------
// Defect 4 — errors must name the destination, and leave the source alone
// ---------------------------------------------------------------------------

/// A move into a directory that cannot be written must fail with an error
/// naming the destination, and must leave the source exactly where it was.
///
/// Today the rename fails with `PermissionDenied`, `execute_move` reads that
/// as "different volume", and the copy attempt fails for the same reason —
/// so the operator is handed an error about a temp file they never asked for,
/// naming only the source. The destination, which is the thing that is
/// actually wrong, never appears.
#[cfg(unix)]
#[test]
fn a_read_only_destination_errors_naming_the_destination_and_leaves_the_source() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("input/holiday.jpg");
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, "MOVED-FILE").unwrap();

    let dest_dir = tmp.path().join("output/2024/01/15");
    fs::create_dir_all(&dest_dir).unwrap();
    let destination = dest_dir.join("2024-01-15-103000.jpg");

    let Some(_guard) = deny_writes(&dest_dir) else {
        eprintln!(
            "SKIPPED a_read_only_destination_errors_naming_the_destination_and_leaves_the_source: \
             writes to a 0o555 directory succeeded, so this process ignores permission bits \
             (running as root?)"
        );
        return;
    };

    let err = execute_move(&plan(&src, &destination))
        .expect_err("moving into a read-only directory must not report success");

    assert!(
        fs::read_to_string(&src).unwrap_or_default() == "MOVED-FILE",
        "the source must be left untouched when the destination is unwritable"
    );
    assert!(
        chain(&err).contains(&dest_dir.display().to_string()),
        "the error must name the destination directory {}; got: {}",
        dest_dir.display(),
        chain(&err)
    );
}

// ---------------------------------------------------------------------------
// Defect 3 — content verification, not size verification
// ---------------------------------------------------------------------------

/// A cross-volume copy that lands the right number of bytes and the wrong
/// bytes must be detected, and the source must survive.
///
/// The scenario is built in full below — two files of identical length whose
/// BLAKE3 digests differ — which is exactly the input that today's
/// `src_size != tmp_size` check waves through before calling
/// `fs::remove_file` on the original.
///
/// It cannot be driven through `execute_move`, because corrupting the copy
/// requires substituting the copy step, and there is no seam to substitute it
/// at: `cross_volume_move` is private and does its own `fs::copy`. Extracting
/// a content-verifying `copy_verify_delete` is task 4 of this phase; this test
/// binds to it then, and until it exists the test fails with the reason —
/// which is the accurate record of the defect, not a green tick over a gap.
#[test]
fn a_same_size_but_corrupted_cross_volume_copy_must_not_delete_the_source() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("holiday.jpg");
    let corrupt = tmp.path().join("corrupt-copy.jpg");

    // 128 KiB apiece: two full read buffers, so a streaming hash has to get
    // past its first chunk to tell them apart.
    let original = vec![b'A'; 128 * 1024];
    let mut damaged = original.clone();
    damaged[64 * 1024] = b'B'; // one flipped byte, identical length

    fs::write(&src, &original).unwrap();
    fs::write(&corrupt, &damaged).unwrap();

    let src_len = fs::metadata(&src).unwrap().len();
    let corrupt_len = fs::metadata(&corrupt).unwrap().len();
    assert_eq!(
        src_len, corrupt_len,
        "the fixture must defeat a size-only check to be worth anything"
    );

    let src_digest = blake3::hash(&original).to_hex().to_string();
    let corrupt_digest = blake3::hash(&damaged).to_hex().to_string();
    assert_ne!(
        src_digest, corrupt_digest,
        "a content check must be able to tell these apart"
    );

    panic!(
        "no content-verified copy seam exists: `cross_volume_move` is private and verifies with \
         `metadata().len()` only, so a {src_len}-byte copy with digest {corrupt_digest} passes \
         verification against a source with digest {src_digest} and the source is then deleted. \
         Bind this test to the extracted `copy_verify_delete` in task 4 of Phase 02."
    );
}
