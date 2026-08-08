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

Everything below is transcribed from one run of that command. The raw console
output is not committed; re-running the command reproduces it.

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

## Whole-cascade wall-clock

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

## Per-phase timing

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
   speedup and keeps a full `cargo bench` to about 109 seconds wall-clock, which
   is short enough that it will actually get run. It is not enough to resolve a
   5% change — do not read one into these tables.
3. **One machine, one storage type.** Everything here is an 8-core Apple Silicon
   laptop with an internal NVMe-class SSD. The thread-count guidance that comes
   out of this work has to be stated in terms of storage class, not copied from
   these numbers.
4. **`phase/1_size_grouping` is at the edge of resolution.** A 12 µs measurement
   with a 6 µs spread is mostly timer and allocator noise. The only claim it
   supports is the one being made: phase 1 is free, and parallelising it is not
   worth doing.

## Parallel results

_Not yet measured. The remaining tasks in Phase 06 parallelise the cascade and
append the second column here, with speedup factors per tier, plus an explicit
note on any tier that shows no improvement or a regression._

Small-file workloads are the tier to watch: at ~101 µs per file in phase 2, that
population is bound by open/close syscalls rather than by hashing, and threads do
not make a syscall cheaper. If `small_100k` fails to improve, that is the
explanation to check first — and it should be reported, not quietly shipped.
