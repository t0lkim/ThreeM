use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::hash::Hash;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::thread::available_parallelism;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
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

// =====================================================================
// How many files may be read at once
// =====================================================================

/// The most threads the *default* will choose, however many cores it finds.
///
/// The bound is about the storage device, not the CPU. Hashing is fast enough
/// that the cascade spends most of its time waiting on reads, so the thread
/// count is really a queue depth — and queue depth stops helping long before
/// core count does. A solid-state device saturates in the single digits; a spinning
/// disk has one head, so every concurrent reader past the first turns a
/// sequential read into a seek storm and makes the run *slower*; a network share
/// is worse again, because each thread is a separate round trip.
///
/// Eight, then, rather than `available_parallelism()` unbounded: a 64-core
/// workstation firing 64 concurrent reads at somebody's photo library is not
/// eight times better than eight, and on half the storage it could plausibly be
/// pointed at, it is worse. Anyone who knows their device wants more says so
/// with `--threads`.
pub const DEFAULT_HASH_THREAD_CEILING: usize = 8;

/// The most threads a *setting* may ask for.
///
/// Far above any real device's useful queue depth, and far below the point at
/// which building the pool becomes the run's biggest problem. It exists so that
/// `--threads 100000` is refused by name rather than discovered as a failure to
/// spawn the ten-thousandth thread, halfway into a library.
pub const MAX_HASH_THREADS: usize = 1024;

/// How many threads the cascade uses when nothing said otherwise.
///
/// [`DEFAULT_HASH_THREAD_CEILING`] applied to whatever this machine reports.
/// A machine that will not report its parallelism gets one thread — the
/// conservative answer, and the one that cannot thrash anything.
pub fn default_hash_threads() -> NonZeroUsize {
    let ceiling = NonZeroUsize::new(DEFAULT_HASH_THREAD_CEILING).unwrap_or(NonZeroUsize::MIN);
    available_parallelism().map_or(NonZeroUsize::MIN, |cores| cores.min(ceiling))
}

/// The bounded worker pool the hashing phases run on.
///
/// **A pool of its own, never the global one.** `rayon`'s global pool is
/// process-wide and built once, on first use, from whatever asked for it first —
/// so configuring the hashing bound through it would mean a setting that could
/// only be applied before anything else touched `rayon`, could not be changed
/// afterwards, and could not be tested twice in one process with two different
/// values. Two tests in this module do exactly that. A dedicated pool makes the
/// bound a value the caller holds rather than a global it hopes nobody else set.
///
/// It also scopes the bound to the thing it is a bound *on*: `hash_threads`
/// describes how hard to push the storage device during dedup, and nothing else
/// in the process inherits it.
#[derive(Debug)]
pub struct HashPool {
    pool: rayon::ThreadPool,
    threads: NonZeroUsize,
}

impl HashPool {
    /// Build a pool of exactly `threads` workers.
    ///
    /// [`NonZeroUsize`] rather than `usize` because `rayon` reads
    /// `num_threads(0)` as "use the default" — so a zero arriving from a config
    /// file would not be an error, it would be the bound silently not applying.
    /// The type makes that unrepresentable; [`crate::settings`] refuses the zero
    /// at the layer that read it, with a message naming the file.
    ///
    /// # Errors
    ///
    /// If the pool cannot be built — in practice, only if the threads cannot be
    /// spawned. A run that cannot build its pool has not read anything yet, and
    /// stopping there is the point.
    pub fn with_threads(threads: NonZeroUsize) -> Result<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.get())
            .thread_name(|index| format!("mmm-hash-{index}"))
            .build()
            .with_context(|| format!("building a hashing pool of {threads} threads"))?;
        Ok(Self { pool, threads })
    }

    /// A pool at this machine's default bound — see [`default_hash_threads`].
    ///
    /// # Errors
    ///
    /// As [`Self::with_threads`].
    pub fn automatic() -> Result<Self> {
        Self::with_threads(default_hash_threads())
    }

    /// How many threads this pool hashes on.
    pub fn threads(&self) -> NonZeroUsize {
        self.threads
    }
}

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
/// or thread completion order.
fn by_depth_then_path(a: &Path, b: &Path) -> Ordering {
    a.components()
        .count()
        .cmp(&b.components().count())
        .then_with(|| a.cmp(b))
}

/// Three-phase dedup cascade:
/// 1. Group by file size (free — metadata only, and serial)
/// 2. Partial BLAKE3 hash (first 64KB + last 64KB), hashed in parallel
/// 3. Full BLAKE3 hash (only for partial-hash matches), hashed in parallel
///
/// Infallible by construction. A file that cannot be read is dropped from the
/// analysis with a warning and counted in [`DedupResult::skipped`]; it never
/// takes the rest of the run down with it.
///
/// **Parallel in phases 2 and 3, and flat rather than group-at-a-time.** Both
/// hashing phases hand their whole candidate set to [`group_by_key`] at once
/// instead of walking one size group after another. That matters more than the
/// `par_iter` does: duplicate groups are typically pairs, so parallelising
/// *within* a group would cap the cascade at two-way concurrency no matter how
/// many cores were free. Phase 1 stays serial deliberately — it reads no file
/// content and measured 12 µs against phase 3's 94 ms, so there is nothing
/// there to win.
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
///
/// **`pool` bounds the concurrency and nothing else.** The answer does not
/// depend on how wide it is: a one-thread pool and an eight-thread pool return
/// byte-identical results over the same tree, which is what
/// `test_one_thread_gives_the_same_answer_as_the_parallel_default` pins. What it
/// changes is how many files are open at once, which is a question about the
/// storage device rather than about duplicate detection — see [`HashPool`].
pub fn find_duplicates(
    files: &[ScannedFile],
    progress: &ProgressBar,
    pool: &HashPool,
) -> DedupResult {
    progress.set_message("Phase 1: grouping by file size");
    let size_groups = group_by_size(files);

    // Files with unique sizes are immediately unique
    let mut unique: Vec<UniqueFile> = Vec::new();
    let mut phase2_candidates: Vec<&ScannedFile> = Vec::new();
    let mut candidate_groups = 0usize;

    for group in size_groups.values() {
        if group.len() == 1 {
            unique.push(UniqueFile {
                file: group[0].clone(),
                known_hash: None,
            });
        } else {
            candidate_groups += 1;
            phase2_candidates.extend(group.iter());
        }
    }

    debug!(
        unique = unique.len(),
        candidate_groups,
        candidate_files = phase2_candidates.len(),
        "phase 1 complete"
    );

    // Phase 2: Partial hash
    progress.set_message("Phase 2: partial hashing size-matched files");
    // Keyed by *size and* digest, not digest alone. A partial hash is only the
    // head (and, past 128 KB, the tail), so two files of different lengths can
    // legitimately share one — a truncated download and the whole file it came
    // from, say. Phase 1 already separated them and phase 2 must not put them
    // back together, or every such pair buys a full read it does not need.
    let (partial_groups, mut skipped) = group_by_key(
        &phase2_candidates,
        |file| partial_hash(&file.path),
        |file, digest| (file.size, digest),
        pool,
    );
    progress.inc(phase2_candidates.len() as u64);

    let mut phase3_candidates: Vec<&ScannedFile> = Vec::new();
    for pgroup in partial_groups.into_values() {
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
            phase3_candidates.extend(pgroup);
        }
    }

    debug!(phase3_files = phase3_candidates.len(), "phase 2 complete");

    // Phase 3: Full hash
    //
    // Keyed by the digest alone, across what were separate size groups, which
    // merges nothing: two files with the same full hash have the same content
    // and therefore the same size, so they were in one size group already.
    progress.set_message("Phase 3: full hashing confirmed candidates");
    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();

    let full = group_by_full_hash(&phase3_candidates, pool);
    skipped += full.skipped;
    progress.inc(phase3_candidates.len() as u64);

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
pub fn group_by_partial_hash<'a>(files: &[&'a ScannedFile], pool: &HashPool) -> HashGroups<'a> {
    let (groups, skipped) = group_by_key(
        files,
        |file| partial_hash(&file.path),
        |_, digest| digest,
        pool,
    );
    HashGroups { groups, skipped }
}

/// Group files by a full-content BLAKE3 hash.
///
/// A file that cannot be opened or read to completion is left out of the
/// returned groups and counted in [`HashGroups::skipped`], with a warning
/// naming it.
// exposed for integration tests
pub fn group_by_full_hash<'a>(files: &[&'a ScannedFile], pool: &HashPool) -> HashGroups<'a> {
    let (groups, skipped) = group_by_key(
        files,
        |file| full_hash(&file.path),
        |_, digest| digest,
        pool,
    );
    HashGroups { groups, skipped }
}

/// The shared body of both hashing passes: hash every file in parallel, bucket
/// each one under `key`, and drop — loudly — the ones that would not read.
///
/// One implementation rather than two, because the interesting behaviour here
/// is the skip, and two copies of it are two chances for one of them to go
/// back to `?` unnoticed. `key` exists only so phase 2 can bucket by size *and*
/// digest while phase 3 buckets by digest alone.
///
/// # Two halves, and why the split is where it is
///
/// The hashing is parallel; the bucketing is not. Rayon's `map(...).collect()`
/// into a `Vec` yields results in **input order** regardless of which thread
/// finished first, so the fold below sees the same sequence on every run and
/// the skip count is an ordinary local variable rather than an
/// [`AtomicUsize`](std::sync::atomic::AtomicUsize) — no counter is shared
/// across threads at all, which is a stronger guarantee than incrementing one
/// atomically, and it is why the warnings come out in a reproducible order too.
/// Bucketing is a few hash-map inserts against a phase that reads gigabytes, so
/// leaving it serial costs nothing measurable and removes the need for a
/// mutex-wrapped map.
///
/// The per-file [`Result`] is the resilience contract from Phase 02 carried
/// intact across the thread boundary: an unreadable file becomes an `Err` in
/// the vector, not a `?` that would abandon every other file's completed work.
///
/// The parallel half runs inside `pool` rather than on `rayon`'s global pool, so
/// how many files this opens at once is the caller's decision — see
/// [`HashPool`].
fn group_by_key<'a, K, H, F>(
    files: &[&'a ScannedFile],
    hash: H,
    key: F,
    pool: &HashPool,
) -> (HashMap<K, Vec<&'a ScannedFile>>, usize)
where
    K: Eq + Hash,
    H: Fn(&ScannedFile) -> Result<String> + Sync,
    F: Fn(&ScannedFile, String) -> K,
{
    let digests: Vec<Result<String>> = pool
        .pool
        .install(|| files.par_iter().map(|file| hash(file)).collect());

    let mut groups: HashMap<K, Vec<&'a ScannedFile>> = HashMap::new();
    let mut skipped = 0;

    for (file, digest) in files.iter().zip(digests) {
        match digest {
            Ok(digest) => groups.entry(key(file, digest)).or_default().push(file),
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

    (groups, skipped)
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

    /// A pool at this machine's default bound — what a real run gets.
    fn pool() -> HashPool {
        HashPool::automatic().expect("a default hashing pool must build")
    }

    /// The cascade as a run performs it: default bound, no visible bar.
    ///
    /// Each call builds its own pool rather than sharing one, which costs a
    /// handful of thread spawns and buys the repeated-run tests a fresh
    /// scheduler each time — the answer has to be the same across pools, not
    /// merely stable within one.
    fn dedup(files: &[ScannedFile]) -> DedupResult {
        find_duplicates(files, &ProgressBar::hidden(), &pool())
    }

    #[test]
    fn test_unique_files_by_size() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        fs::write(&f1, b"short").unwrap();
        fs::write(&f2, b"much longer content here").unwrap();

        let files = vec![make_scanned(f1, 5), make_scanned(f2, 24)];

        let result = dedup(&files);
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

        let result = dedup(&files);
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
        let result = dedup(&files);

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
        let grouped = group_by_full_hash(&[&a, &b], &pool());

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

        let result = dedup(&files);
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

        let result = dedup(&files);
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

        let first = render(&dedup(&files));
        for run in 1..20 {
            assert_eq!(
                render(&dedup(&files)),
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

        let forwards = render(&dedup(&files));
        files.reverse();
        let backwards = render(&dedup(&files));

        assert_eq!(forwards, backwards);
    }

    /// The retained original is the shallowest path, *not* the lexicographically
    /// smallest one — and the rest of the group follows the same rule.
    #[test]
    fn test_the_retained_original_is_the_shallowest_path() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = dedup(&files);

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
        let result = dedup(&files);

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
        let result = dedup(&files);

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
        let result = dedup(&files);

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

    /// A tree wide enough that the hashing phases really do span threads:
    /// `groups` duplicate groups of three copies each, every file the same
    /// 40 KB length so all of them survive phase 1 into one enormous phase-2
    /// candidate set.
    ///
    /// The narrow fixtures above have four or five files in them, which a
    /// work-stealing pool may well run on one thread — they prove the sort
    /// rules, not the concurrency. This one is here so that "the same tree
    /// gives the same answer" is a claim about parallel execution.
    fn wide_fixture(tmp: &TempDir, groups: u32) -> Vec<ScannedFile> {
        let mut files = Vec::with_capacity(groups as usize * 3);
        for g in 0..groups {
            // Distinct content per group, identical length across all groups.
            let body: Vec<u8> = (0..40_000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(g) % 251) as u8)
                .collect();
            for copy in 0..3 {
                files.push(plant(tmp, &format!("g{g:03}/c{copy}/photo.jpg"), &body));
            }
        }
        files
    }

    /// The determinism guarantee, restated against a corpus large enough to be
    /// hashed on several threads at once: completion order must not reach the
    /// output.
    #[test]
    fn test_parallel_hashing_gives_byte_identical_output_across_runs() {
        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 32);

        let first = render(&dedup(&files));
        for run in 1..8 {
            assert_eq!(
                render(&dedup(&files)),
                first,
                "run {run} disagreed with run 0 — thread completion order is leaking"
            );
        }

        let result = dedup(&files);
        assert_eq!(result.duplicate_groups.len(), 32);
        assert!(result.duplicate_groups.iter().all(|g| g.files.len() == 3));
        assert_eq!(result.skipped, 0);
    }

    /// Many unreadable files, spread across many groups, hashed concurrently:
    /// every one of them is counted exactly once and excluded from the plan,
    /// and the readable copies around them still form their groups.
    ///
    /// The count is the point. A skip counter that lost increments under
    /// concurrency would report a clean run over a library that was only
    /// partly processed — and it would do so intermittently, which is worse.
    #[cfg(unix)]
    #[test]
    fn test_concurrent_read_failures_are_each_counted_exactly_once() {
        use std::os::unix::fs::PermissionsExt as _;

        const LOCKED: usize = 12;

        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 32);

        // One copy locked out of each of the first twelve groups.
        let locked: Vec<PathBuf> = (0..LOCKED)
            .map(|g| tmp.path().join(format!("g{g:03}/c0/photo.jpg")))
            .collect();
        for path in &locked {
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let denied = File::open(&locked[0]).is_err();
        let result = dedup(&files);

        for path in &locked {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }

        if !denied {
            eprintln!(
                "SKIPPED test_concurrent_read_failures_are_each_counted_exactly_once: a 0o000 \
                 file was still readable, so this process ignores permission bits (running \
                 as root?)"
            );
            return;
        }

        assert_eq!(
            result.skipped, LOCKED,
            "every unreadable file must be counted once — no more, no less"
        );
        for path in &locked {
            assert!(
                !result.unique.iter().any(|u| &u.file.path == path),
                "{} was never read, so it must not be offered as unique",
                path.display()
            );
            assert!(
                !result
                    .duplicate_groups
                    .iter()
                    .any(|g| g.files.contains(path)),
                "{} was never read, so it cannot be known to duplicate anything",
                path.display()
            );
        }

        assert_eq!(result.duplicate_groups.len(), 32);
        let sizes: Vec<usize> = result
            .duplicate_groups
            .iter()
            .map(|g| g.files.len())
            .collect();
        assert_eq!(
            sizes.iter().filter(|&&n| n == 2).count(),
            LOCKED,
            "the twelve groups that lost a copy must come back as pairs"
        );
        assert_eq!(sizes.iter().filter(|&&n| n == 3).count(), 32 - LOCKED);
    }

    /// Phase 2 buckets by size *and* digest, so flattening the candidate set
    /// across size groups cannot undo phase 1's work.
    ///
    /// Both fixtures below sit under 128 KB, so `partial_hash` reads only their
    /// first 64 KB — which is byte-identical between the two sizes. Keyed by
    /// the digest alone they would land in one bucket of two, be promoted to
    /// phase 3, and each buy a full read to discover what their lengths already
    /// said. Keyed by `(size, digest)` each is a bucket of one and neither is
    /// opened again, which is what `known_hash: None` records here.
    #[test]
    fn test_phase_2_does_not_regroup_files_phase_1_separated_by_size() {
        let tmp = TempDir::new().unwrap();

        let head: Vec<u8> = (0..PARTIAL_HASH_BYTES).map(|i| (i % 251) as u8).collect();
        let shared_head = |extra: usize, fill: u8| {
            let mut body = head.clone();
            body.extend(std::iter::repeat_n(fill, extra));
            body
        };
        // A different first byte is enough to give a different partial digest.
        let other_head = |extra: usize| {
            let mut body = vec![0xAAu8; head.len()];
            body.extend(std::iter::repeat_n(0xBBu8, extra));
            body
        };

        let files = vec![
            // 70 000 bytes: one file sharing the head, one not — so this size
            // group splits into two buckets of one.
            plant(&tmp, "small-shared.jpg", &shared_head(5_536, 0x11)),
            plant(&tmp, "small-other.jpg", &other_head(5_536)),
            // 80 000 bytes: same shape, and its shared-head file has exactly
            // the same partial digest as `small-shared.jpg`.
            plant(&tmp, "large-shared.jpg", &shared_head(15_536, 0x22)),
            plant(&tmp, "large-other.jpg", &other_head(15_536)),
        ];

        assert_eq!(
            partial_hash(&files[0].path).unwrap(),
            partial_hash(&files[2].path).unwrap(),
            "the fixture is pointless unless the two sizes really do share a partial digest"
        );

        let result = dedup(&files);

        assert!(result.duplicate_groups.is_empty());
        assert_eq!(result.unique.len(), 4);
        assert!(
            result.unique.iter().all(|u| u.known_hash.is_none()),
            "every file was retired by the partial hash, so none of them should have been \
             read end to end"
        );
    }

    // -----------------------------------------------------------------
    // The bound on how many files are open at once
    // -----------------------------------------------------------------

    /// `--threads 1` must return the same answer as the parallel default.
    ///
    /// This is the property that makes the bound safe to turn down: somebody on
    /// a spinning disk or a network share who drops to one thread is trading
    /// speed for kindness to their storage, and must not also be trading which
    /// copy of their photograph gets kept. Byte-identical over the *whole*
    /// result — groups, their order, the retained originals, the unique list and
    /// the skip count — not merely "the same number of duplicates".
    ///
    /// Run on the wide fixture rather than the five-file one so the parallel
    /// side really is parallel: 96 files across 32 groups spans the pool.
    #[test]
    fn test_one_thread_gives_the_same_answer_as_the_parallel_default() {
        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 32);

        let serial = HashPool::with_threads(NonZeroUsize::MIN).unwrap();
        let parallel = pool();

        assert_eq!(
            render(&find_duplicates(&files, &ProgressBar::hidden(), &serial)),
            render(&find_duplicates(&files, &ProgressBar::hidden(), &parallel)),
            "the thread count bounds the concurrency and must not reach the answer"
        );
    }

    /// The default is bounded by the ceiling rather than by the core count
    /// alone — the whole point of the setting existing.
    #[test]
    fn test_the_default_thread_count_respects_the_ceiling() {
        let threads = default_hash_threads().get();
        assert!(
            threads <= DEFAULT_HASH_THREAD_CEILING,
            "{threads} threads is past the {DEFAULT_HASH_THREAD_CEILING}-thread ceiling — an \
             unbounded default is what this setting exists to prevent"
        );
    }

    /// The bound is real: a pool of N never runs work on more than N workers.
    ///
    /// Asserted by watching which worker indices actually pick tasks up, over
    /// enough deliberately slow tasks that a wider pool would certainly spread
    /// them. `<= n` is the contract — a run that thrashes the disk is one that
    /// opened *more* files at once than it was told to.
    ///
    /// The second half is the negative control. `<= 2` alone would also pass on
    /// an implementation that silently ran everything on one thread, so on a
    /// machine with cores to spare the default pool is checked to use more than
    /// one — which is what proves the first assertion is measuring a bound and
    /// not an accident.
    #[test]
    fn test_a_pool_runs_on_no_more_workers_than_it_was_built_with() {
        use std::collections::HashSet;
        use std::sync::Mutex;
        use std::time::Duration;

        /// The worker indices that picked up any of `TASKS` slow tasks.
        fn workers_used(pool: &HashPool) -> HashSet<usize> {
            const TASKS: u32 = 512;
            let seen = Mutex::new(HashSet::new());
            pool.pool.install(|| {
                (0..TASKS).into_par_iter().for_each(|_| {
                    if let Some(index) = rayon::current_thread_index() {
                        seen.lock()
                            .expect("the index set is never poisoned")
                            .insert(index);
                    }
                    std::thread::sleep(Duration::from_micros(100));
                });
            });
            seen.into_inner().expect("the index set is never poisoned")
        }

        let two = HashPool::with_threads(NonZeroUsize::new(2).unwrap()).unwrap();
        assert_eq!(two.threads().get(), 2);
        let used = workers_used(&two);
        assert!(
            used.len() <= 2,
            "a two-thread pool ran work on {} workers",
            used.len()
        );

        let one = HashPool::with_threads(NonZeroUsize::MIN).unwrap();
        assert_eq!(
            workers_used(&one).len(),
            1,
            "a one-thread pool must open one file at a time"
        );

        if default_hash_threads().get() > 1 {
            assert!(
                workers_used(&pool()).len() > 1,
                "this machine has cores to spare, so the default pool spreading over one worker \
                 would mean the bound is not what is being measured above"
            );
        }
    }

    /// `unique` is the order the organiser plans moves in, so it is sorted by
    /// the same rule rather than by whatever the last `HashMap` said.
    #[test]
    fn test_unique_files_come_back_in_path_order() {
        let tmp = TempDir::new().unwrap();
        let files = deterministic_fixture(&tmp);
        let result = dedup(&files);

        let paths: Vec<PathBuf> = result.unique.iter().map(|u| u.file.path.clone()).collect();
        let mut sorted = paths.clone();
        sorted.sort_by(|a, b| by_depth_then_path(a, b));
        assert_eq!(paths, sorted);
    }
}
