use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, warn};

use crate::scanner::ScannedFile;

/// Size of the partial hash read (first + last N bytes)
const PARTIAL_HASH_BYTES: u64 = 64 * 1024; // 64KB

/// Read buffer shared by every streaming operation in this module — the full
/// hash, and the verified copy the organiser's cross-volume move runs.
///
/// One constant rather than one per function, so a copy and the hash that
/// verifies it can never disagree about how they walk a file.
const STREAM_BUFFER_BYTES: usize = 128 * 1024;

/// A file the cascade is passing through to be organised, and the digest it
/// happens to already know for it.
///
/// The hash is carried rather than recomputed because it is *free*: a file that
/// reached phase 3 was fully hashed to decide whether it was a duplicate, and
/// throwing that digest away only to have the journal want it later would mean
/// reading every such file twice. A file eliminated in phase 1 or 2 has no
/// digest and gets `None` — establishing one would mean a full read of every
/// file in the library, which is the cost the cascade exists to avoid.
#[derive(Debug, Clone)]
pub struct UniqueFile {
    pub file: ScannedFile,
    /// The full BLAKE3 digest, when phase 3 already computed one.
    pub known_hash: Option<String>,
}

/// Result of the three-phase dedup analysis
#[derive(Debug)]
pub struct DedupResult {
    /// Files that are unique (no duplicates found)
    pub unique: Vec<UniqueFile>,
    /// Groups of duplicate files (each group shares identical content)
    pub duplicate_groups: Vec<DuplicateGroup>,
    /// Files excluded from the analysis because their contents could not be
    /// read — deleted or truncated mid-run, or permission denied.
    ///
    /// Excluded means excluded from *everything*: they are absent from
    /// `unique` too, so nothing downstream moves a file whose content this
    /// pass never managed to establish. That is deliberate, and it is why the
    /// count exists — a file dropped from the plan must be reported, or the
    /// operator reads a clean summary over a library that was quietly only
    /// partly processed.
    pub skipped: usize,
}

/// Files grouped by a hash, plus a count of those that could not be hashed.
///
/// One unreadable file used to abort the whole dedup pass with `?`. A photo
/// library is a live filesystem — files get deleted, truncated and locked
/// underneath a long run — so a per-file read failure is ordinary, and the
/// only correct response is to leave that file out of the comparison and say
/// so.
#[derive(Debug)]
pub struct HashGroups<'a> {
    pub groups: HashMap<String, Vec<&'a ScannedFile>>,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub hash: String,
    pub size: u64,
    /// Every file sharing this content, in [`by_depth_then_path`] order.
    ///
    /// **`files[0]` is the original**, the one copy left where it is; every
    /// other entry is relocated into `duplicates/NNN/`. That is a rule, not an
    /// accident of iteration — see [`by_depth_then_path`].
    pub files: Vec<PathBuf>,
}

/// The order two paths take when the choice between them must not depend on
/// how a `HashMap` happened to iterate: **shallower first, then
/// lexicographically smaller**.
///
/// Depth leads because the first file of a duplicate group is the one that is
/// *kept*. Of two identical photographs, the one nearer the top of the tree is
/// far more likely to be the one somebody filed deliberately — copies
/// accumulate downwards, in `old-phone/DCIM/backup/`. Lexicographic order then
/// breaks the tie, which makes the rule total: for any two distinct paths it
/// names one of them, with no appeal to iteration order, filesystem walk order
/// or (once this cascade is parallel) thread completion order.
fn by_depth_then_path(a: &Path, b: &Path) -> Ordering {
    a.components()
        .count()
        .cmp(&b.components().count())
        .then_with(|| a.cmp(b))
}

/// Three-phase dedup cascade:
/// 1. Group by file size (free — metadata only)
/// 2. Partial BLAKE3 hash (first 64KB + last 64KB)
/// 3. Full BLAKE3 hash (only for partial-hash matches)
///
/// Infallible by construction. A file that cannot be read is dropped from the
/// analysis with a warning and counted in [`DedupResult::skipped`]; it never
/// takes the rest of the run down with it.
///
/// **Deterministic by construction too.** The cascade's working sets are
/// `HashMap`s, and a `HashMap` iterates in an order that differs between two
/// runs over the same tree — so which file a group kept as its original, and
/// the order the groups came back in, used to be decided by a random seed.
/// That is not a cosmetic difference: the retained original is the copy that is
/// *not* moved into `duplicates/`, and the order of `unique` decides which of
/// two files competing for one name gets `photo.jpg` and which gets
/// `photo-1.jpg`. Both are now settled by [`by_depth_then_path`] and by the
/// content hash, applied *after* the hashing is finished — which is also what
/// keeps the answer stable once the hashing runs on several threads.
pub fn find_duplicates(files: &[ScannedFile], progress: &ProgressBar) -> DedupResult {
    progress.set_message("Phase 1: grouping by file size");
    let size_groups = group_by_size(files);

    // Files with unique sizes are immediately unique
    let mut unique: Vec<UniqueFile> = Vec::new();
    let mut candidates: Vec<Vec<&ScannedFile>> = Vec::new();

    for group in size_groups.values() {
        if group.len() == 1 {
            unique.push(UniqueFile {
                file: group[0].clone(),
                known_hash: None,
            });
        } else {
            candidates.push(group.iter().collect());
        }
    }

    debug!(
        unique = unique.len(),
        candidate_groups = candidates.len(),
        "phase 1 complete"
    );

    // Phase 2: Partial hash
    progress.set_message("Phase 2: partial hashing size-matched files");
    let mut phase3_candidates: Vec<Vec<&ScannedFile>> = Vec::new();
    let mut skipped = 0;

    for group in &candidates {
        let partial = group_by_partial_hash(group);
        skipped += partial.skipped;
        for (_hash, pgroup) in partial.groups {
            if pgroup.len() == 1 {
                // A *partial* hash is head-plus-tail, not the whole file — it
                // is not the digest the journal would need, so this one stays
                // unhashed rather than recording something that only looks like
                // a full digest.
                unique.push(UniqueFile {
                    file: pgroup[0].clone(),
                    known_hash: None,
                });
            } else {
                phase3_candidates.push(pgroup);
            }
        }
        progress.inc(group.len() as u64);
    }

    debug!(phase3_groups = phase3_candidates.len(), "phase 2 complete");

    // Phase 3: Full hash
    progress.set_message("Phase 3: full hashing confirmed candidates");
    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();

    for group in &phase3_candidates {
        let full = group_by_full_hash(group);
        skipped += full.skipped;
        for (hash, mut fgroup) in full.groups {
            // Before anything reads `fgroup[0]`, and before the group is
            // emitted: the members arrive here in whatever order phase 3
            // bucketed them, and index 0 is the file that will be left alone.
            fgroup.sort_by(|a, b| by_depth_then_path(&a.path, &b.path));

            if fgroup.len() == 1 {
                // Fully hashed to prove it was not a duplicate; the digest is
                // already paid for, so the journal gets it.
                unique.push(UniqueFile {
                    file: fgroup[0].clone(),
                    known_hash: Some(hash),
                });
            } else {
                // Keep the first file as the "original", rest are duplicates.
                // Which one that is, is `by_depth_then_path`'s answer, applied
                // just above. It carries the group's digest for the same reason.
                unique.push(UniqueFile {
                    file: fgroup[0].clone(),
                    known_hash: Some(hash.clone()),
                });
                duplicate_groups.push(DuplicateGroup {
                    hash,
                    size: fgroup[0].size,
                    files: fgroup.iter().map(|f| f.path.clone()).collect(),
                });
            }
        }
        progress.inc(group.len() as u64);
    }

    // Both output lists are assembled from `HashMap` iteration, so both are
    // sorted here rather than relied upon to arrive in order. Groups go by
    // their content hash — the one property of a group that is derived from
    // nothing but its contents — with the retained original's path as a
    // tie-break, so the order is total even if two groups ever shared a digest.
    duplicate_groups.sort_by(|a, b| {
        a.hash
            .cmp(&b.hash)
            .then_with(|| match (a.files.first(), b.files.first()) {
                (Some(x), Some(y)) => by_depth_then_path(x, y),
                _ => Ordering::Equal,
            })
    });
    // `unique` is what the organiser plans moves from, in order. Left unsorted
    // it decides collision suffixes by coin toss.
    unique.sort_by(|a, b| by_depth_then_path(&a.file.path, &b.file.path));

    if skipped > 0 {
        warn!(count = skipped, "files excluded from duplicate detection");
    }

    DedupResult {
        unique,
        duplicate_groups,
        skipped,
    }
}

// exposed for integration tests
pub fn group_by_size(files: &[ScannedFile]) -> HashMap<u64, Vec<ScannedFile>> {
    let mut groups: HashMap<u64, Vec<ScannedFile>> = HashMap::new();
    for file in files {
        groups.entry(file.size).or_default().push(file.clone());
    }
    groups
}

/// Group files by a partial (head + tail) BLAKE3 hash.
///
/// A file that cannot be opened, seeked or read is left out of the returned
/// groups and counted in [`HashGroups::skipped`], with a warning naming it.
// exposed for integration tests
pub fn group_by_partial_hash<'a>(files: &[&'a ScannedFile]) -> HashGroups<'a> {
    group_by(files, |file| partial_hash(&file.path))
}

/// Group files by a full-content BLAKE3 hash.
///
/// A file that cannot be opened or read to completion is left out of the
/// returned groups and counted in [`HashGroups::skipped`], with a warning
/// naming it.
// exposed for integration tests
pub fn group_by_full_hash<'a>(files: &[&'a ScannedFile]) -> HashGroups<'a> {
    group_by(files, |file| full_hash(&file.path))
}

/// The shared body of both grouping passes: hash each file, bucket it by the
/// digest, and drop — loudly — the ones that would not read.
///
/// One implementation rather than two, because the interesting behaviour here
/// is the skip, and two copies of it are two chances for one of them to go
/// back to `?` unnoticed.
fn group_by<'a>(
    files: &[&'a ScannedFile],
    hash: impl Fn(&ScannedFile) -> Result<String>,
) -> HashGroups<'a> {
    let mut groups: HashMap<String, Vec<&'a ScannedFile>> = HashMap::new();
    let mut skipped = 0;

    for file in files {
        match hash(file) {
            Ok(digest) => groups.entry(digest).or_default().push(file),
            Err(e) => {
                warn!(
                    path = %file.path.display(),
                    error = %format!("{e:#}"),
                    "cannot read this file — excluding it from duplicate detection"
                );
                skipped += 1;
            }
        }
    }

    HashGroups { groups, skipped }
}

/// Hash first 64KB + last 64KB of a file using BLAKE3.
///
/// The length is taken from the open handle, not from the scan. The old code
/// sized a `read_exact` from the scan-time figure, so a file that shrank in
/// between — a camera import still writing, a sync client rewriting a photo —
/// failed with "failed to fill whole buffer" and took the whole run with it.
/// Every read here tolerates a short answer, and hashes what was actually
/// there.
fn partial_hash(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for partial hash", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("reading the length of {}", path.display()))?
        .len();

    let mut hasher = blake3::Hasher::new();
    let mut buf = Vec::new();

    // Read first chunk
    read_up_to(&mut file, PARTIAL_HASH_BYTES, &mut buf)
        .with_context(|| format!("reading first bytes of {}", path.display()))?;
    hasher.update(&buf);

    // Read last chunk (if file is large enough for it to differ from the first)
    if size > PARTIAL_HASH_BYTES * 2 {
        let tail_offset = i64::try_from(PARTIAL_HASH_BYTES)
            .context("partial-hash chunk size does not fit in i64")?;
        file.seek(SeekFrom::End(-tail_offset))
            .with_context(|| format!("seeking in {}", path.display()))?;
        read_up_to(&mut file, PARTIAL_HASH_BYTES, &mut buf)
            .with_context(|| format!("reading last bytes of {}", path.display()))?;
        hasher.update(&buf);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Read at most `limit` bytes into `buf`, replacing whatever it held.
///
/// Short reads are not an error: end-of-file is an answer, not a failure.
fn read_up_to<R: Read>(reader: &mut R, limit: u64, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    reader.by_ref().take(limit).read_to_end(buf)?;
    Ok(())
}

/// Stream everything `reader` yields through BLAKE3 and return the hex digest.
///
/// The single hashing primitive in the crate. Dedup and the organiser's
/// cross-volume verification both reach content identity through this function
/// and no other, because two implementations of "is this the same file" are two
/// chances to disagree — and the one place they would disagree is immediately
/// before `remove_file` on somebody's only copy of a photograph.
///
/// # Errors
///
/// Returns the reader's own error, uncontextualised — the caller knows what it
/// is reading and this function does not.
pub fn hash_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_BUFFER_BYTES];

    loop {
        let bytes_read = reader.read(&mut buf)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buf[..bytes_read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Full streaming BLAKE3 hash of a file.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read to completion.
pub fn full_hash(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for full hash", path.display()))?;

    hash_reader(&mut file).with_context(|| format!("reading {}", path.display()))
}

/// Copy `src` to `dst`, hashing the bytes on the way through, and return the
/// digest of what was *read*.
///
/// One pass over the file, not two: the source is read once and each buffer is
/// both hashed and written. The digest describes the source as it was actually
/// read during this copy, which is the only version of it that matters — the
/// caller hashes the file that landed and compares.
///
/// `dst` is created with `O_CREAT | O_EXCL`, so this never writes over an
/// existing file. The write is flushed and `fsync`ed before returning, because
/// the caller deletes the source immediately afterwards and a copy still living
/// in the page cache is not yet a copy.
///
/// # Errors
///
/// Returns an error if `src` cannot be opened or read, if `dst` already exists
/// or cannot be created, or if the write, flush or sync fails.
pub fn copy_hashing(src: &Path, dst: &Path) -> Result<String> {
    let mut input =
        File::open(src).with_context(|| format!("opening {} to copy it", src.display()))?;
    let mut output = File::create_new(dst)
        .with_context(|| format!("creating {} to copy into", dst.display()))?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_BUFFER_BYTES];

    loop {
        let bytes_read = input
            .read(&mut buf)
            .with_context(|| format!("reading {}", src.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buf[..bytes_read]);
        output
            .write_all(&buf[..bytes_read])
            .with_context(|| format!("writing {}", dst.display()))?;
    }

    // `fs::copy` carries the source's mode across; `File::create_new` does not,
    // so a read-only original would silently become writable without this.
    if let Ok(metadata) = input.metadata() {
        let _ = output.set_permissions(metadata.permissions());
    }

    output
        .sync_all()
        .with_context(|| format!("flushing {} to disk", dst.display()))?;

    Ok(hasher.finalize().to_hex().to_string())
}

/// Create a progress bar styled for hashing operations
pub fn hashing_progress_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(styled_bar(
        "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}",
    ));
    pb
}

/// Build a bar style from `template`, falling back to the default bar if the
/// template is malformed — cosmetics must never abort a run.
pub fn styled_bar(template: &str) -> ProgressStyle {
    ProgressStyle::default_bar().template(template).map_or_else(
        |_| ProgressStyle::default_bar(),
        |s| s.progress_chars("##-"),
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::fs;
    use tempfile::TempDir;

    fn make_scanned(path: PathBuf, size: u64) -> ScannedFile {
        ScannedFile {
            path,
            size,
            extension: "jpg".to_string(),
            is_video: false,
        }
    }

    #[test]
    fn test_unique_files_by_size() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        fs::write(&f1, b"short").unwrap();
        fs::write(&f2, b"much longer content here").unwrap();

        let files = vec![make_scanned(f1, 5), make_scanned(f2, 24)];

        let result = find_duplicates(&files, &ProgressBar::hidden());
        assert_eq!(result.unique.len(), 2);
        assert!(result.duplicate_groups.is_empty());
    }

    #[test]
    fn test_exact_duplicates_detected() {
        let tmp = TempDir::new().unwrap();
        let content = b"identical content for both files";
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        fs::write(&f1, content).unwrap();
        fs::write(&f2, content).unwrap();

        let size = content.len() as u64;
        let files = vec![make_scanned(f1, size), make_scanned(f2, size)];

        let result = find_duplicates(&files, &ProgressBar::hidden());
        assert_eq!(result.duplicate_groups.len(), 1);
        assert_eq!(result.duplicate_groups[0].files.len(), 2);
    }

    /// A body several buffers long, with a short final read — where an
    /// off-by-one in a streaming loop shows up.
    fn multi_buffer_body() -> Vec<u8> {
        (0..300_000u32).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn test_hash_reader_agrees_with_a_one_shot_hash() {
        let body = multi_buffer_body();
        let digest = hash_reader(&mut body.as_slice()).unwrap();
        assert_eq!(digest, blake3::hash(&body).to_hex().to_string());
    }

    #[test]
    fn test_hash_reader_handles_an_empty_stream() {
        let digest = hash_reader(&mut [].as_slice()).unwrap();
        assert_eq!(digest, blake3::hash(b"").to_hex().to_string());
    }

    #[test]
    fn test_full_hash_reads_the_whole_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("photo.jpg");
        let body = multi_buffer_body();
        fs::write(&path, &body).unwrap();

        assert_eq!(
            full_hash(&path).unwrap(),
            blake3::hash(&body).to_hex().to_string()
        );
    }

    /// The copy reproduces the bytes exactly and reports the digest of what it
    /// read — which is what makes the caller's comparison meaningful.
    #[test]
    fn test_copy_hashing_copies_the_bytes_and_reports_the_source_digest() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("copy.jpg");
        let body = multi_buffer_body();
        fs::write(&src, &body).unwrap();

        let digest = copy_hashing(&src, &dst).unwrap();

        assert_eq!(digest, blake3::hash(&body).to_hex().to_string());
        assert_eq!(fs::read(&dst).unwrap(), body);
        assert_eq!(
            full_hash(&dst).unwrap(),
            digest,
            "the file that landed must hash to the digest that was reported"
        );
    }

    /// `O_CREAT | O_EXCL`: the copy never writes over a file that is already
    /// there, even when the caller hands it a path it thinks is free.
    #[test]
    fn test_copy_hashing_refuses_an_existing_destination() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("taken.jpg");
        fs::write(&src, b"NEW").unwrap();
        fs::write(&dst, b"PRE-EXISTING").unwrap();

        assert!(copy_hashing(&src, &dst).is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"PRE-EXISTING");
    }

    /// A file that shrank after the scan recorded its size must still hash.
    ///
    /// The old `partial_hash` sized a `read_exact` from the scan-time figure
    /// and failed with "failed to fill whole buffer", taking the whole dedup
    /// pass down with it. A photo library being written to during a long run
    /// is ordinary — a camera import, a sync client, the user themselves.
    #[test]
    fn test_partial_hash_survives_a_file_that_shrank_since_the_scan() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shrinking.jpg");
        fs::write(&path, multi_buffer_body()).unwrap();

        // The scan saw 300 000 bytes; by the time we read it there are 4.
        fs::write(&path, b"tiny").unwrap();

        assert_eq!(
            partial_hash(&path).unwrap(),
            blake3::hash(b"tiny").to_hex().to_string(),
            "the digest must describe the bytes that were actually there"
        );
    }

    /// For a file large enough to have a distinct tail, the partial hash is
    /// head-then-tail over the file's *current* length.
    ///
    /// Pins the digest against an independent one-shot computation so the
    /// rewrite from `read_exact` to a tolerant read cannot have quietly
    /// changed which bytes are hashed — that would repartition every existing
    /// user's library on upgrade.
    #[test]
    fn test_partial_hash_is_head_then_tail_of_the_current_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("big.jpg");
        let size = u32::try_from(PARTIAL_HASH_BYTES).unwrap() * 3;
        let body: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let size = usize::try_from(size).unwrap();
        fs::write(&path, &body).unwrap();

        let head = usize::try_from(PARTIAL_HASH_BYTES).unwrap();
        let mut expected = blake3::Hasher::new();
        expected.update(&body[..head]);
        expected.update(&body[size - head..]);

        assert_eq!(
            partial_hash(&path).unwrap(),
            expected.finalize().to_hex().to_string()
        );
    }

    /// A file the dedup pass cannot read is dropped from the analysis and
    /// counted — it does not abort the run, and it does not silently vanish.
    ///
    /// The two fixtures share a size so the unreadable one actually reaches
    /// the hashing phase; a file with a unique size never gets hashed at all.
    ///
    /// Skips itself with a printed reason where permission bits do not deny
    /// reads (running as root, as some CI containers do).
    #[cfg(unix)]
    #[test]
    fn test_an_unreadable_file_is_excluded_from_dedup_not_fatal() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let readable = tmp.path().join("readable.jpg");
        let locked = tmp.path().join("locked.jpg");
        fs::write(&readable, b"same-length-aaaa").unwrap();
        fs::write(&locked, b"same-length-bbbb").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let denied = File::open(&locked).is_err();
        let files = vec![
            make_scanned(readable.clone(), 16),
            make_scanned(locked.clone(), 16),
        ];
        let result = find_duplicates(&files, &ProgressBar::hidden());

        // Restore before asserting, or `TempDir` cannot clean up after a panic.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

        if !denied {
            eprintln!(
                "SKIPPED test_an_unreadable_file_is_excluded_from_dedup_not_fatal: a 0o000 \
                 file was still readable, so this process ignores permission bits (running \
                 as root?)"
            );
            return;
        }

        assert_eq!(result.skipped, 1, "the unreadable file must be counted");
        assert_eq!(
            result
                .unique
                .iter()
                .map(|u| &u.file.path)
                .collect::<Vec<_>>(),
            vec![&readable],
            "the readable file must survive, and the unreadable one must not be \
             offered to the organiser as though its content were known"
        );
        assert!(result.duplicate_groups.is_empty());
    }

    /// The same property one phase later: `group_by_full_hash` skips rather
    /// than aborting. Asserted directly because a file that fails to open in
    /// phase 3 would already have failed in phase 2, so the cascade cannot
    /// reach this branch on its own.
    #[cfg(unix)]
    #[test]
    fn test_group_by_full_hash_skips_what_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let readable = tmp.path().join("readable.jpg");
        let locked = tmp.path().join("locked.jpg");
        fs::write(&readable, b"same-length-aaaa").unwrap();
        fs::write(&locked, b"same-length-bbbb").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let denied = File::open(&locked).is_err();
        let a = make_scanned(readable, 16);
        let b = make_scanned(locked.clone(), 16);
        let grouped = group_by_full_hash(&[&a, &b]);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

        if !denied {
            eprintln!(
                "SKIPPED test_group_by_full_hash_skips_what_it_cannot_read: a 0o000 file was \
                 still readable, so this process ignores permission bits (running as root?)"
            );
            return;
        }

        assert_eq!(grouped.skipped, 1);
        assert_eq!(grouped.groups.len(), 1);
        assert_eq!(grouped.groups.values().next().unwrap().len(), 1);
    }

    /// A clean run reports nothing skipped — the counter is a signal, so it
    /// has to be silent when there is nothing to signal.
    #[test]
    fn test_nothing_is_skipped_when_everything_reads() {
        let tmp = TempDir::new().unwrap();
        let content = b"identical content for both files";
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        fs::write(&f1, content).unwrap();
        fs::write(&f2, content).unwrap();

        let size = content.len() as u64;
        let files = vec![make_scanned(f1, size), make_scanned(f2, size)];

        let result = find_duplicates(&files, &ProgressBar::hidden());
        assert_eq!(result.skipped, 0);
        assert_eq!(result.duplicate_groups.len(), 1);
    }

    #[test]
    fn test_same_size_different_content() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        // Same length, different content
        fs::write(&f1, b"aaaa1234").unwrap();
        fs::write(&f2, b"bbbb5678").unwrap();

        let files = vec![make_scanned(f1, 8), make_scanned(f2, 8)];

        let result = find_duplicates(&files, &ProgressBar::hidden());
        assert_eq!(result.unique.len(), 2);
        assert!(result.duplicate_groups.is_empty());
    }

    /// Write `body` to `tmp/relative`, creating the directories on the way, and
    /// return the `ScannedFile` the scan would have produced for it.
    fn plant(tmp: &TempDir, relative: &str, body: &[u8]) -> ScannedFile {
        let path = tmp.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
        make_scanned(path, body.len() as u64)
    }

    /// A tree with two duplicate groups, a size-collision pair that is not
    /// duplicated, and a file unique by size — laid out so that the correct
    /// answers are *not* the ones any accidental ordering would give.
    ///
    /// `zzz.jpg` sits at the top of the tree and sorts lexicographically last of
    /// its group, so a plain path sort would pick `aaa/deep/one.jpg` as the
    /// original and a `HashMap` would pick whichever it felt like. The depth
    /// rule picks `zzz.jpg`, and only the depth rule does.
    fn deterministic_fixture(tmp: &TempDir) -> Vec<ScannedFile> {
        // 40 KB apiece: over one buffer, under the 64 KB partial-hash window,
        // so both hashing phases actually run.
        let a: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..40_000u32).map(|i| (i % 241) as u8).collect();

        vec![
            // Group A — three copies at three depths.
            plant(tmp, "aaa/deep/one.jpg", &a),
            plant(tmp, "zzz.jpg", &a),
            plant(tmp, "bbb/two.jpg", &a),
            // Group B — two copies at the same depth, so the tie-break decides.
            plant(tmp, "ccc/bravo.jpg", &b),
            plant(tmp, "ccc/alpha.jpg", &b),
            // Same length as group A, different bytes: reaches phase 3 and is
            // separated there.
            plant(tmp, "ddd/impostor.jpg", &{
                let mut v = a.clone();
                v[39_999] ^= 0xFF;
                v
            }),
            // Unique by size — eliminated in phase 1.
            plant(tmp, "eee/lonely.jpg", b"short"),
        ]
    }

    /// Everything about a result that a caller could observe, as one string.
    fn render(result: &DedupResult) -> String {
        let mut out = format!("skipped={}\n", result.skipped);
        for group in &result.duplicate_groups {
            let _ = writeln!(out, "group {} size={}", group.hash, group.size);
            for path in &group.files {
                let _ = writeln!(out, "  {}", path.display());
            }
        }
        for unique in &result.unique {
            let _ = writeln!(
                out,
                "unique {} hash={}",
                unique.file.path.display(),
                unique.known_hash.as_deref().unwrap_or("-")
            );
        }
        out
    }

    /// The same tree must produce byte-identical output every time.
    ///
    /// Each `HashMap` in a process gets its own `RandomState` seed, so the
    /// repeats below really do re-roll the iteration order the cascade used to
    /// take its answers from — this fails on the unsorted implementation.
    #[test]
    fn test_find_duplicates_is_byte_identical_across_repeated_runs() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);

        let first = render(&find_duplicates(&files, &ProgressBar::hidden()));
        for run in 1..20 {
            assert_eq!(
                render(&find_duplicates(&files, &ProgressBar::hidden())),
                first,
                "run {run} disagreed with run 0"
            );
        }
    }

    /// Reordering the input — which is all a different filesystem walk order
    /// is — must not change the answer either.
    #[test]
    fn test_find_duplicates_does_not_depend_on_the_order_it_is_handed_files() {
        let tmp = TempDir::new().unwrap();
        let mut files = deterministic_fixture(&tmp);

        let forwards = render(&find_duplicates(&files, &ProgressBar::hidden()));
        files.reverse();
        let backwards = render(&find_duplicates(&files, &ProgressBar::hidden()));

        assert_eq!(forwards, backwards);
    }

    /// The retained original is the shallowest path, *not* the lexicographically
    /// smallest one — and the rest of the group follows the same rule.
    #[test]
    fn test_the_retained_original_is_the_shallowest_path() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = find_duplicates(&files, &ProgressBar::hidden());

        let group = result
            .duplicate_groups
            .iter()
            .find(|g| g.files.len() == 3)
            .expect("the three-copy group must be detected");

        assert_eq!(
            group.files,
            vec![
                tmp.path().join("zzz.jpg"),
                tmp.path().join("bbb/two.jpg"),
                tmp.path().join("aaa/deep/one.jpg"),
            ],
            "shallowest first, then lexicographic — a plain path sort would have \
             kept aaa/deep/one.jpg"
        );
    }

    /// At equal depth the tie-break is lexicographic on the whole path.
    #[test]
    fn test_the_retained_original_breaks_a_depth_tie_lexicographically() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = find_duplicates(&files, &ProgressBar::hidden());

        let group = result
            .duplicate_groups
            .iter()
            .find(|g| g.files.len() == 2)
            .expect("the two-copy group must be detected");

        assert_eq!(
            group.files,
            vec![
                tmp.path().join("ccc/alpha.jpg"),
                tmp.path().join("ccc/bravo.jpg"),
            ],
            "same depth, so the smaller path is the original"
        );
    }

    /// The original a group keeps is also the copy handed on as unique, with the
    /// group's digest — the two lists must not disagree about which file that is.
    #[test]
    fn test_the_retained_original_is_the_file_offered_to_the_organiser() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = find_duplicates(&files, &ProgressBar::hidden());

        for group in &result.duplicate_groups {
            let original = &group.files[0];
            let carried = result
                .unique
                .iter()
                .find(|u| &u.file.path == original)
                .unwrap_or_else(|| panic!("{} must survive as unique", original.display()));
            assert_eq!(carried.known_hash.as_ref(), Some(&group.hash));

            for duplicate in group.files.iter().skip(1) {
                assert!(
                    !result.unique.iter().any(|u| &u.file.path == duplicate),
                    "{} is a duplicate and must not also be offered as unique",
                    duplicate.display()
                );
            }
        }
    }

    /// Groups come back ordered by their content hash, so a report or a
    /// `duplicates/NNN/` numbering is stable between runs.
    #[test]
    fn test_duplicate_groups_are_ordered_by_hash() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = find_duplicates(&files, &ProgressBar::hidden());

        assert_eq!(result.duplicate_groups.len(), 2);
        let hashes: Vec<&str> = result
            .duplicate_groups
            .iter()
            .map(|g| g.hash.as_str())
            .collect();
        let mut sorted = hashes.clone();
        sorted.sort_unstable();
        assert_eq!(hashes, sorted);
    }

    /// `unique` is the order the organiser plans moves in, so it is sorted by
    /// the same rule rather than by whatever the last `HashMap` said.
    #[test]
    fn test_unique_files_come_back_in_path_order() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = find_duplicates(&files, &ProgressBar::hidden());

        let paths: Vec<PathBuf> = result.unique.iter().map(|u| u.file.path.clone()).collect();
        let mut sorted = paths.clone();
        sorted.sort_by(|a, b| by_depth_then_path(a, b));
        assert_eq!(paths, sorted);
    }
}
