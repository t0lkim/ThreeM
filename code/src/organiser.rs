use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use tracing::{debug, error, info};

use crate::geocoder::GeoLookup;
use crate::hasher::DuplicateGroup;
use crate::metadata::{self, DateSource, FileMetadata};
use crate::scanner::ScannedFile;

/// A planned file operation (computed during scan, executed during process)
#[derive(Debug, Clone)]
pub struct PlannedMove {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub date_source: DateSource,
    pub has_location: bool,
}

/// Build the target path for a file based on its metadata
///
/// # Errors
///
/// Returns an error if the file's metadata cannot be extracted.
pub fn plan_move(file: &ScannedFile, output_dir: &Path, geo: &GeoLookup) -> Result<PlannedMove> {
    let meta = metadata::extract_metadata(&file.path, file.is_video)?;

    let (date_dir, filename) = build_target_path(&meta, &file.extension, geo);
    let destination = output_dir.join(date_dir).join(filename);

    Ok(PlannedMove {
        source: file.path.clone(),
        destination,
        date_source: meta.date_source,
        has_location: meta.latitude.is_some() && meta.longitude.is_some(),
    })
}

/// Build the directory path (YYYY/MM/DD) and filename (YYYY-MM-DD-HHMMSS[-location].ext)
// exposed for integration tests
pub fn build_target_path(
    meta: &FileMetadata,
    extension: &str,
    geo: &GeoLookup,
) -> (PathBuf, String) {
    if let Some(dt) = meta.date {
        let dir = date_directory(&dt);
        let filename = date_filename(&dt, meta, extension, geo);
        (dir, filename)
    } else {
        let dir = PathBuf::from("unsorted");
        let filename = format!("unknown.{extension}");
        (dir, filename)
    }
}

fn date_directory(dt: &DateTime<Utc>) -> PathBuf {
    PathBuf::from(format!("{}/{:02}/{:02}", dt.year(), dt.month(), dt.day()))
}

fn date_filename(
    dt: &DateTime<Utc>,
    meta: &FileMetadata,
    extension: &str,
    geo: &GeoLookup,
) -> String {
    let base = format!(
        "{}-{:02}-{:02}-{:02}{:02}{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );

    let location_part = match (meta.latitude, meta.longitude) {
        (Some(lat), Some(lon)) => geo
            .lookup(lat, lon)
            .map(|info| format!("-{}", info.filename_part)),
        _ => None,
    };

    match location_part {
        Some(loc) => format!("{base}{loc}.{extension}"),
        None => format!("{base}.{extension}"),
    }
}

/// The number of destination candidates tried before a move gives up.
///
/// A photo library would need ten thousand files claiming the same second and
/// the same location to exhaust this. Giving up is the right end of the range:
/// the alternative to a bounded search is an unbounded one, and the whole
/// point of this module is that no path ends in "overwrite it anyway".
const MAX_COLLISION_ATTEMPTS: usize = 10_000;

/// The `attempt`-th candidate destination for `path`.
///
/// Attempt 0 is `path` itself; attempt *n* is `stem-n.ext`. This is a pure
/// function of the path — it asks the filesystem nothing, deliberately.
/// The previous `resolve_collision` called `Path::exists()` and handed its
/// answer to `fs::rename`, which is wrong twice over: the answer is stale the
/// instant it is returned, and `exists()` follows symlinks, so a dangling link
/// reads as "nothing here" while the directory entry is very much there.
///
/// [`move_no_clobber`] is now the only authority on whether a candidate is
/// free, and it answers by failing rather than by overwriting. This function
/// only says which name to try next.
pub fn collision_candidate(path: &Path, attempt: usize) -> PathBuf {
    if attempt == 0 {
        return path.to_path_buf();
    }

    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let parent = path.parent().unwrap_or(Path::new("."));

    if ext.is_empty() {
        parent.join(format!("{stem}-{attempt}"))
    } else {
        parent.join(format!("{stem}-{attempt}.{ext}"))
    }
}

/// Move duplicate files into numbered subdirectories under duplicates/
/// Each duplicate group gets its own directory: duplicates/000/, duplicates/001/, etc.
/// The first file in each group is the "original" and is NOT moved here.
///
/// # Errors
///
/// Returns an error if a `duplicates/NNN/` directory or its `manifest.txt`
/// cannot be created. Individual failed moves are counted, not propagated.
pub fn move_duplicates(groups: &[DuplicateGroup], output_dir: &Path) -> Result<(usize, usize)> {
    let dup_base = output_dir.join("duplicates");
    let mut moved = 0;
    let mut errors = 0;

    for (i, group) in groups.iter().enumerate() {
        let group_dir = dup_base.join(format!("{i:03}"));
        fs::create_dir_all(&group_dir)
            .with_context(|| format!("creating duplicate dir {}", group_dir.display()))?;

        // Write a manifest file for the verifier
        let manifest_path = group_dir.join("manifest.txt");
        let mut manifest = format!(
            "# Duplicate group {:03}\n# BLAKE3 hash: {}\n# File size: {} bytes\n# Original kept at: {}\n\n",
            i, group.hash, group.size,
            group.files[0].display()
        );

        // Skip the first file (kept as original), move the rest
        for dup_path in group.files.iter().skip(1) {
            let filename = dup_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            // No pre-flight collision check: `execute_move` walks the
            // candidate names itself and lets the move be the authority on
            // which one is free.
            let dest = group_dir.join(&filename);

            let _ = writeln!(manifest, "{}", dup_path.display());

            let planned = PlannedMove {
                source: dup_path.clone(),
                destination: dest,
                date_source: DateSource::None,
                has_location: false,
            };

            match execute_move(&planned) {
                Ok(_) => moved += 1,
                Err(e) => {
                    error!(path = %dup_path.display(), error = %e, "failed to move duplicate");
                    errors += 1;
                }
            }
        }

        fs::write(&manifest_path, manifest)
            .with_context(|| format!("writing manifest {}", manifest_path.display()))?;
    }

    Ok((moved, errors))
}

/// What a completed move actually did.
///
/// Worth recording because the two are not equivalent under interruption: a
/// same-volume move creates a directory entry and drops another, and cannot
/// half-happen to the file's contents, while a cross-volume move reads and
/// rewrites every byte. Callers and the journal want to know which one moved
/// a given photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    /// Same volume: the destination was linked to the source's inode and the
    /// source link dropped. No data was copied.
    Renamed,
    /// Different volumes: copied to a temp file, verified, promoted into
    /// place, and only then was the source removed.
    CrossVolume,
}

impl std::fmt::Display for MoveKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Renamed => f.write_str("same-volume link"),
            Self::CrossVolume => f.write_str("cross-volume copy+verify+delete"),
        }
    }
}

/// Why a no-clobber move did not happen.
///
/// The split exists because the caller has to tell "that name is taken, try
/// the next one" from "stop, something is wrong". `anyhow` alone cannot carry
/// that distinction without downcasting to an `io::Error` that context has
/// already wrapped, and getting it wrong in either direction is expensive: a
/// missed retry loses a photo's move, a spurious one writes a `-1` copy next
/// to a file that failed for an unrelated reason.
#[derive(Debug)]
enum MoveError {
    /// `dst` already exists. Not fatal — [`execute_move`] tries the next
    /// candidate name.
    DestinationExists(PathBuf),
    /// Anything else, with its context already attached.
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for MoveError {
    fn from(err: anyhow::Error) -> Self {
        Self::Fatal(err)
    }
}

/// The step of a same-volume move that failed.
///
/// Only a failed `link` can mean "these two paths are on different volumes";
/// a failed `unlink` of the source never does, and must not be allowed to send
/// the move down the copy path.
#[derive(Debug, Clone, Copy)]
enum LinkStep {
    Link,
    UnlinkSource,
}

/// `link(src, dst)` then `unlink(src)` — a same-volume move that cannot
/// overwrite `dst`.
///
/// `link(2)` fails with `EEXIST` if anything at all occupies `dst`, including
/// a dangling symlink, which is precisely the question `Path::exists()` gets
/// wrong. It is also the reason this is not `fs::rename`: rename replaces the
/// destination silently and unconditionally, and there is no flag on the
/// stable `std` API to ask it not to.
///
/// Not atomic in the way rename is — there is a window where both names point
/// at the file — but the window contains no state in which data is missing,
/// and the unlink failure path below closes it rather than leaving two names.
fn link_and_unlink(src: &Path, dst: &Path) -> Result<(), (LinkStep, io::Error)> {
    fs::hard_link(src, dst).map_err(|e| (LinkStep::Link, e))?;

    if let Err(e) = fs::remove_file(src) {
        // The link landed but the source will not go away. Undo the link
        // rather than leave the run with two names for one file, which the
        // dedup pass would later "helpfully" report as a duplicate.
        let _ = fs::remove_file(dst);
        return Err((LinkStep::UnlinkSource, e));
    }

    Ok(())
}

/// Move `src` to `dst`, failing if `dst` is taken rather than overwriting it.
///
/// # Errors
///
/// [`MoveError::DestinationExists`] when something already occupies `dst` —
/// the caller is expected to try another name — and [`MoveError::Fatal`] for
/// everything else.
fn move_no_clobber(src: &Path, dst: &Path) -> Result<MoveKind, MoveError> {
    match link_and_unlink(src, dst) {
        Ok(()) => Ok(MoveKind::Renamed),

        Err((LinkStep::Link, e)) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(MoveError::DestinationExists(dst.to_path_buf()))
        }

        // Any other link failure still falls through to the copy path, which
        // is the misclassification task 3 of Phase 02 narrows to `EXDEV`
        // alone. Note also that a filesystem without hard links (exFAT, FAT32)
        // lands here and then fails again in the promotion below — safely, with
        // the source intact, but it fails. That gap belongs to task 3 too.
        Err((LinkStep::Link, _)) => cross_volume_move(src, dst).map(|()| MoveKind::CrossVolume),

        Err((LinkStep::UnlinkSource, e)) => {
            Err(MoveError::Fatal(anyhow::Error::new(e).context(format!(
                "removing source {} after linking it to {}",
                src.display(),
                dst.display()
            ))))
        }
    }
}

/// Execute a planned move, never overwriting an existing file
///
/// Walks the collision candidates for the planned destination and lets
/// [`move_no_clobber`] decide which one is free, rather than asking the
/// filesystem beforehand and trusting the answer to still hold.
///
/// # Errors
///
/// Returns an error if the destination has no parent directory, if that
/// directory cannot be created, if the move itself fails, or if every one of
/// [`MAX_COLLISION_ATTEMPTS`] candidate names is taken.
pub fn execute_move(planned: &PlannedMove) -> Result<MoveKind> {
    let dest_dir = planned
        .destination
        .parent()
        .context("destination has no parent directory")?;

    // Create target directory
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating directory {}", dest_dir.display()))?;

    for attempt in 0..MAX_COLLISION_ATTEMPTS {
        let candidate = collision_candidate(&planned.destination, attempt);

        match move_no_clobber(&planned.source, &candidate) {
            Ok(kind) => {
                info!(
                    src = %planned.source.display(),
                    dst = %candidate.display(),
                    kind = %kind,
                    "moved"
                );
                return Ok(kind);
            }
            Err(MoveError::DestinationExists(taken)) => {
                debug!(
                    src = %planned.source.display(),
                    candidate = %taken.display(),
                    "destination taken, trying the next candidate"
                );
            }
            Err(MoveError::Fatal(e)) => return Err(e),
        }
    }

    bail!(
        "no free destination for {} after {} candidates around {}",
        planned.source.display(),
        MAX_COLLISION_ATTEMPTS,
        planned.destination.display()
    )
}

/// Safe cross-volume move: copy → verify → delete source
fn cross_volume_move(src: &Path, dst: &Path) -> Result<(), MoveError> {
    let dst_dir = dst.parent().context("destination has no parent")?;

    // Copy to temp file in same directory as destination
    let temp_name = format!(".tmp-{}", chrono::Utc::now().timestamp_millis());
    let temp_path = dst_dir.join(temp_name);

    fs::copy(src, &temp_path).with_context(|| format!("copying {} to temp file", src.display()))?;

    // Verify the copy by comparing sizes
    let src_size = fs::metadata(src)
        .with_context(|| format!("reading source metadata: {}", src.display()))?
        .len();
    let tmp_size = fs::metadata(&temp_path)
        .with_context(|| format!("reading temp file metadata: {}", temp_path.display()))?
        .len();

    if src_size != tmp_size {
        // Clean up temp file and bail
        let _ = fs::remove_file(&temp_path);
        return Err(MoveError::Fatal(anyhow::anyhow!(
            "copy verification failed for {}: source {} bytes, copy {} bytes",
            src.display(),
            src_size,
            tmp_size
        )));
    }

    // Promote the temp file into place. Same directory, therefore same volume,
    // so this is the link path — and it refuses an occupied destination for
    // the same reason the first attempt did.
    match link_and_unlink(&temp_path, dst) {
        Ok(()) => {}
        Err((LinkStep::Link, e)) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temp_path);
            return Err(MoveError::DestinationExists(dst.to_path_buf()));
        }
        Err((_, e)) => {
            let _ = fs::remove_file(&temp_path);
            return Err(MoveError::Fatal(
                anyhow::Error::new(e).context(format!("promoting temp file to {}", dst.display())),
            ));
        }
    }

    // Only now delete the source
    fs::remove_file(src).with_context(|| format!("removing source file {}", src.display()))?;

    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_date_directory() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap()
            .and_utc();
        assert_eq!(date_directory(&dt), PathBuf::from("2024/03/15"));
    }

    /// A planned move with the metadata fields pinned inert — nothing in the
    /// move path reads them.
    fn plan(src: &Path, dst: &Path) -> PlannedMove {
        PlannedMove {
            source: src.to_path_buf(),
            destination: dst.to_path_buf(),
            date_source: DateSource::None,
            has_location: false,
        }
    }

    #[test]
    fn test_collision_candidate_zero_is_the_path_itself() {
        let path = Path::new("/photos/2024/01/15/photo.jpg");
        assert_eq!(collision_candidate(path, 0), path);
    }

    #[test]
    fn test_collision_candidate_appends_the_attempt_number() {
        let path = Path::new("/photos/photo.jpg");
        assert_eq!(
            collision_candidate(path, 1),
            PathBuf::from("/photos/photo-1.jpg")
        );
        assert_eq!(
            collision_candidate(path, 2),
            PathBuf::from("/photos/photo-2.jpg")
        );
    }

    #[test]
    fn test_collision_candidate_without_extension() {
        let path = Path::new("/photos/photo");
        assert_eq!(
            collision_candidate(path, 3),
            PathBuf::from("/photos/photo-3")
        );
    }

    /// The candidate function must not consult the filesystem: an occupied
    /// path still yields itself at attempt 0. Anything else would be the old
    /// `exists()`-then-rename shape wearing a new name.
    #[test]
    fn test_collision_candidate_ignores_what_is_on_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("photo.jpg");
        fs::write(&path, b"occupied").unwrap();
        assert_eq!(collision_candidate(&path, 0), path);
    }

    #[test]
    fn test_execute_move_same_volume() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst_dir = tmp.path().join("2024/01/15");
        let dst = dst_dir.join("2024-01-15-103000.jpg");
        fs::write(&src, b"image data").unwrap();

        let kind = execute_move(&plan(&src, &dst)).unwrap();

        assert_eq!(kind, MoveKind::Renamed, "a move within one volume links");
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"image data");
    }

    /// The no-clobber contract at its own level: an occupied destination is a
    /// refusal, not an overwrite, and both files are untouched afterwards.
    #[test]
    fn test_move_no_clobber_refuses_an_occupied_destination() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("taken.jpg");
        fs::write(&src, b"MOVED").unwrap();
        fs::write(&dst, b"PRE-EXISTING").unwrap();

        let err = move_no_clobber(&src, &dst).expect_err("an occupied destination must refuse");

        assert!(
            matches!(err, MoveError::DestinationExists(ref p) if p == &dst),
            "expected DestinationExists({}), got {err:?}",
            dst.display()
        );
        assert_eq!(fs::read(&dst).unwrap(), b"PRE-EXISTING");
        assert_eq!(fs::read(&src).unwrap(), b"MOVED");
    }

    /// `Path::exists()` follows symlinks, so a dangling link reads as "nothing
    /// here" while the directory entry is very much there. `link(2)` asks the
    /// right question and answers `EEXIST`.
    #[cfg(unix)]
    #[test]
    fn test_move_no_clobber_refuses_a_dangling_symlink() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("dangling.jpg");
        fs::write(&src, b"MOVED").unwrap();
        std::os::unix::fs::symlink("./nothing-here.jpg", &dst).unwrap();

        assert!(
            !dst.exists(),
            "the fixture is only meaningful while dangling"
        );

        let err = move_no_clobber(&src, &dst).expect_err("a dangling symlink is an existing entry");

        assert!(
            matches!(err, MoveError::DestinationExists(_)),
            "got {err:?}"
        );
        assert!(fs::symlink_metadata(&dst).unwrap().is_symlink());
    }

    /// `execute_move` walks the candidates until one is free rather than
    /// trusting a single pre-flight check.
    #[test]
    fn test_execute_move_retries_past_taken_candidates() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("out/photo.jpg");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&src, b"MOVED").unwrap();
        fs::write(&dst, b"TAKEN-0").unwrap();
        fs::write(tmp.path().join("out/photo-1.jpg"), b"TAKEN-1").unwrap();

        execute_move(&plan(&src, &dst)).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"TAKEN-0");
        assert_eq!(
            fs::read(tmp.path().join("out/photo-1.jpg")).unwrap(),
            b"TAKEN-1"
        );
        assert_eq!(
            fs::read(tmp.path().join("out/photo-2.jpg")).unwrap(),
            b"MOVED",
            "the move should have landed on the first free candidate"
        );
        assert!(!src.exists());
    }

    /// When the link succeeds but the source cannot be unlinked, the new link
    /// is undone — the run must not end with two names for one file, which the
    /// dedup pass would later report as a duplicate of itself.
    ///
    /// Skips itself with a printed reason where permission bits do not deny
    /// writes (running as root, as some CI containers do).
    #[cfg(unix)]
    #[test]
    fn test_a_failed_source_unlink_undoes_the_link() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("input");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("source.jpg");
        let dst = tmp.path().join("photo.jpg");
        fs::write(&src, b"MOVED").unwrap();

        let original = fs::metadata(&src_dir).unwrap().permissions().mode();
        fs::set_permissions(&src_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = move_no_clobber(&src, &dst);
        let unlink_denied = fs::remove_file(src_dir.join(".probe")).is_err()
            && fs::write(src_dir.join(".probe"), b"p").is_err();

        // Restore before asserting, or `TempDir` cannot clean up after a panic.
        fs::set_permissions(&src_dir, fs::Permissions::from_mode(original)).unwrap();

        if !unlink_denied {
            eprintln!(
                "SKIPPED test_a_failed_source_unlink_undoes_the_link: writes to a 0o555 \
                 directory succeeded, so this process ignores permission bits (running as root?)"
            );
            return;
        }

        assert!(
            outcome.is_err(),
            "a move that could not drop the source link must not report success"
        );
        assert!(
            !dst.exists(),
            "the link at {} should have been undone",
            dst.display()
        );
        assert_eq!(fs::read(&src).unwrap(), b"MOVED");
    }

    #[test]
    fn test_build_target_path_no_date() {
        let meta = FileMetadata {
            date: None,
            latitude: None,
            longitude: None,
            date_source: DateSource::None,
        };
        let geo = GeoLookup::new();
        let (dir, name) = build_target_path(&meta, "jpg", &geo);
        assert_eq!(dir, PathBuf::from("unsorted"));
        assert_eq!(name, "unknown.jpg");
    }
}
