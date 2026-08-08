//! Throughput baseline for the three-phase dedup cascade.
//!
//! This exists to make a parallelisation change falsifiable. Phase 06 rewrites
//! [`find_duplicates`] to hash on a bounded `rayon` pool; without a measured
//! before, "it is faster now" is an opinion. The numbers this produces are
//! transcribed into `docs/research/hashing-baseline.md`, and the same benches
//! are re-run afterwards so the two columns sit next to each other.
//!
//! # Corpus
//!
//! Synthesised at run time into a [`TempDir`], never checked in — the same rule
//! the integration fixtures follow, for the same reason: a benchmark that
//! depends on somebody's photo library is a benchmark nobody else can run.
//!
//! Each size tier contains three populations, chosen so that every phase of the
//! cascade actually does work:
//!
//! * **Unique** — every file a distinct size, so phase 1 retires it on metadata
//!   alone and it is never opened. These are the cheap majority of a real
//!   library and they are here to keep the ratio honest.
//! * **Size-collision** — pairs sharing a size but differing from byte zero, so
//!   they survive phase 1, reach the partial hash, and are separated there.
//!   This is the phase-2 load.
//! * **True duplicate** — pairs with byte-identical content, so they survive
//!   both earlier phases and are read end to end by phase 3. This is the
//!   phase-3 load, and it is the only population whose full size is read.
//!
//! # What the numbers mean
//!
//! Criterion re-runs each benchmark many times over the *same* corpus, so after
//! the first iteration the files are in the page cache. What is measured is
//! therefore BLAKE3 plus per-file syscall overhead, not cold-disk throughput —
//! which is the right thing to measure for a parallelisation change (it is the
//! CPU-bound half we are trying to spread across cores) but it is emphatically
//! not a prediction of how long a first run over a cold 400 GB library takes.
//! `docs/research/hashing-baseline.md` repeats this caveat where the numbers
//! are read.

// Benches are separate crates, so the `#[cfg(test)]`-scoped allow inside the
// library does not reach here — see the note in `Cargo.toml`. A benchmark that
// cannot build its own corpus has nothing to measure, so failing loudly at the
// point of the failure is the desired behaviour.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking setup failure in a benchmark is a failed benchmark, which is the \
              desired signal"
)]

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
    criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion, Throughput,
};
use indicatif::ProgressBar;
use mmm::hasher::{self, find_duplicates, HashPool};
use mmm::scanner::ScannedFile;
use tempfile::TempDir;

/// One file-size tier of the corpus.
///
/// The counts fall away as the sizes rise because the corpus is written to a
/// real filesystem before anything is measured: holding the tiers at equal file
/// counts would mean writing several gigabytes to run a benchmark, which is a
/// good way to ensure it never gets run.
struct Tier {
    /// Appears in the criterion benchmark id, so it lands in the saved
    /// baseline and in the report table verbatim.
    name: &'static str,
    /// Nominal size of a file in this tier. Actual sizes are this plus a small
    /// offset that keeps the three populations from colliding with each other.
    base: u64,
    unique: usize,
    collision_pairs: usize,
    duplicate_pairs: usize,
}

const TIERS: &[Tier] = &[
    Tier {
        name: "small_100k",
        base: 100 * 1024,
        unique: 24,
        collision_pairs: 6,
        duplicate_pairs: 6,
    },
    Tier {
        name: "medium_5m",
        base: 5 * 1024 * 1024,
        unique: 8,
        collision_pairs: 2,
        duplicate_pairs: 2,
    },
    Tier {
        name: "large_50m",
        base: 50 * 1024 * 1024,
        unique: 1,
        collision_pairs: 1,
        duplicate_pairs: 1,
    },
];

/// Size offsets that keep the three populations in a tier from accidentally
/// sharing a size with each other. A unique file that happened to match a
/// collision pair would be a three-file size group, and the benchmark would
/// silently be measuring a different shape of work than it claims to.
const UNIQUE_OFFSET: u64 = 1;
const COLLISION_OFFSET: u64 = 1_000;
const DUPLICATE_OFFSET: u64 = 2_000;

/// Domain separators for [`seed_for`], so a unique file and a collision file at
/// the same index cannot draw the same content stream.
const UNIQUE_KIND: u64 = 1;
const COLLISION_KIND: u64 = 2;
const DUPLICATE_KIND: u64 = 3;

/// Mix a file's position in the corpus into a content seed.
///
/// The mixing is not decoration. The first version of this composed the seed by
/// XOR-ing small integers into a tier constant, which meant the two sides of a
/// collision pair differed only in bit 0 — and [`pseudo_random`] starts its
/// state at `seed | 1`, which set that bit in both and handed them the same
/// byte stream. Every "size-collision" pair was silently a byte-identical
/// duplicate, so phase 2 separated nothing and the phase-3 benchmark was
/// measuring twice the file count it claimed. A splitmix64 finaliser makes
/// adjacent inputs produce unrelated seeds, and the corpus-shape assertions in
/// [`bench_phases`] fail loudly if that ever stops being true.
fn seed_for(tier_seed: u64, kind: u64, index: u64, side: u64) -> u64 {
    let mut z = tier_seed
        .wrapping_add(kind.wrapping_mul(0x1234_5678_9ABC_DEF1))
        .wrapping_add(index.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(side.wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Sampling settings, applied identically to both groups so the two halves of
/// the report are measured the same way.
///
/// Criterion's defaults — 100 samples inside 5 seconds — cannot be met by the
/// large tier, which reads roughly 100 MB per iteration; criterion would print
/// a warning and overrun anyway. Ten samples inside a 12-second window is
/// enough resolution to see a parallel speedup, is wide enough that the
/// slowest benchmark (`phase/3_full_hash`, which wants ~10.8 s) meets it
/// without a warning, and keeps a full `cargo bench` to about ninety seconds —
/// short enough that it will actually get run.
fn configure(group: &mut BenchmarkGroup<'_, WallTime>) {
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(12));
}

/// A generated tree plus the scan result that describes it.
///
/// Holds its [`TempDir`] so the files outlive every borrow taken from
/// `per_tier`; dropping this deletes the corpus.
struct Corpus {
    _dir: TempDir,
    per_tier: Vec<(&'static str, Vec<ScannedFile>)>,
    /// Total bytes on disk per tier, in the same order as `per_tier` — the
    /// denominator criterion reports throughput against.
    bytes_per_tier: Vec<u64>,
}

impl Corpus {
    fn build() -> Self {
        let dir = TempDir::new().expect("creating the benchmark corpus directory");
        let mut per_tier = Vec::with_capacity(TIERS.len());
        let mut bytes_per_tier = Vec::with_capacity(TIERS.len());

        for (tier_index, tier) in TIERS.iter().enumerate() {
            let tier_dir = dir.path().join(tier.name);
            fs::create_dir_all(&tier_dir).expect("creating a tier directory");

            let mut files = Vec::new();
            let mut bytes = 0u64;
            // Seeds are derived from where a file sits in the corpus, so its
            // content is a pure function of its position: the same tree comes
            // back on every machine, and the "before" and "after" runs are
            // measuring identical bytes.
            let tier_seed = (tier_index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);

            for i in 0..tier.unique {
                let size = tier.base + UNIQUE_OFFSET + i as u64;
                let path = tier_dir.join(format!("unique-{i:03}.jpg"));
                write_file(&path, seed_for(tier_seed, UNIQUE_KIND, i as u64, 0), size);
                bytes += size;
                files.push(scanned(path, size));
            }

            for p in 0..tier.collision_pairs {
                let size = tier.base + COLLISION_OFFSET + p as u64;
                for side in 0..2u64 {
                    let path = tier_dir.join(format!("collision-{p:03}-{side}.jpg"));
                    // Independently seeded, so the two differ from byte zero and
                    // the partial hash separates them — deliberately the easy
                    // case for phase 2, because the hard case (identical head and
                    // tail, different middle) is what the duplicate population
                    // already covers by reaching phase 3.
                    write_file(
                        &path,
                        seed_for(tier_seed, COLLISION_KIND, p as u64, side),
                        size,
                    );
                    bytes += size;
                    files.push(scanned(path, size));
                }
            }

            for p in 0..tier.duplicate_pairs {
                let size = tier.base + DUPLICATE_OFFSET + p as u64;
                // One body written twice — the two sides are byte-identical by
                // construction rather than by both being seeded the same way,
                // so there is no seeding accident that could make them differ.
                let body = pseudo_random(seed_for(tier_seed, DUPLICATE_KIND, p as u64, 0), size);
                for side in 0..2u64 {
                    let path = tier_dir.join(format!("duplicate-{p:03}-{side}.jpg"));
                    fs::write(&path, &body).expect("writing a duplicate corpus file");
                    bytes += size;
                    files.push(scanned(path, size));
                }
            }

            per_tier.push((tier.name, files));
            bytes_per_tier.push(bytes);
        }

        Self {
            _dir: dir,
            per_tier,
            bytes_per_tier,
        }
    }

    /// Every tier at once — the shape a real library has, where a scan hands
    /// the cascade thumbnails and raw video in the same list.
    fn all(&self) -> Vec<ScannedFile> {
        self.per_tier
            .iter()
            .flat_map(|(_, files)| files.iter().cloned())
            .collect()
    }

    fn total_bytes(&self) -> u64 {
        self.bytes_per_tier.iter().sum()
    }

    /// How many files *should* survive phase 1 — everything whose size is
    /// shared with another file, which is both populations of pairs.
    fn expected_phase2_candidates() -> usize {
        TIERS
            .iter()
            .map(|t| (t.collision_pairs + t.duplicate_pairs) * 2)
            .sum()
    }

    /// How many files *should* survive phase 2 — only the true duplicates. The
    /// collision pairs differ from byte zero, so the partial hash must retire
    /// every one of them.
    fn expected_phase3_candidates() -> usize {
        TIERS.iter().map(|t| t.duplicate_pairs * 2).sum()
    }
}

fn scanned(path: PathBuf, size: u64) -> ScannedFile {
    ScannedFile {
        path,
        size,
        extension: "jpg".to_string(),
        is_video: false,
    }
}

fn write_file(path: &std::path::Path, seed: u64, size: u64) {
    fs::write(path, pseudo_random(seed, size)).expect("writing a corpus file");
}

/// Deterministic filler bytes from an xorshift64 sequence.
///
/// Not cryptographic and not meant to be — the requirement is only that the
/// bytes are the same on every machine and every run (so two runs of the
/// benchmark hash identical input) and that they are not long runs of one value
/// (which would let a filesystem sparse-allocate the file and turn a read
/// benchmark into a benchmark of nothing).
fn pseudo_random(seed: u64, size: u64) -> Vec<u8> {
    let len = usize::try_from(size).expect("corpus file size fits in usize");
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Total wall-clock of the whole cascade, per tier and over the mixed corpus.
fn bench_cascade(c: &mut Criterion, corpus: &Corpus) {
    // The machine's default bound — the same one a plain `mmm` run gets, so
    // these numbers describe the shipped configuration rather than a
    // benchmark-only one.
    let pool = HashPool::automatic().expect("a default hashing pool must build");

    let mut group = c.benchmark_group("find_duplicates");
    configure(&mut group);

    for ((name, files), bytes) in corpus.per_tier.iter().zip(&corpus.bytes_per_tier) {
        group.throughput(Throughput::Bytes(*bytes));
        group.bench_with_input(BenchmarkId::from_parameter(*name), files, |b, files| {
            b.iter(|| {
                black_box(find_duplicates(
                    black_box(files),
                    &ProgressBar::hidden(),
                    &pool,
                ))
            });
        });
    }

    let mixed = corpus.all();
    group.throughput(Throughput::Bytes(corpus.total_bytes()));
    group.bench_with_input(BenchmarkId::from_parameter("mixed"), &mixed, |b, files| {
        b.iter(|| {
            black_box(find_duplicates(
                black_box(files),
                &ProgressBar::hidden(),
                &pool,
            ))
        });
    });

    group.finish();
}

/// The three phases measured separately over the mixed corpus, so the report
/// can say *which* phase the parallel version actually improved rather than
/// only that the total moved.
///
/// The candidate sets are derived the same way `find_duplicates` derives them:
/// phase 2 gets the files whose size is shared, phase 3 gets those whose
/// partial hash is shared.
fn bench_phases(c: &mut Criterion, corpus: &Corpus) {
    let pool = HashPool::automatic().expect("a default hashing pool must build");
    let mixed = corpus.all();
    // The hashing passes tick a bar per file. Nobody is watching a benchmark, so
    // they tick a hidden one — the atomic increment is measured either way,
    // which is right: it is a cost the real run pays too.
    let quiet = ProgressBar::hidden();

    let size_groups = hasher::group_by_size(&mixed);
    let phase2_candidates: Vec<&ScannedFile> = size_groups
        .values()
        .filter(|group| group.len() > 1)
        .flatten()
        .collect();

    let partial = hasher::group_by_partial_hash(&phase2_candidates, &quiet, &pool);
    let phase3_candidates: Vec<&ScannedFile> = partial
        .groups
        .values()
        .filter(|group| group.len() > 1)
        .flat_map(|group| group.iter().copied())
        .collect();

    // Exact counts, not merely "non-empty". A corpus that does not exercise a
    // phase would let that phase's benchmark report a flattering zero — but the
    // subtler failure, and the one that actually happened, is a corpus that
    // exercises a phase with the *wrong population*: a seeding accident made
    // every collision pair byte-identical, so phase 3 was handed twice the
    // files it should have been and the numbers described work the cascade
    // would never really do. Pinning both counts catches that.
    assert_eq!(
        phase2_candidates.len(),
        Corpus::expected_phase2_candidates(),
        "phase-1 survivors must be exactly the size-sharing files"
    );
    assert_eq!(
        phase3_candidates.len(),
        Corpus::expected_phase3_candidates(),
        "phase-2 survivors must be exactly the true duplicates — if this is high, the \
         collision pairs are not actually differing in their hashed bytes"
    );

    let mut group = c.benchmark_group("phase");
    configure(&mut group);

    group.throughput(Throughput::Elements(mixed.len() as u64));
    group.bench_function("1_size_grouping", |b| {
        b.iter(|| black_box(hasher::group_by_size(black_box(&mixed))));
    });

    group.throughput(Throughput::Elements(phase2_candidates.len() as u64));
    group.bench_function("2_partial_hash", |b| {
        b.iter(|| {
            black_box(hasher::group_by_partial_hash(
                black_box(&phase2_candidates),
                &quiet,
                &pool,
            ))
        });
    });

    group.throughput(Throughput::Elements(phase3_candidates.len() as u64));
    group.bench_function("3_full_hash", |b| {
        b.iter(|| {
            black_box(hasher::group_by_full_hash(
                black_box(&phase3_candidates),
                &quiet,
                &pool,
            ))
        });
    });

    group.finish();
}

/// One entry point, so the ~350 MB corpus is written once per `cargo bench`
/// rather than once per benchmark group.
fn bench_hashing(c: &mut Criterion) {
    let corpus = Corpus::build();

    // Printed rather than asserted: it is the context that makes the numbers
    // interpretable when they are read back months later, and it belongs in the
    // benchmark output next to them.
    let file_count: usize = corpus.per_tier.iter().map(|(_, f)| f.len()).sum();
    let parallelism = std::thread::available_parallelism()
        .map_or_else(|_| "unknown".to_string(), |n| n.get().to_string());
    println!(
        "corpus: {file_count} files, {} MiB, {} tiers, available_parallelism = {parallelism}\n\
         corpus: {} files reach phase 2, {} reach phase 3",
        corpus.total_bytes() / (1024 * 1024),
        TIERS.len(),
        Corpus::expected_phase2_candidates(),
        Corpus::expected_phase3_candidates(),
    );

    bench_cascade(c, &corpus);
    bench_phases(c, &corpus);
}

criterion_group!(benches, bench_hashing);
criterion_main!(benches);
