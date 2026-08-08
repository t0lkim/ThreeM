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
// How much of a file the partial hash actually reads
// =====================================================================

/// Which bytes of a file of a given length [`partial_hash`] reads.
///
/// The rule was two inline comparisons and a comment; it is a named function
/// because the middle case is a genuine surprise and the promotion in
/// [`find_duplicates`] now turns on getting it right.
///
/// | size            | hashed                     | why                                                        |
/// |-----------------|----------------------------|------------------------------------------------------------|
/// | `0 ..= 64 KB`   | the whole file             | the head read reaches the end, so the digest *is* the full hash |
/// | `64 KB+1 ..= 128 KB` | the first 64 KB only  | a tail read here would overlap the head — see below         |
/// | `128 KB+1 ..`   | the first and last 64 KB   | two disjoint windows                                        |
///
/// **The middle band is the surprising one.** A 100 KB file is fingerprinted on
/// its first 64 KB and nothing else, so two files of exactly that length that
/// differ only past that point both reach phase 3 and are separated
/// there. That is the cascade working — a partial hash is a filter, and a filter
/// is allowed false positives — but it is the sort of fact that gets discovered
/// by someone reading a profile rather than the source, so it is written down
/// and pinned by `test_the_partial_hash_reads_what_the_coverage_rule_says`
/// rather than left implicit in a `>`.
///
/// The band exists because below 128 KB the last 64 KB and the first 64 KB
/// overlap: the tail read would re-read bytes already hashed and, for a 65 KB
/// file, would be almost entirely the head again. Hashing the head twice is
/// harmless but the *read* is not free, and this is the phase whose job is to
/// avoid reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialCoverage {
    /// Every byte — so the partial digest is also the full-content digest.
    WholeFile,
    /// The first [`PARTIAL_HASH_BYTES`] and no more.
    HeadOnly,
    /// The first and last [`PARTIAL_HASH_BYTES`], which do not overlap.
    HeadAndTail,
}

/// The coverage rule, applied to one length. See [`PartialCoverage`].
const fn partial_coverage(size: u64) -> PartialCoverage {
    if size <= PARTIAL_HASH_BYTES {
        PartialCoverage::WholeFile
    } else if size <= PARTIAL_HASH_BYTES * 2 {
        PartialCoverage::HeadOnly
    } else {
        PartialCoverage::HeadAndTail
    }
}

/// What [`partial_hash`] returns: the digest, and whether it happens to be the
/// whole file's digest as well.
///
/// The flag is the whole point of the type. Without it phase 3 re-reads every
/// small file to compute a digest phase 2 already holds — for a library of
/// thumbnails, sidecars and screenshots that is the entire second read of the
/// cascade, spent confirming what was already known.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PartialHash {
    digest: String,
    /// `true` only for [`PartialCoverage::WholeFile`] — see [`partial_hash`],
    /// which refuses to return at all if the bytes it read do not account for
    /// the length it was promised.
    covers_whole_file: bool,
}

// =====================================================================
// What the bar says it is doing
// =====================================================================

/// The three phase messages, named rather than written inline at the point they
/// are set, so the tests that pin the *ordering* between them are asserting
/// against the same strings the run displays.
const PHASE_1_MESSAGE: &str = "Phase 1: grouping by file size";
const PHASE_2_MESSAGE: &str = "Phase 2: partial hashing size-matched files";
const PHASE_3_MESSAGE: &str = "Phase 3: full hashing confirmed candidates";

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
/// reading every such file twice. A file small enough that its partial hash
/// read the whole file — see [`PartialCoverage::WholeFile`] — is free in the
/// same way and carries its digest too.
///
/// `None` is what is left: a file eliminated in phase 1, and one eliminated in
/// phase 2 whose partial hash was only a head or a head and a tail. Filling
/// those in would mean a full read of every file in the library, which is the
/// cost the cascade exists to avoid.
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
/// **Phase 3 does not re-read what phase 2 already read end to end.** A file of
/// 64 KB or less is covered entirely by its partial hash — the head read
/// reaches the end of the file — so that digest is its full digest, and a group
/// of such files is a confirmed duplicate group the moment phase 2 buckets it.
/// Sending it to phase 3 would read every one of those files a second time to
/// recompute a number already in hand. On a library of screenshots, thumbnails
/// and sidecars that second read is most of the cascade's IO. See
/// [`PartialCoverage`] for where the boundary is and why the 64 KB–128 KB band
/// above it is *not* covered.
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
///
/// # What the progress bar counts
///
/// **Reads, not files** — and this function owns the bar's position, length and
/// message for the duration of the call, whatever the caller set them to.
///
/// Counting files would make the bar lie about where the time goes. Phase 1 is
/// metadata only: it retires most of a library in microseconds (12 µs against
/// phase 3's 94 ms on the project's benchmark corpus). A bar that ticked once
/// per file would leap to 90% in the first millisecond and then spend the whole
/// run on the last tenth — technically counting something real, and useless for
/// answering "how much longer". The bar therefore starts counting at phase 2,
/// where the first byte of file content is read.
///
/// **The length is charged pessimistically and refunded, so it only ever
/// shrinks.** At the start of phase 2 the cascade knows it must partial-hash
/// `C₂` candidates and that *up to* `C₂` of them will go on to a full read, so
/// the length is `2 × C₂`. Phase 2 then rules most of them out — and settles
/// the small ones outright, which refunds their full read too — and the length
/// drops to `C₂ + C₃` for the phase-3 set it actually produced. Charging the
/// second read only when it was confirmed would be the same arithmetic run the
/// other way — the bar would reach 100% at the end of phase 2 and then fall
/// back for the phase that takes 92% of the time, which is the behaviour this
/// accounting exists to avoid. Position rises and length falls, so the fraction
/// never goes backwards.
///
/// **It ends exactly full.** On return `position() == length()`, without
/// [`ProgressBar::finish`] having to paper over a shortfall — a file that could
/// not be read ticks like any other, because the read was still attempted and
/// the operator still waited for it.
pub fn find_duplicates(
    files: &[ScannedFile],
    progress: &ProgressBar,
    pool: &HashPool,
) -> DedupResult {
    find_duplicates_with(files, progress, pool, full_hash)
}

/// The cascade, with phase 3's read supplied by the caller.
///
/// The seam exists for one reason: a file that phase 2 could read and phase 3
/// could not is a file deleted, truncated or locked *between* the two reads, and
/// no test can produce that race against a real filesystem. It is also the only
/// way [`DedupResult::skipped`] ever gains a count after phase 2 — so without a
/// seam here, the line that adds phase 3's skips to phase 2's is executed only
/// ever with a zero on one side, and "the operator is told how many files were
/// dropped from the plan" ships unproven.
///
/// A parameter rather than a `#[cfg(test)]` global, following the injected
/// `copy` on [`crate::organiser`]'s cross-volume move: the production path
/// passes [`full_hash`] and carries no test scaffolding at all, and two tests
/// injecting two different failures cannot reach each other. A thread-local
/// would not work here in any case — the read happens on `pool`'s workers, not
/// on the thread that armed it.
fn find_duplicates_with<F>(
    files: &[ScannedFile],
    progress: &ProgressBar,
    pool: &HashPool,
    full_hash_of: F,
) -> DedupResult
where
    F: Fn(&Path) -> Result<String> + Sync,
{
    progress.set_message(PHASE_1_MESSAGE);
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
    //
    // The bar starts counting here, because this is where the reading starts.
    // Two reads are charged per candidate — the partial hash it is about to get,
    // and the full hash it may go on to need — and the second is refunded below
    // for every file phase 2 rules out.
    let phase2_reads = phase2_candidates.len() as u64;
    progress.set_position(0);
    progress.set_length(phase2_reads.saturating_mul(2));
    progress.set_message(PHASE_2_MESSAGE);
    // Keyed by *size and* digest, not digest alone. A partial hash is only the
    // head (and, past 128 KB, the tail), so two files of different lengths can
    // legitimately share one — a truncated download and the whole file it came
    // from, say. Phase 1 already separated them and phase 2 must not put them
    // back together, or every such pair buys a full read it does not need.
    let (partial_groups, mut skipped) = group_by_key(
        &phase2_candidates,
        |file| partial_hash(&file.path, file.size),
        |file, partial| (file.size, partial),
        progress,
        pool,
    );

    let mut phase3_candidates: Vec<&ScannedFile> = Vec::new();
    // Groups the partial hash already settled: every member was small enough
    // that phase 2 read all of it, so the digest bucketing them *is* their full
    // hash and phase 3 would only read them again to be told so. Whole groups
    // rather than individual files, because the bucket key includes the size —
    // every member of a bucket has the same length, so coverage is uniform
    // across it by construction.
    let mut settled_by_phase_2: Vec<(String, Vec<&ScannedFile>)> = Vec::new();

    for ((_size, partial), pgroup) in partial_groups {
        if pgroup.len() == 1 {
            // Nothing else shares this fingerprint, so the file is unique. It
            // carries a digest only if the partial hash happened to read the
            // whole file — a head, or a head and a tail, is not the digest the
            // journal needs and must not be recorded as though it were.
            unique.push(UniqueFile {
                file: pgroup[0].clone(),
                known_hash: partial.covers_whole_file.then_some(partial.digest),
            });
        } else if partial.covers_whole_file {
            settled_by_phase_2.push((partial.digest, pgroup));
        } else {
            phase3_candidates.extend(pgroup);
        }
    }

    debug!(
        phase3_files = phase3_candidates.len(),
        settled_groups = settled_by_phase_2.len(),
        "phase 2 complete"
    );

    // Phase 3: Full hash
    //
    // Keyed by the digest alone, across what were separate size groups, which
    // merges nothing: two files with the same full hash have the same content
    // and therefore the same size, so they were in one size group already.
    //
    // Phase 2 has finished — `group_by_key` collects, and a collect is a barrier
    // — so the full-read charge can be settled against the set it actually
    // produced rather than the worst case. `phase3_candidates` is a subset of
    // the phase-2 candidates, so this only ever lowers the length, and the
    // message below cannot be displayed beside a position phase 2 is still
    // moving.
    progress.set_length(phase2_reads + phase3_candidates.len() as u64);
    progress.set_message(PHASE_3_MESSAGE);
    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();

    let full = {
        let (groups, skipped) = group_by_key(
            &phase3_candidates,
            |file| full_hash_of(&file.path),
            |_, digest| digest,
            progress,
            pool,
        );
        HashGroups { groups, skipped }
    };
    skipped += full.skipped;

    // The groups phase 2 settled join the ones phase 3 confirmed, on equal
    // terms: both are keyed by a full-content digest, so from here down there is
    // no such thing as a promoted group. Merged through `entry` rather than
    // inserted, so that two groups arriving with one digest would be one group
    // and not a lost one — it cannot happen (equal content means equal size, and
    // a file over 64 KB is never settled by phase 2) but a silent overwrite is
    // not the way to record that.
    let mut full_groups = full.groups;
    for (digest, members) in settled_by_phase_2 {
        full_groups.entry(digest).or_default().extend(members);
    }

    for (hash, mut fgroup) in full_groups {
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
    // The tie-break is unreachable and deliberately kept: the groups were the
    // keys of a `HashMap`, so no two of them share a digest and `then_with`
    // never runs. It states the total order the sort would need if that ever
    // stopped being true — which is why deleting its arm changes no test.
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

/// Group files by a partial BLAKE3 hash — see [`PartialCoverage`] for which
/// bytes that covers at which length.
///
/// A file that cannot be opened, seeked or read is left out of the returned
/// groups and counted in [`HashGroups::skipped`], with a warning naming it. So
/// is one that is no longer the length the scan recorded.
///
/// Buckets by digest alone, which is the right key for a caller holding one
/// size group. [`find_duplicates`] flattens every size group into one call and
/// so keys by `(size, digest)` instead — it does not go through here.
///
/// `progress` is ticked once per file as its read completes; pass
/// [`ProgressBar::hidden`] if there is nobody watching.
// exposed for integration tests
pub fn group_by_partial_hash<'a>(
    files: &[&'a ScannedFile],
    progress: &ProgressBar,
    pool: &HashPool,
) -> HashGroups<'a> {
    let (groups, skipped) = group_by_key(
        files,
        |file| partial_hash(&file.path, file.size),
        |_, partial: PartialHash| partial.digest,
        progress,
        pool,
    );
    HashGroups { groups, skipped }
}

/// Group files by a full-content BLAKE3 hash.
///
/// A file that cannot be opened or read to completion is left out of the
/// returned groups and counted in [`HashGroups::skipped`], with a warning
/// naming it.
///
/// `progress` is ticked once per file as its read completes; pass
/// [`ProgressBar::hidden`] if there is nobody watching.
// exposed for integration tests
pub fn group_by_full_hash<'a>(
    files: &[&'a ScannedFile],
    progress: &ProgressBar,
    pool: &HashPool,
) -> HashGroups<'a> {
    let (groups, skipped) = group_by_key(
        files,
        |file| full_hash(&file.path),
        |_, digest| digest,
        progress,
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
/// digest while phase 3 buckets by digest alone; `V` is generic for the same
/// reason, so that phase 2 can carry a [`PartialHash`] — digest plus whether it
/// covered the whole file — where phase 3 needs no more than the digest.
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
///
/// # Progress
///
/// `progress` is ticked from inside the parallel closure, once per file, the
/// moment that file's read finishes — not once per phase and not once per size
/// group. A bar that advanced per group would sit motionless through the
/// longest read in a library and then jump, which under parallelism is most of
/// the time: the groups are hashed all at once, so "this group is done" stopped
/// being a milestone the moment the work was flattened.
///
/// The tick is unconditional. A file that would not open still ticks, because
/// the bar measures reads attempted, not reads that succeeded — a run whose bar
/// stalled two files short of its total would have the operator looking for a
/// hang instead of reading the skip count.
///
/// [`ProgressBar`] is `Send + Sync` and its position is an atomic, so this needs
/// no synchronisation of its own; the caller owns the length and the message.
fn group_by_key<'a, K, V, H, F>(
    files: &[&'a ScannedFile],
    hash: H,
    key: F,
    progress: &ProgressBar,
    pool: &HashPool,
) -> (HashMap<K, Vec<&'a ScannedFile>>, usize)
where
    K: Eq + Hash,
    V: Send,
    H: Fn(&ScannedFile) -> Result<V> + Sync,
    F: Fn(&ScannedFile, V) -> K,
{
    let digests: Vec<Result<V>> = pool.pool.install(|| {
        files
            .par_iter()
            .map(|file| {
                let digest = hash(file);
                progress.inc(1);
                digest
            })
            .collect()
    });

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

/// Fingerprint a file from its first 64 KB and, past 128 KB, its last 64 KB —
/// see [`PartialCoverage`] for exactly which bytes, at which length.
///
/// # The file must still be the size the scan recorded
///
/// `scanned_size` is what [`crate::scanner`] saw, and a file that no longer
/// matches it is **skipped**, not hashed. That is a deliberate change of
/// answer, and the reason is that the scan-time size is not merely a hint here:
/// phase 1 partitioned by it, phase 2 buckets by it, [`DuplicateGroup::size`]
/// reports it and the journal records it as the evidence undo checks. Hashing
/// the new bytes and filing them under the old length would put a file into the
/// plan under facts that are no longer true — and a file whose length is moving
/// is usually one that is still being written, which is not a file to be
/// organising at all. Skipping it costs the operator one line in the summary;
/// the alternative costs them a plan they cannot trust.
///
/// The length is read from the open handle rather than re-`stat`ing the path,
/// so the check is against the file this function actually read. The two
/// short-read checks below close the rest of the window: between the `metadata`
/// call and the reads the file could still change, and a read that does not
/// return the bytes the length promised is how that shows up. What cannot be
/// closed is a change *after* the last read returns — no read of a live
/// filesystem can promise more than "this is what was there".
///
/// # Errors
///
/// If the file cannot be opened, `stat`ed, seeked or read — and if it is not
/// the length `scanned_size` says. Every one of those is a per-file skip in
/// [`group_by_key`], never a failed run.
fn partial_hash(path: &Path, scanned_size: u64) -> Result<PartialHash> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for partial hash", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("reading the length of {}", path.display()))?
        .len();

    if size != scanned_size {
        anyhow::bail!(
            "{} changed size since it was scanned — {scanned_size} bytes then, {size} now",
            path.display()
        );
    }

    let coverage = partial_coverage(size);
    let mut hasher = blake3::Hasher::new();
    let mut buf = Vec::new();

    // The head: the whole file, or the first 64 KB of it.
    read_up_to(&mut file, PARTIAL_HASH_BYTES, &mut buf)
        .with_context(|| format!("reading first bytes of {}", path.display()))?;
    let expected_head = size.min(PARTIAL_HASH_BYTES);
    if buf.len() as u64 != expected_head {
        anyhow::bail!(
            "{} changed while it was being read — {expected_head} bytes expected, {} read",
            path.display(),
            buf.len()
        );
    }
    hasher.update(&buf);

    // The tail, only where it does not overlap the head.
    if coverage == PartialCoverage::HeadAndTail {
        let tail_offset = i64::try_from(PARTIAL_HASH_BYTES)
            .context("partial-hash chunk size does not fit in i64")?;
        file.seek(SeekFrom::End(-tail_offset))
            .with_context(|| format!("seeking in {}", path.display()))?;
        read_up_to(&mut file, PARTIAL_HASH_BYTES, &mut buf)
            .with_context(|| format!("reading last bytes of {}", path.display()))?;
        if buf.len() as u64 != PARTIAL_HASH_BYTES {
            anyhow::bail!(
                "{} changed while it was being read — {PARTIAL_HASH_BYTES} trailing bytes \
                 expected, {} read",
                path.display(),
                buf.len()
            );
        }
        hasher.update(&buf);
    }

    Ok(PartialHash {
        digest: hasher.finalize().to_hex().to_string(),
        covers_whole_file: coverage == PartialCoverage::WholeFile,
    })
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

/// Create a progress bar styled for hashing operations.
///
/// `total` is only what the bar shows before the first phase boundary —
/// [`find_duplicates`] resets the length to the number of reads it is going to
/// perform as soon as phase 1 tells it what that is, so the scan count passed
/// here is a placeholder for the microseconds phase 1 takes and not a claim
/// about the work ahead.
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

    /// A file that no longer has the length the scan recorded is refused, and
    /// the message says both lengths.
    ///
    /// It used to be hashed: the digest described the new bytes and the file
    /// went into the plan under its old size. That is worse than a skip, and
    /// quietly so — phase 1 partitioned by the old size, the group reports it
    /// and the journal records it as the evidence undo checks, so every one of
    /// those is now a fact about a file that does not exist. A file whose
    /// length is moving is usually one still being written; the run says so and
    /// leaves it alone.
    #[test]
    fn test_partial_hash_refuses_a_file_that_changed_size_since_the_scan() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shrinking.jpg");
        fs::write(&path, multi_buffer_body()).unwrap();

        // The scan saw 300 000 bytes; by the time we read it there are 4.
        fs::write(&path, b"tiny").unwrap();

        let error = format!("{:#}", partial_hash(&path, 300_000).unwrap_err());
        assert!(
            error.contains("300000") && error.contains('4'),
            "the operator needs both lengths to know what happened, got {error:?}"
        );

        assert!(
            partial_hash(&path, 4).is_ok(),
            "the same file at the length it is really is must hash normally"
        );
    }

    /// For a file large enough to have a distinct tail, the partial hash is
    /// head-then-tail.
    ///
    /// Pins the digest against an independent one-shot computation, because
    /// changing *which* bytes are hashed would repartition every existing
    /// user's library on upgrade.
    #[test]
    fn test_partial_hash_is_head_then_tail() {
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

        let partial = partial_hash(&path, size as u64).unwrap();
        assert_eq!(partial.digest, expected.finalize().to_hex().to_string());
        assert!(
            !partial.covers_whole_file,
            "192 KB is not covered by two 64 KB windows"
        );
    }

    // -----------------------------------------------------------------
    // How much of a file the partial hash reads, at every boundary
    // -----------------------------------------------------------------

    /// The coverage rule at the exact sizes where it changes.
    ///
    /// Boundaries are where an off-by-one lives, and this one is load-bearing
    /// twice over: one side of it decides whether a file is read again in phase
    /// 3, and the other decides whether its partial digest may be recorded as
    /// its content hash. `<=` slipping to `<` at 64 KB would publish a
    /// head-only digest as a full one.
    #[test]
    fn test_the_coverage_rule_at_every_boundary() {
        let k64 = PARTIAL_HASH_BYTES;
        let cases = [
            (0, PartialCoverage::WholeFile),
            (1, PartialCoverage::WholeFile),
            (k64 - 1, PartialCoverage::WholeFile),
            (k64, PartialCoverage::WholeFile),
            (k64 + 1, PartialCoverage::HeadOnly),
            (k64 * 2 - 1, PartialCoverage::HeadOnly),
            (k64 * 2, PartialCoverage::HeadOnly),
            (k64 * 2 + 1, PartialCoverage::HeadAndTail),
        ];

        for (size, expected) in cases {
            assert_eq!(
                partial_coverage(size),
                expected,
                "a {size}-byte file must be {expected:?}"
            );
        }
    }

    /// And the same boundaries end to end: the digest a file of each length
    /// actually gets, computed independently from the bytes the rule says are
    /// read.
    ///
    /// The 128 KB case is the one worth staring at. A file of exactly 131 072
    /// bytes is fingerprinted on its first half and nothing else — two such
    /// files differing only in their second half both survive phase 2. That is
    /// the filter working as designed, and it is asserted here so that it is a
    /// decision rather than a thing somebody discovers.
    #[test]
    fn test_the_partial_hash_reads_what_the_coverage_rule_says() {
        let tmp = TempDir::new().unwrap();
        let k64 = usize::try_from(PARTIAL_HASH_BYTES).unwrap();

        for size in [0, 1, k64 - 1, k64, k64 + 1, k64 * 2, k64 * 2 + 1] {
            let body: Vec<u8> = (0..u32::try_from(size).unwrap())
                .map(|i| (i % 251) as u8)
                .collect();
            let path = tmp.path().join(format!("{size}.jpg"));
            fs::write(&path, &body).unwrap();

            let mut expected = blake3::Hasher::new();
            let coverage = partial_coverage(size as u64);
            match coverage {
                PartialCoverage::WholeFile => {
                    expected.update(&body);
                }
                PartialCoverage::HeadOnly => {
                    expected.update(&body[..k64]);
                }
                PartialCoverage::HeadAndTail => {
                    expected.update(&body[..k64]);
                    expected.update(&body[size - k64..]);
                }
            }

            let partial = partial_hash(&path, size as u64).unwrap();
            assert_eq!(
                partial.digest,
                expected.finalize().to_hex().to_string(),
                "wrong bytes hashed for a {size}-byte file ({coverage:?})"
            );
            assert_eq!(
                partial.covers_whole_file,
                coverage == PartialCoverage::WholeFile,
                "wrong coverage claim for a {size}-byte file"
            );

            // The claim that matters: where the flag is set, the digest really
            // is what a full read would have produced.
            if partial.covers_whole_file {
                assert_eq!(
                    partial.digest,
                    full_hash(&path).unwrap(),
                    "a {size}-byte file claimed whole-file coverage, so its partial digest \
                     must equal its full hash"
                );
            } else {
                assert_ne!(
                    partial.digest,
                    full_hash(&path).unwrap(),
                    "a {size}-byte file's partial digest must not coincide with its full \
                     hash, or this test proves nothing about the flag"
                );
            }
        }
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
        let grouped = group_by_full_hash(&[&a, &b], &ProgressBar::hidden(), &pool());

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

    /// A file length that reaches every phase of the cascade.
    ///
    /// It has to be over `PARTIAL_HASH_BYTES * 2`, and that is not a detail. A
    /// file of 64 KB or less is settled by phase 2 — its partial hash covered
    /// the whole file, so phase 3 never sees it — and a fixture built at 40 KB
    /// would leave every test below quietly asserting the sort rules of a phase
    /// that no longer ran. Above 128 KB the partial hash is a head and a tail,
    /// the middle is unread, and phase 3 does the confirming.
    const CASCADING_FILE_BYTES: usize = 150_000;

    /// A tree with two duplicate groups, a size-collision pair that is not
    /// duplicated, and a file unique by size — laid out so that the correct
    /// answers are *not* the ones any accidental ordering would give.
    ///
    /// `zzz.jpg` sits at the top of the tree and sorts lexicographically last of
    /// its group, so a plain path sort would pick `aaa/deep/one.jpg` as the
    /// original and a `HashMap` would pick whichever it felt like. The depth
    /// rule picks `zzz.jpg`, and only the depth rule does.
    fn deterministic_fixture(tmp: &TempDir) -> Vec<ScannedFile> {
        let n = CASCADING_FILE_BYTES;
        let span = u32::try_from(n).unwrap();
        let a: Vec<u8> = (0..span).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..span).map(|i| (i % 241) as u8).collect();

        vec![
            // Group A — three copies at three depths.
            plant(tmp, "aaa/deep/one.jpg", &a),
            plant(tmp, "zzz.jpg", &a),
            plant(tmp, "bbb/two.jpg", &a),
            // Group B — two copies at the same depth, so the tie-break decides.
            plant(tmp, "ccc/bravo.jpg", &b),
            plant(tmp, "ccc/alpha.jpg", &b),
            // Same length as group A and the same head and tail, differing only
            // in the middle — which the partial hash does not read. It therefore
            // survives phase 2 and is separated in phase 3, which is the one
            // job phase 3 has.
            plant(tmp, "ddd/impostor.jpg", &{
                let mut v = a.clone();
                v[n / 2] ^= 0xFF;
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
    /// [`CASCADING_FILE_BYTES`] length so all of them survive phase 1 into one
    /// enormous phase-2 candidate set, and all of them go on to phase 3.
    ///
    /// The narrow fixtures above have four or five files in them, which a
    /// work-stealing pool may well run on one thread — they prove the sort
    /// rules, not the concurrency. This one is here so that "the same tree
    /// gives the same answer" is a claim about parallel execution.
    fn wide_fixture(tmp: &TempDir, groups: u32) -> Vec<ScannedFile> {
        triplicate_fixture(tmp, groups, CASCADING_FILE_BYTES)
    }

    /// `groups` duplicate groups of three copies each, every file `size` bytes
    /// of content distinct to its group.
    ///
    /// `size` is a parameter because it is the one thing the cascade's shape
    /// turns on: at or below 64 KB phase 2 settles the groups on its own, above
    /// it phase 3 confirms them. The same tree at two lengths is how the tests
    /// below tell those two paths apart.
    fn triplicate_fixture(tmp: &TempDir, groups: u32, size: usize) -> Vec<ScannedFile> {
        let mut files = Vec::with_capacity(groups as usize * 3);
        for g in 0..groups {
            // Distinct content per group, identical length across all groups.
            let body: Vec<u8> = (0..u32::try_from(size).unwrap())
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
    /// Four files in two size groups of two, where **no** pair survives the
    /// partial hash — so phase 2 has four candidates and phase 3 has none.
    ///
    /// Both size groups hold one file sharing a 64 KB head with the other
    /// group's and one that does not, so the group splits into two buckets of
    /// one and nothing is promoted. Every file is under 128 KB, so the partial
    /// hash covers only that head.
    fn partial_hash_retires_everything_fixture(tmp: &TempDir) -> Vec<ScannedFile> {
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

        vec![
            // 70 000 bytes: one file sharing the head, one not — so this size
            // group splits into two buckets of one.
            plant(tmp, "small-shared.jpg", &shared_head(5_536, 0x11)),
            plant(tmp, "small-other.jpg", &other_head(5_536)),
            // 80 000 bytes: same shape, and its shared-head file has exactly
            // the same partial digest as `small-shared.jpg`.
            plant(tmp, "large-shared.jpg", &shared_head(15_536, 0x22)),
            plant(tmp, "large-other.jpg", &other_head(15_536)),
        ]
    }

    #[test]
    fn test_phase_2_does_not_regroup_files_phase_1_separated_by_size() {
        let tmp = TempDir::new().unwrap();
        let files = partial_hash_retires_everything_fixture(&tmp);

        assert_eq!(
            partial_hash(&files[0].path, files[0].size).unwrap().digest,
            partial_hash(&files[2].path, files[2].size).unwrap().digest,
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
    // Files phase 2 read end to end are not read again
    // -----------------------------------------------------------------

    /// A file small enough to be covered by its partial hash is never opened a
    /// second time — and the group is still detected.
    ///
    /// The evidence is the bar, which counts reads: 24 candidates and a final
    /// length of 24 means every one of the 24 full reads charged at the start
    /// of phase 2 was given back, which is only true if phase 3 was handed
    /// nothing. Asserting the groups alone would not show it — the old code
    /// found the same groups, it just read the library twice to do it.
    #[test]
    fn test_a_small_duplicate_group_is_settled_without_a_second_read() {
        const SIZE: usize = 40_000;
        let tmp = TempDir::new().unwrap();
        let files = triplicate_fixture(&tmp, 8, SIZE);
        assert_eq!(
            partial_coverage(SIZE as u64),
            PartialCoverage::WholeFile,
            "the fixture only tests anything if its files are covered by the partial hash"
        );

        let (position, length, _) = bar_after(&files);
        assert_eq!(
            length, 24,
            "24 partial hashes and no full ones — the 24 full reads charged up front must \
             have been refunded, which they are only if phase 3 got an empty candidate set"
        );
        assert_eq!(position, length);

        let result = dedup(&files);
        assert_eq!(result.duplicate_groups.len(), 8);
        assert!(result.duplicate_groups.iter().all(|g| g.files.len() == 3));
        assert_eq!(result.skipped, 0);
        assert!(result.unique.iter().all(|u| u.known_hash.is_some()));
    }

    /// The digest a settled group is filed under is the one a full read would
    /// have produced.
    ///
    /// This is the correctness half, and it is the half a shortcut gets wrong:
    /// a promotion that published a head-and-tail digest as a content hash
    /// would put a *wrong* number in the journal, where undo later compares
    /// against it before deleting somebody's file.
    #[test]
    fn test_a_settled_groups_digest_is_the_files_full_hash() {
        let tmp = TempDir::new().unwrap();
        let files = triplicate_fixture(&tmp, 4, 40_000);
        let result = dedup(&files);

        assert_eq!(result.duplicate_groups.len(), 4);
        for group in &result.duplicate_groups {
            for path in &group.files {
                assert_eq!(
                    group.hash,
                    full_hash(path).unwrap(),
                    "{} is filed under a digest that is not its content hash",
                    path.display()
                );
            }
        }
        for unique in &result.unique {
            assert_eq!(
                unique.known_hash.as_deref(),
                Some(full_hash(&unique.file.path).unwrap().as_str()),
                "{} was handed on with a digest that is not its content hash",
                unique.file.path.display()
            );
        }
    }

    /// One byte over the boundary and phase 3 runs again.
    ///
    /// The end-to-end control for the promotion: the same tree, the same shape,
    /// 65 537 bytes instead of 40 000. Its partial hash is a head only, so
    /// nothing may be settled and every file must be read a second time —
    /// length `2 × candidates` rather than `candidates`. Without this, a
    /// promotion that fired on every file regardless of coverage would pass
    /// every other test in this section.
    #[test]
    fn test_a_file_past_the_boundary_is_still_confirmed_by_phase_3() {
        const SIZE: usize = 64 * 1024 + 1;
        let tmp = TempDir::new().unwrap();
        let files = triplicate_fixture(&tmp, 4, SIZE);
        assert_eq!(partial_coverage(SIZE as u64), PartialCoverage::HeadOnly);

        let (position, length, _) = bar_after(&files);
        assert_eq!(
            length,
            12 + 12,
            "nothing here is covered by its partial hash, so all twelve files must be read \
             again in phase 3"
        );
        assert_eq!(position, length);

        let result = dedup(&files);
        assert_eq!(result.duplicate_groups.len(), 4);
        assert!(result.duplicate_groups.iter().all(|g| g.files.len() == 3));
    }

    /// A file retired by phase 2 on its own carries a digest exactly when phase
    /// 2 read all of it.
    ///
    /// Two files of one size, differing content, both covered: each is a bucket
    /// of one, and each is handed to the organiser with a real content hash
    /// that cost nothing. The journal takes that as the evidence undo checks, so
    /// it is worth having. The other side of the rule — the same shape above the
    /// boundary yielding `None` — is
    /// `test_phase_2_does_not_regroup_files_phase_1_separated_by_size`.
    #[test]
    fn test_a_small_unique_file_carries_the_hash_phase_2_already_computed() {
        let tmp = TempDir::new().unwrap();
        let files = vec![
            plant(&tmp, "one.jpg", &vec![0x11u8; 40_000]),
            plant(&tmp, "two.jpg", &vec![0x22u8; 40_000]),
        ];

        let result = dedup(&files);

        assert!(result.duplicate_groups.is_empty());
        assert_eq!(result.unique.len(), 2);
        for unique in &result.unique {
            assert_eq!(
                unique.known_hash.as_deref(),
                Some(full_hash(&unique.file.path).unwrap().as_str()),
                "{} was read end to end by phase 2, so its digest is already paid for",
                unique.file.path.display()
            );
        }
    }

    /// A file that changed length between the scan and the hash is dropped from
    /// the analysis and counted, exactly as an unreadable one is.
    ///
    /// It is not offered to the organiser under either length: not the stale one
    /// the plan was built from, and not the new one, which nothing else in this
    /// run has seen.
    #[test]
    fn test_a_file_that_changed_size_since_the_scan_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let steady = plant(&tmp, "steady.jpg", &vec![0x33u8; 40_000]);
        let moving = plant(&tmp, "moving.jpg", &vec![0x44u8; 40_000]);

        // Still being written: the scan saw 40 000 bytes, the hash finds more.
        fs::write(&moving.path, vec![0x44u8; 41_000]).unwrap();

        let files = vec![steady.clone(), moving.clone()];
        let result = dedup(&files);

        assert_eq!(result.skipped, 1);
        assert_eq!(
            result
                .unique
                .iter()
                .map(|u| &u.file.path)
                .collect::<Vec<_>>(),
            vec![&steady.path],
            "the file that moved under the run must not reach the plan"
        );
        assert!(result.duplicate_groups.is_empty());
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
    ///
    /// Both halves, because the ceiling on its own is satisfied by a default of
    /// one and that would be a different bug: it would cost every machine its
    /// parallelism while still passing a test named after the ceiling. The
    /// second assertion pins the rule the doc comment states — `min(cores,
    /// ceiling)` — so a default that stopped tracking the machine is a failure
    /// rather than a silent single-threaded run.
    #[test]
    fn test_the_default_thread_count_respects_the_ceiling() {
        let threads = default_hash_threads().get();
        assert!(
            threads <= DEFAULT_HASH_THREAD_CEILING,
            "{threads} threads is past the {DEFAULT_HASH_THREAD_CEILING}-thread ceiling — an \
             unbounded default is what this setting exists to prevent"
        );

        let expected = available_parallelism()
            .map_or(1, NonZeroUsize::get)
            .min(DEFAULT_HASH_THREAD_CEILING);
        assert_eq!(
            threads, expected,
            "the default is this machine's parallelism capped at the ceiling, not a constant"
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

    // -----------------------------------------------------------------
    // What the progress bar says while all that is happening
    // -----------------------------------------------------------------

    /// The bar's state after a cascade, as a caller could read it.
    fn bar_after(files: &[ScannedFile]) -> (u64, u64, String) {
        let bar = hashing_progress_bar(files.len() as u64);
        let _ = find_duplicates(files, &bar, &pool());
        (
            bar.position(),
            bar.length().unwrap_or_default(),
            bar.message(),
        )
    }

    /// The bar reaches its total by doing the work, not by
    /// [`ProgressBar::finish`] setting the position to the length on the way
    /// out.
    ///
    /// This is the property the phase's title is about. Before it, the bar's
    /// length was the *scan* count while only the hashing candidates ever
    /// ticked it, so a run over a library of mostly-unique files — which is
    /// every real library — finished at something like 40/12000 and the
    /// operator's only clue that it had not stalled was that the program
    /// exited.
    ///
    /// Asserted on a hidden bar, which is what the tests and any non-terminal
    /// caller get: a hidden bar keeps its position and length exactly as a
    /// visible one does, so "renders correctly when hidden" is the same claim
    /// as "the numbers are right when nobody is drawing them".
    #[test]
    fn test_the_bar_ends_exactly_full() {
        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 32);

        let (position, length, message) = bar_after(&files);

        // 96 files, all one size, in 32 partial-hash groups of three — so every
        // one of them is partial-hashed and every one is promoted to a full
        // read. Spelt out rather than derived from `position`, which would make
        // the assertion true of any number the cascade happened to reach.
        assert_eq!(
            length,
            96 + 96,
            "one tick per read, and there are two per file"
        );
        assert_eq!(position, length, "the bar must arrive at its total");
        assert_eq!(message, PHASE_3_MESSAGE);
    }

    /// A library with no size collisions reads nothing, and says so: no reads
    /// charged, no reads outstanding.
    ///
    /// The degenerate case matters because it is the common one — most files in
    /// a photo library have a size nothing else shares — and because a bar of
    /// length zero is where an accounting scheme divides by it.
    #[test]
    fn test_a_library_with_no_size_collisions_charges_no_reads() {
        let tmp = TempDir::new().unwrap();
        let files = vec![
            plant(&tmp, "a.jpg", b"one"),
            plant(&tmp, "b.jpg", b"two-"),
            plant(&tmp, "c.jpg", b"three"),
        ];

        let (position, length, _) = bar_after(&files);

        assert_eq!(length, 0);
        assert_eq!(position, length);
    }

    /// The second read is charged up front and *refunded* when phase 2 rules it
    /// out — so a corpus phase 2 retires entirely ends at one tick per file,
    /// not two.
    #[test]
    fn test_the_full_read_charge_is_refunded_when_phase_2_retires_a_file() {
        let tmp = TempDir::new().unwrap();
        let files = partial_hash_retires_everything_fixture(&tmp);

        let (position, length, _) = bar_after(&files);

        assert_eq!(
            length,
            files.len() as u64,
            "four partial hashes and no full ones — the four full reads charged at the \
             start of phase 2 must have been given back"
        );
        assert_eq!(position, length);
    }

    /// A file that will not open still ticks.
    ///
    /// The bar counts reads *attempted*, because that is what the operator
    /// waited for. A bar that only counted successes would stall a dozen short
    /// of its total on a library with a dozen unreadable files, and stalling is
    /// the one thing a progress bar must not do for a reason it is not going to
    /// explain — the skip count in the summary is where that is explained.
    #[cfg(unix)]
    #[test]
    fn test_an_unreadable_file_still_ticks_the_bar() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 4);
        let locked = tmp.path().join("g000/c0/photo.jpg");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let denied = File::open(&locked).is_err();
        let (position, length, _) = bar_after(&files);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

        if !denied {
            eprintln!(
                "SKIPPED test_an_unreadable_file_still_ticks_the_bar: a 0o000 file was still \
                 readable, so this process ignores permission bits (running as root?)"
            );
            return;
        }

        // 12 files partial-hashed; the locked one fails there and is not
        // promoted, so 11 full reads follow.
        assert_eq!(length, 12 + 11);
        assert_eq!(
            position, length,
            "the file that would not open was still opened for — the bar must account for it"
        );
    }

    /// Every file ticks as *its own* read finishes, not once per phase and not
    /// once per size group.
    ///
    /// Observed from inside the hashing closure: each file records the position
    /// it saw on the way in. On a one-thread pool, the file that runs `k`th
    /// finds `k` ticks already banked, whatever order the pool chooses to run
    /// them in — so the observations are exactly `0..n`, sorted. Batched at the
    /// end of the phase, as the old code did, every observation would be `0`.
    #[test]
    fn test_every_file_ticks_as_its_own_read_finishes() {
        use std::sync::Mutex;

        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 4);
        let candidates: Vec<&ScannedFile> = files.iter().collect();

        let bar = ProgressBar::hidden();
        bar.set_length(candidates.len() as u64);
        let observed = Mutex::new(Vec::new());
        let serial = HashPool::with_threads(NonZeroUsize::MIN).unwrap();

        let (_groups, skipped) = group_by_key(
            &candidates,
            |file| {
                observed
                    .lock()
                    .expect("the observation log is never poisoned")
                    .push(bar.position());
                full_hash(&file.path)
            },
            |_, digest| digest,
            &bar,
            &serial,
        );

        assert_eq!(skipped, 0);
        let mut seen = observed
            .into_inner()
            .expect("the observation log is never poisoned");
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..candidates.len() as u64).collect::<Vec<_>>(),
            "each file must have found a different number of ticks already banked — all \
             zeroes means the phase ticked once at the end"
        );
        assert_eq!(bar.position(), candidates.len() as u64);
    }

    /// A terminal that keeps every line drawn to it, so a test can read what a
    /// watching operator would have seen rather than only the final state.
    ///
    /// [`indicatif::ProgressDrawTarget::term_like`] applies no rate limit of its
    /// own, so every redraw the bar asks for is recorded — including the one
    /// each `set_message` and `set_length` triggers, which is what makes the
    /// phase-boundary assertions below deterministic rather than a race against
    /// the redraw clock.
    #[derive(Debug, Clone, Default)]
    struct RecordingTerm {
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl RecordingTerm {
        fn record(&self, s: &str) {
            if !s.trim().is_empty() {
                self.lines
                    .lock()
                    .expect("the frame log is never poisoned")
                    .push(s.to_string());
            }
        }

        /// Every recorded frame, parsed into the position, the length and the
        /// message the template put on the line.
        fn frames(&self) -> Vec<(u64, u64, String)> {
            self.lines
                .lock()
                .expect("the frame log is never poisoned")
                .iter()
                .filter_map(|line| parse_frame(line))
                .collect()
        }
    }

    impl indicatif::TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            // Wide enough that nothing the template emits is truncated away.
            200
        }
        fn move_cursor_up(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }
        fn write_line(&self, s: &str) -> io::Result<()> {
            self.record(s);
            Ok(())
        }
        fn write_str(&self, s: &str) -> io::Result<()> {
            self.record(s);
            Ok(())
        }
        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Pull `pos`, `len` and the message back out of one rendered line.
    ///
    /// The template is `... {pos}/{len} {msg}`, so the first whitespace-delimited
    /// token that reads as two integers around a slash is the counter and
    /// everything after it is the message.
    fn parse_frame(line: &str) -> Option<(u64, u64, String)> {
        let mut rest = line;
        while let Some(start) = rest.find('/') {
            let (before, after) = rest.split_at(start);
            let pos = before
                .rsplit(|c: char| c.is_whitespace())
                .next()
                .and_then(|t| t.parse::<u64>().ok());
            let after = &after[1..];
            let end = after
                .find(|c: char| c.is_whitespace())
                .unwrap_or(after.len());
            let len = after[..end].parse::<u64>().ok();
            if let (Some(pos), Some(len)) = (pos, len) {
                return Some((pos, len, after[end..].trim().to_string()));
            }
            rest = after;
        }
        None
    }

    /// What a watching operator actually sees: the bar never goes backwards,
    /// never overshoots, and never shows one phase's message beside another
    /// phase's outstanding work.
    ///
    /// The last is the phase-boundary claim. Phases cannot overlap in wall-clock
    /// time because `group_by_key` ends in a `collect`, and a collect is a
    /// barrier — so no frame may carry the phase-3 message while the phase-2
    /// reads are still landing. If the barrier were ever removed in the name of
    /// overlapping the phases, this is what would fail, and the message would
    /// have to become something that could honestly describe both at once.
    #[test]
    fn test_the_bar_reads_honestly_frame_by_frame() {
        /// 32 groups of three, all one size, so every file is partial-hashed.
        const PHASE_2_READS: u64 = 96;

        let tmp = TempDir::new().unwrap();
        let files = wide_fixture(&tmp, 32);

        let term = RecordingTerm::default();
        let bar = hashing_progress_bar(files.len() as u64);
        bar.set_draw_target(indicatif::ProgressDrawTarget::term_like(Box::new(
            term.clone(),
        )));

        let _ = find_duplicates(&files, &bar, &pool());

        let frames = term.frames();
        assert!(
            frames.len() >= 3,
            "the three phase messages alone must have drawn three frames, got {}",
            frames.len()
        );

        // Fractions compared as rationals, cross-multiplied, rather than as
        // floats: the question is whether the bar ever moved backwards, and an
        // answer that depended on rounding would be no answer. A length of zero
        // is a finished bar, not a division by it.
        let fraction = |(position, length): (u64, u64)| {
            if length == 0 {
                (1u128, 1u128)
            } else {
                (u128::from(position), u128::from(length))
            }
        };
        let mut previous = (0u128, 1u128);
        let mut length_after_phase_2_began: Option<u64> = None;
        for (position, length, message) in &frames {
            assert!(
                position <= length,
                "{position}/{length} — the bar showed more work done than there was"
            );

            let current = fraction((*position, *length));
            assert!(
                current.0 * previous.1 >= previous.0 * current.1,
                "the bar went backwards, from {}/{} to {position}/{length}",
                previous.0,
                previous.1
            );
            previous = current;

            if message == PHASE_2_MESSAGE {
                assert!(
                    *position <= PHASE_2_READS,
                    "{position} reads shown during phase 2, which only has {PHASE_2_READS}"
                );
                length_after_phase_2_began.get_or_insert(*length);
            }
            if message == PHASE_3_MESSAGE {
                assert!(
                    *position >= PHASE_2_READS,
                    "phase 3's message was displayed at {position} reads, before phase 2's \
                     {PHASE_2_READS} had all landed — the phases are overlapping"
                );
            }
            if let Some(charged) = length_after_phase_2_began {
                assert!(
                    *length <= charged,
                    "the length grew from {charged} to {length}; it may only be refunded"
                );
            }
        }

        assert_eq!(
            frames.last().map(|(_, _, message)| message.as_str()),
            Some(PHASE_3_MESSAGE),
            "the run ends in phase 3, so that is what the last frame drawn must say"
        );
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

    /// A file that phase 2 read and phase 3 could not is counted as skipped —
    /// *added* to whatever phase 2 already skipped, and left out of `unique`.
    ///
    /// The race is real and untestable against a real filesystem: the file has
    /// to survive the partial read and be gone by the full one. Injected at the
    /// phase-3 read for exactly that reason — see [`find_duplicates_with`].
    ///
    /// The two skips are made to happen in different phases on purpose. Phase 2
    /// skips `vanished.jpg` (its length no longer matches the scan), phase 3
    /// skips `locked-b.jpg`, and the expected total is the sum. A single skip in
    /// one phase would be counted the same by any arithmetic at all.
    #[test]
    fn test_a_file_lost_between_the_two_reads_is_counted_with_the_earlier_skips() {
        let tmp = TempDir::new().unwrap();

        // Over 128 KB, so the partial hash is a head and a tail and phase 2
        // cannot settle the group on its own — these have to reach phase 3.
        let body = vec![7u8; 200_000];
        let a = tmp.path().join("locked-a.jpg");
        let b = tmp.path().join("locked-b.jpg");
        fs::write(&a, &body).unwrap();
        fs::write(&b, &body).unwrap();

        // Phase 2's own skip: two files agreeing on a size that neither of them
        // is any more, so both are refused before a digest is computed.
        let stale = tmp.path().join("vanished.jpg");
        let stale_twin = tmp.path().join("vanished-twin.jpg");
        fs::write(&stale, b"nine byte").unwrap();
        fs::write(&stale_twin, b"nine byte").unwrap();

        let size = body.len() as u64;
        let files = vec![
            make_scanned(a, size),
            make_scanned(b.clone(), size),
            make_scanned(stale, 4096),
            make_scanned(stale_twin, 4096),
        ];

        let result = find_duplicates_with(&files, &ProgressBar::hidden(), &pool(), |path| {
            if path == b {
                anyhow::bail!("{} went away between the two reads", path.display());
            }
            full_hash(path)
        });

        assert_eq!(
            result.skipped, 3,
            "two files phase 2 refused plus one phase 3 could not read"
        );

        let survivors: Vec<&Path> = result
            .unique
            .iter()
            .map(|u| u.file.path.as_path())
            .collect();
        assert_eq!(
            survivors,
            vec![tmp.path().join("locked-a.jpg").as_path()],
            "a file excluded from the analysis is excluded from the plan too"
        );
        assert!(
            result.duplicate_groups.is_empty(),
            "one readable file is not a duplicate group"
        );
    }
}
