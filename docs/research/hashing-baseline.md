---
type: analysis
title: Hashing Throughput Baseline
created: 2026-08-08
tags:
  - performance
  - hashing
  - dedup
  - benchmark
related:
  - '[[adr-007-parallel-hashing]]'
  - '[[TECHNICAL]]'
---

# Hashing Throughput Baseline

Measurements of the serial three-phase dedup cascade, taken **before** any
parallelisation work, so that the parallel version has something to be compared
against. A parallelisation change with no benchmark is a guess.

The benchmark lives in `code/benches/hashing.rs` and is run with:

```console
$ cd code && cargo bench --bench hashing
```

Everything below is transcribed from runs of that command — one before the
parallelisation work and one after. The raw console output is not committed;
re-running the command reproduces it. Sections up to and including
[Caveats](#caveats) describe the serial baseline; [Parallel
results](#parallel-results) is the second run, with the speedups.

## Headline

**Phase 3 is 92% of the cascade.** On the mixed corpus the whole of
`find_duplicates` takes 102.49 ms, and the full-content hash accounts for 94.35 ms
of it. Phase 2 is 3.6%, and phase 1 is 0.012% — twelve microseconds, which is
noise.

That single number should decide the shape of the parallelisation work: spreading
phase 3 across cores is the entire opportunity, phase 2 is worth doing only
because it is nearly free to do at the same time, and parallelising phase 1 would
buy nothing measurable at any library size. It is also the sanity check on the
result — if a parallel version does not move phase 3, it has not moved anything.

## Machine

The numbers are meaningless without this, and they are not portable to another
machine.

| Property | Value |
|---|---|
| CPU | Apple M1 Pro |
| Logical cores | 8 (6 performance + 2 efficiency) |
| `available_parallelism()` | 8 |
| Memory | 16 GiB |
| Storage | Internal Apple Fabric SSD (NVMe-class), APFS |
| OS | macOS 26.5.2 (build 25F84) |
| Toolchain | rustc 1.92.0 (ded5c06cf 2025-12-08) |
| Build profile | `bench`, inheriting `[profile.release]` — `opt-level=3`, `lto`, `codegen-units=1` |

The core split matters for the later thread-count work: six of these eight cores
are fast and two are not, so a naive "one thread per logical core" default will
not scale by a factor of eight, and a measured speedup below 8× is expected
rather than a failure.

## Corpus

Synthesised into a temp directory at run time — no checked-in binary assets, the
same rule the integration fixtures follow. Content comes from a seeded xorshift
stream, so the tree is byte-identical on every run and the before/after
comparison is over the same input.

Each tier holds three populations, so that every phase does real work:

- **Unique** — every file a distinct size. Retired by phase 1 on metadata alone;
  never opened.
- **Size-collision** — pairs sharing a size, differing from byte zero. They
  survive phase 1 and are separated by the partial hash. This is the phase-2 load.
- **True duplicate** — pairs with byte-identical content. They survive both
  earlier phases and are read end to end. This is the phase-3 load.

| Tier | File size | Files | On disk | Reach phase 2 | Reach phase 3 |
|---|---|---|---|---|---|
| `small_100k` | ~100 KiB | 48 | 4.72 MiB | 24 | 12 |
| `medium_5m` | ~5 MiB | 16 | 80.01 MiB | 8 | 4 |
| `large_50m` | ~50 MiB | 5 | 250.01 MiB | 4 | 2 |
| **Total** | — | **69** | **334.74 MiB** | **36** | **18** |

Only 124.21 MiB of that 334.74 MiB is actually read per cascade iteration — 37.1%
— because the unique population is retired on metadata and the partial hash reads
at most 128 KiB of a file however large it is. That ratio is the cascade working
as designed, and it is worth remembering when reading the GiB/s figures below:
criterion computes throughput against the corpus size on disk, not against bytes
read.

The corpus counts are asserted, not assumed. `bench_phases` fails the run if the
number of files reaching phase 2 or phase 3 is not exactly what the tier
definitions imply. This is not defensive decoration — the first draft of the
corpus had a seeding bug that made every "size-collision" pair byte-identical, so
phase 2 separated nothing and the phase-3 benchmark silently measured 36 files
instead of 18. The numbers looked entirely plausible. The assertion is what makes
the next such mistake loud.

## Whole-cascade wall-clock (serial)

`find_duplicates` end to end, including a hidden `ProgressBar`. Criterion reports
a confidence interval; the middle figure is the point estimate.

| Benchmark | Lower | **Estimate** | Upper | Throughput (on-disk bytes) |
|---|---|---|---|---|
| `find_duplicates/small_100k` | 2.7265 ms | **2.7948 ms** | 2.8397 ms | 1.6500 GiB/s |
| `find_duplicates/medium_5m` | 15.256 ms | **15.471 ms** | 15.691 ms | 5.0505 GiB/s |
| `find_duplicates/large_50m` | 72.588 ms | **73.179 ms** | 73.934 ms | 3.3363 GiB/s |
| `find_duplicates/mixed` | 98.723 ms | **102.49 ms** | 108.09 ms | 3.1894 GiB/s |

The per-tier throughput figures are not comparable with each other, because each
tier has a different ratio of on-disk bytes to bytes actually read.
`medium_5m` looks fastest only because a larger share of its corpus is retired
without being opened. Read them as "this tier, before and after", never as
"this tier versus that one".

## Per-phase timing (serial)

The three phases measured separately over the mixed corpus, so a later run can
say *which* phase changed rather than only that the total moved. Throughput here
is in files per second, since what varies between phases is how much of each file
is read.

| Phase | Files in | Bytes read | Lower | **Estimate** | Upper | Share of cascade |
|---|---|---|---|---|---|---|
| 1 — size grouping | 69 | 0 | 10.041 µs | **12.460 µs** | 16.070 µs | 0.012% |
| 2 — partial hash | 36 | 3.00 MiB | 3.4458 ms | **3.6495 ms** | 3.8177 ms | 3.56% |
| 3 — full hash | 18 | 121.21 MiB | 91.403 ms | **94.347 ms** | 97.074 ms | 92.05% |

The three phases sum to 98.01 ms against a measured mixed cascade of 102.49 ms.
The ~4.5 ms difference is the cascade's own bookkeeping — cloning `ScannedFile`s
into the `unique` vector, building the intermediate group vectors — plus the fact
that the phase benchmarks group each phase's whole candidate set in one call
while `find_duplicates` calls it once per size group.

Effective hashing rate against **bytes actually read**:

| Phase | Rate |
|---|---|
| 2 — partial hash | 0.803 GiB/s |
| 3 — full hash | 1.255 GiB/s |
| Whole cascade | 1.183 GiB/s |

Phase 2 being the slower of the two per byte is the expected shape: it reads at
most 128 KiB per file, so its cost is dominated by `open`/`seek`/`read`/`close`
rather than by BLAKE3 — about 101 µs per file. That is the reason to be sceptical
about a large parallel win on small-file workloads, and it is called out again in
the re-benchmark section below.

Phase 3 at 1.255 GiB/s is roughly single-threaded BLAKE3 on this CPU, which is
the point: **one core is doing all of it**, and seven are idle.

## Caveats

These bound what the numbers can honestly be used for.

1. **The files are in the page cache.** Criterion re-runs each benchmark many
   times over the same corpus, so after the first iteration nothing touches the
   storage device. What is measured is BLAKE3 plus syscall overhead. That is the
   right thing to measure for a parallelisation change — the CPU-bound half is
   exactly what we are trying to spread — but it is *not* a prediction of a first
   run over a cold multi-hundred-gigabyte library, where the disk, not the CPU,
   sets the pace. A parallel speedup measured here is an upper bound on the
   speedup a user with a cold cache will see, and on a spinning disk or a network
   share the real figure may be a slowdown. That is what the `--threads` bound
   exists for.
2. **Ten samples, not a hundred.** The large tier reads ~121 MiB per iteration;
   criterion's defaults cannot sample that a hundred times inside a sane window.
   Ten samples in a 12-second window is enough resolution to see a multi-core
   speedup and keeps a full `cargo bench` to about two minutes wall-clock, which
   is short enough that it will actually get run. It is not enough to resolve a
   5% change — do not read one into these tables. On the `small_100k` tier it is
   not enough to resolve a *fifty* percent change either; see
   [Reproducibility](#reproducibility), which measures that directly rather than
   leaving this caveat as an estimate.
3. **One machine, one storage type.** Everything here is an 8-core Apple Silicon
   laptop with an internal NVMe-class SSD. The thread-count guidance that comes
   out of this work has to be stated in terms of storage class, not copied from
   these numbers.
4. **`phase/1_size_grouping` is at the edge of resolution.** A 12 µs measurement
   with a 6 µs spread is mostly timer and allocator noise. The only claim it
   supports is the one being made: phase 1 is free, and parallelising it is not
   worth doing.

## Parallel results

Measured after the whole of Phase 06 landed — the deterministic sorts, the
`rayon` cascade, the bounded `HashPool`, the per-read progress bar and the
partial-hash promotion. Same machine, same OS build, same toolchain, same
corpus, same sampling settings; the only variable is the code.

Two things this column is *not*. It is not a measurement of `par_iter` alone —
five commits sit between the two runs and the delta is their sum. And "before"
means the table above, not criterion's own `change:` line, which compares against
whatever run last wrote `target/criterion` (task 3's, in practice) and therefore
understates the distance travelled.

### Whole-cascade wall-clock

| Benchmark | Serial | **Parallel** | Speedup | Conservative | Throughput (on-disk bytes) |
|---|---|---|---|---|---|
| `find_duplicates/small_100k` | 2.7948 ms | **1.4233 ms** | **1.96×** | ≥1.86× | 3.2400 GiB/s (was 1.6500) |
| `find_duplicates/medium_5m` | 15.471 ms | **5.6211 ms** | **2.75×** | ≥2.63× | 13.901 GiB/s (was 5.0505) |
| `find_duplicates/large_50m` | 73.179 ms | **36.082 ms** | **2.03×** | ≥1.99× | 6.7665 GiB/s (was 3.3363) |
| `find_duplicates/mixed` | 102.49 ms | **39.174 ms** | **2.62×** | ≥2.46× | 8.3446 GiB/s (was 3.1894) |

Parallel confidence intervals, for completeness: small `[1.3757, 1.4596] ms`,
medium `[5.4360, 5.7847] ms`, large `[35.915, 36.333] ms`, mixed
`[38.611, 39.996] ms`.

The **Speedup** column divides point estimate by point estimate.
**Conservative** divides the serial *lower* bound by the parallel *upper* bound
and rounds down — the smallest speedup consistent with both intervals. Ten
samples is not enough to resolve a 5% change (caveat 2), so the conservative
column is the one to quote if a single number has to be defended.

### Per-phase timing

| Phase | Serial | **Parallel** | Speedup | Share of cascade (was) |
|---|---|---|---|---|
| 1 — size grouping | 12.460 µs | **7.7279 µs** | 1.61× † | 0.020% (0.012%) |
| 2 — partial hash | 3.6495 ms | **1.3194 ms** | **2.77×** | 3.37% (3.56%) |
| 3 — full hash | 94.347 ms | **37.181 ms** | **2.54×** | 94.91% (92.05%) |

† **Phase 1 did not change and this figure is a baseline artefact, not a win.**
`group_by_size` is byte-for-byte identical to the version that produced the
12.460 µs (`git diff d67674c..HEAD -- code/src/hasher.rs` touches its doc comment
and its callers, never its body), and it is still serial by design. The serial
interval was `[10.041, 16.070] µs` — a 48% spread that caveat 4 already called
mostly timer and allocator noise — against a parallel interval of
`[7.6996, 7.7751] µs`, which is tight. The honest reading is that the *old*
number was unreliable, not that the phase got faster. Either way it is one part
in five thousand of the cascade, and no conclusion rests on it.

Phase 3 remains the cascade, and by a slightly larger margin than before: it was
92.05% of the serial run and is 94.91% of the parallel one, because phase 2
parallelised marginally better than it did. The prediction from the baseline
headline held — the parallel version moved phase 3, so it moved something.

Effective rate against **bytes actually read**:

| Phase | Serial | **Parallel** | Speedup |
|---|---|---|---|
| 2 — partial hash (3.00 MiB) | 0.803 GiB/s | **2.220 GiB/s** | 2.77× |
| 3 — full hash (121.21 MiB) | 1.255 GiB/s | **3.184 GiB/s** | 2.54× |
| Whole cascade (124.21 MiB) | 1.183 GiB/s | **3.096 GiB/s** | 2.62× |

The three phases now sum to 38.51 ms against a measured mixed cascade of
39.17 ms — 0.67 ms of bookkeeping, where the serial run had 4.48 ms. That gap
closing is a second, independent confirmation of what the parallel work actually
did: the serial `find_duplicates` called the grouping helper once per size group
while the phase benchmarks called it once for the whole set, and the two shapes
cost measurably different amounts. `find_duplicates` now flattens the candidate
set into one call as well, so the two measurements have converged on the same
shape of work.

### Why 2.6× and not 8×

Eight logical cores, a 2.62× mixed speedup. Three reasons, in order of size, and
none of them is a defect to be fixed:

1. **The corpus is too small at the top end.** Phase 3 reads 18 files, of which
   two are 50 MiB and four are 5 MiB. The tail of the phase is one thread
   finishing a 50 MiB file while seven have nothing left to take. `large_50m`
   is the clearest case — 5 files, 2 of which reach phase 3, so it is a two-way
   parallel workload on an 8-core machine and it returned 2.03×, which is
   essentially the whole of the available win. A real library has thousands of
   files and this ceiling does not apply to it.
2. **Six of the eight cores are fast and two are not.** The baseline's machine
   section flagged this in advance: a "one thread per logical core" default
   cannot scale by eight on this CPU, and a figure below 8× is the expected
   shape rather than a failure.
3. **BLAKE3 is memory-bandwidth-bound before it is core-bound.** At 3.18 GiB/s
   against page-cached bytes, phase 3 is reading from RAM as fast as it hashes;
   adding threads past that point buys progressively less.

### The small-file tier: the prediction was wrong, in the cautious direction

The baseline section this replaces named `small_100k` as the tier to watch, on
the argument that at ~101 µs per file phase 2 is bound by `open`/`seek`/`read`/
`close` rather than by hashing, and that threads do not make a syscall cheaper.

It improved 1.96×, and phase 2 — the phase that argument was about — improved
2.77×, better than phase 3 did.

The reasoning was right about the cost and wrong about the conclusion. Threads do
not make a syscall cheaper, but they do let several be in flight at once: the
kernel work of eight concurrent `open`s overlaps on eight cores much as eight
concurrent hashes do. Per read across the small tier (36 reads: 24 in phase 2,
12 in phase 3), the cost fell from 77.6 µs to 39.5 µs; the tier's effective rate
against the 2.70 MiB it actually reads went from 0.942 GiB/s to 1.849 GiB/s.

No tier regressed, and none failed to improve, so there is nothing here of the
kind that was to be reported rather than quietly shipped. The one number that
moved without a cause behind it is phase 1, and it is dissected above rather
than left in the table to be read as a win.

### Reproducibility

The tables above are each a single run. The end-of-phase gate re-ran the whole
suite against a tree with **no source change at all** since the run that produced
them — `git diff a7fbce5..HEAD -- code/` is empty, the five commits in between
touch only documentation — so the second run is a direct measurement of how much
these figures move when nothing moves.

| Benchmark | Recorded above | Gate re-run | Speedup vs serial, re-run |
|---|---|---|---|
| `find_duplicates/small_100k` | 1.4233 ms | 1.6847 ms | 1.66× |
| `find_duplicates/medium_5m` | 5.6211 ms | 5.2276 ms | 2.96× |
| `find_duplicates/large_50m` | 36.082 ms | 36.180 ms | 2.02× |
| `find_duplicates/mixed` | 39.174 ms | 38.652 ms | 2.65× |
| `phase/2_partial_hash` | 1.3194 ms | 1.2868 ms | 2.84× |
| `phase/3_full_hash` | 37.181 ms | 38.687 ms | 2.44× |

**Everything except the small tier reproduces.** `large_50m` lands within 0.3%,
`mixed` within 1.3%, phases 2 and 3 within 2.5% and 4%. The headline claim — a
2.6× cascade, phase 3 doing the bulk of it — is stable across runs.

**`small_100k` does not reproduce, and criterion's `change:` line on it should be
ignored.** The gate run reported it `+52.906% (p = 0.00)`, *Performance has
regressed*, against a binary that cannot have regressed because it is the same
binary. Re-running that one benchmark three further times, back to back, on
unchanged code:

| Run | Point estimate | Criterion's verdict |
|---|---|---|
| gate | 1.6847 ms | regressed +52.9% (p = 0.00) |
| repeat 1 | 1.5604 ms | **improved** −31.2% (p = 0.01) |
| repeat 2 | 1.4897 ms | no change (p = 0.47) |
| repeat 3 | 2.2686 ms | regressed +42.4% (p = 0.01) |

Five measurements of one binary spanning 1.42–2.27 ms, with criterion reporting a
statistically significant improvement *and* a statistically significant
regression among them. The tier's honest speedup is **somewhere between 1.2× and
2.0×**, and this corpus cannot pin it tighter; the 1.96× in the table above is
the luckiest of the five, not a lie, but it should not be quoted to three
significant figures.

The cause is the tier's shape, not the machine being busy. `small_100k` is
48 files of ~100 KiB finishing in under two milliseconds, so a single scheduling
hiccup or page-cache eviction is a large fraction of the total, and the work is
too short for the parallel section to amortise pool wake-up. The tiers that read
hundreds of megabytes have no such problem. **A future run that wants to compare
small-file performance needs a longer-running small tier, not more samples of
this one.**

### What this corpus cannot measure

**There is no sub-64 KiB tier, so the partial-hash promotion scores exactly 0%
here.** The promotion only fires when a file is no larger than
`PARTIAL_HASH_BYTES` (64 KiB), and the smallest tier is 100 KiB — the benchmark
still reports the same `36 files reach phase 2, 18 reach phase 3` it did before
that change. That is a property of the corpus, not of the change.

**A tier was deliberately not added to fix this.** Every phase benchmark and the
`mixed` cascade run over the whole corpus, so a new tier changes their input and
retires the serial column above as a comparison — trading the measurement this
document exists for against a second measurement of something already measured
another way. The promotion was instead measured directly, on a tree built for it:
6 400 duplicate 40 KiB files (250 MB), page-cache warm on this machine, whole-run
wall clock 0.61 s → 0.52 s (median of seven, interleaved with the control), with
phase 3 going from 6 400 reads to zero. The deterministic half of that claim —
that the second read does not happen — is asserted by unit tests rather than
timed.

If a small-file tier is ever wanted here, add it as a *fourth* tier in a new
document with its own before/after pair, rather than editing this corpus.

### Verification

`code/tests/dedup.rs` — the eleven end-to-end tests that pin what a user
actually gets — all pass unchanged: the retained original, the group directories,
the manifests, and `one_thread_plans_exactly_what_the_parallel_default_plans`.
The full suite is 566 tests across all targets, all passing. Speed is the only
thing these numbers were allowed to change.
