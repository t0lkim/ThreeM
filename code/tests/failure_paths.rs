//! Regression suite for the destructive-path defects in `organiser`.
//!
//! Every test here was written *before* the fix it describes, and each one
//! encodes a way `ThreeM` could lose someone's photos. They are deliberately
//! library-level rather than binary-level: the defects live in
//! [`mmm::organiser::execute_move`] and the copy path beneath it, and driving
//! them through the CLI would add a scan, a plan and a progress bar between the
//! assertion and the thing being asserted.
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
//! Defect 1 is fixed as of task 2 of the phase: `execute_move` now walks the
//! collision candidates and lets `move_no_clobber` — `link(2)` plus `unlink`,
//! which fails `EEXIST` on any occupied name including a dangling symlink —
//! decide which one is free. Both no-clobber tests below pass on merit rather
//! than on ordering luck.
//!
//! Defects 2 and 4 are fixed as of task 3: a failed `link` is classified
//! before it is acted on, and only `EXDEV` — or a destination filesystem with
//! no hard links at all — reaches the copy path. Everything else propagates
//! with both paths named, so the missing-source test now passes because the
//! error is deliberate rather than because a doomed copy happened to fail the
//! same way.
//!
//! Defect 3 is fixed as of task 4: `cross_volume_move` is now a thin call to
//! `copy_verify_delete`, which hashes the source as it streams it into the temp
//! file, hashes the file that landed, and refuses to remove the source unless
//! the two BLAKE3 digests agree. The copy step is a parameter, which is what
//! lets the corruption test below inject damage at the one place a failing
//! drive or cable would introduce it.
//!
//! Every test in this file passes as of task 4. The per-task record of what
//! failed when, and why, is in
//! `.maestro/playbooks/Initiation/Phase-02-Destructive-Path-Hardening.md`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mmm::metadata::DateSource;
use mmm::organiser::{copy_verify_delete, execute_move, PlannedMove};
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
///
/// This passed before task 3 by accident: the rename failed `ENOENT`,
/// `execute_move` read that as "different volume", and the copy failed for the
/// same reason — the right outcome reached by the wrong route, and the error
/// named a temp file rather than the missing photo. It now fails as a
/// `NotFound` naming both paths, with no copy attempted.
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
/// Before task 3 the link failed with `PermissionDenied`, `execute_move` read
/// that as "different volume", and the copy attempt failed for the same reason
/// — so the operator was handed an error about a temp file they never asked
/// for, naming only the source. The destination, which is the thing that is
/// actually wrong, never appeared. `EACCES` is now classified as fatal and the
/// error names both paths.
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
/// The scenario is exactly the input that a `src_size != tmp_size` check waves
/// through before calling `fs::remove_file` on the original: two byte-streams
/// of identical length whose BLAKE3 digests differ.
///
/// It cannot be driven through `execute_move`, because corrupting the copy
/// means substituting the copy step, and a real cross-volume move needs a
/// second mounted filesystem no test runner can be assumed to have. Task 4
/// extracts `copy_verify_delete` with the copy step as a parameter, so the
/// damage can be injected at the one place a bad drive, a bad cable or a bad
/// filesystem would do it: between reading the source and writing the copy.
///
/// The injected step is honest about the source — it returns the true digest
/// of the bytes it read — and dishonest only about what it wrote, which is the
/// shape of real copy corruption. Nothing but a content comparison can catch it.
#[test]
fn a_same_size_but_corrupted_cross_volume_copy_must_not_delete_the_source() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("input/holiday.jpg");
    fs::create_dir_all(src.parent().unwrap()).unwrap();

    // 128 KiB: two full read buffers, so a streaming hash has to get past its
    // first chunk to tell the two apart.
    let original = vec![b'A'; 128 * 1024];
    fs::write(&src, &original).unwrap();

    let dest_dir = tmp.path().join("output/2024/01/15");
    fs::create_dir_all(&dest_dir).unwrap();
    let destination = dest_dir.join("2024-01-15-103000.jpg");

    let mut damaged = original.clone();
    damaged[64 * 1024] = b'B'; // one flipped byte, identical length

    let src_digest = blake3::hash(&original).to_hex().to_string();
    let corrupt_digest = blake3::hash(&damaged).to_hex().to_string();
    assert_ne!(
        src_digest, corrupt_digest,
        "a content check must be able to tell these apart"
    );

    // The seam: reads the source truthfully, writes something else.
    let corrupting_copy = |from: &Path, temp: &Path| -> anyhow::Result<String> {
        let read = fs::read(from)?;
        let digest = blake3::hash(&read).to_hex().to_string();
        fs::write(temp, &damaged)?;
        Ok(digest)
    };

    let err = copy_verify_delete(&src, &destination, corrupting_copy)
        .expect_err("a copy whose contents differ from the source must not verify");
    let message = format!("{err}");

    assert!(
        message.contains(&src_digest) && message.contains(&corrupt_digest),
        "the error must state both digests; got: {message}"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        original,
        "the source must be left byte-for-byte intact when verification fails"
    );
    assert!(
        !destination.exists(),
        "the unverified copy must not be promoted to {}",
        destination.display()
    );

    let leftovers: Vec<String> = fs::read_dir(&dest_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the temp file must be cleaned up on the mismatch path; found {leftovers:?}"
    );
}

/// The same seam on its success path: a copy that matches lands the bytes
/// exactly and removes the source, and leaves no temp file behind.
///
/// This is the guarantee the verification exists to protect. Driven through
/// `copy_verify_delete` with the real copy step, which is the code an actual
/// cross-volume move runs — only the "different volume" part is simulated,
/// because a second mount is not available to a test runner.
#[test]
fn a_verified_cross_volume_copy_preserves_content_and_removes_the_source() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("input/holiday.jpg");
    fs::create_dir_all(src.parent().unwrap()).unwrap();

    // Deliberately not a round number of buffers: the last read is short, which
    // is where an off-by-one in the streaming loop would show.
    let original: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(&src, &original).unwrap();

    let dest_dir = tmp.path().join("output/2024/01/15");
    fs::create_dir_all(&dest_dir).unwrap();
    let destination = dest_dir.join("2024-01-15-103000.jpg");

    copy_verify_delete(&src, &destination, mmm::hasher::copy_hashing)
        .expect("a copy that matches its source must verify");

    assert_eq!(
        fs::read(&destination).unwrap(),
        original,
        "the copy must be byte-for-byte identical to the source"
    );
    assert!(
        !src.exists(),
        "the source must be gone once the copy landed"
    );

    let leftovers: Vec<String> = fs::read_dir(&dest_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the temp file must be gone on the success path; found {leftovers:?}"
    );
}
