---
type: decision
title: Parallel hashing
created: 2026-08-08
tags:
  - performance
  - concurrency
  - dedup
related:
  - '[[hashing-baseline]]'
  - '[[TECHNICAL]]'
  - '[[USER-GUIDE]]'
  - '[[CHANGELOG]]'
---

# ADR-007: Parallel hashing, and what had to be settled first

**Status:** Accepted
**Date:** 2026-08-08

## Problem

The dedup cascade hashed one file at a time. On the project's benchmark corpus that put the whole of `find_duplicates` at 102.49 ms, of which the full-content hash was 94.35 ms — **92% of the cascade in a single phase, running at 1.255 GiB/s, which is roughly single-threaded BLAKE3 on the machine that measured it.** One core was busy and seven were idle, and a photo library is the workload where that matters most: the cascade is the only part of a run that reads whole files, and there may be hundreds of gigabytes of them.

Spreading that across cores is obvious. Three things about doing it were not.

1. **The output was not deterministic to begin with, so there was nothing to preserve.** The cascade's working sets are `HashMap`s, and a `HashMap` iterates in a randomly seeded order. Which copy of a duplicate group was kept — the one file *not* moved into `duplicates/` — was therefore decided by that seed, and two runs over the same untouched library could disagree. Parallelising on top of that would have made a real defect unfalsifiable: every "the output changed" report would have had two candidate causes, and no way to tell them apart.
2. **More concurrency is not uniformly better.** The bound the CPU wants and the bound the storage device wants are different numbers, and on a spinning disk they point in opposite directions.
3. **The resilience contract had to survive the thread boundary.** Phase 02 established that one unreadable file costs one file. A `?` inside a parallel closure abandons every other file's completed work.

## Decision

**Phases 2 and 3 hash in parallel on a bounded pool of the run's own. The answer is settled by sorting after the hashing finishes, so it cannot depend on how wide the pool is or which thread won.**

### Determinism is established first, and by construction

Two orderings are now rules rather than accidents, both applied *after* the parallel work has collected:

```rust
fn by_depth_then_path(a: &Path, b: &Path) -> Ordering {
    a.components().count().cmp(&b.components().count()).then_with(|| a.cmp(b))
}
```

- **Within a duplicate group** — members are sorted by `by_depth_then_path`, and `files[0]` is the retained original. Depth leads because copies accumulate downwards: of `photo.jpg` and `backup/old-phone/photo.jpg`, the one nearer the top of the tree is the one somebody filed deliberately. Lexicographic order breaks the tie, which makes the rule *total* — for any two distinct paths it names one, with no appeal to iteration order, filesystem walk order or thread completion order.
- **Between groups** — sorted by the group's BLAKE3 digest, with the retained original's path as a tie-break. A group therefore keeps its `duplicates/NNN` number between runs over the same tree.
- **`unique` is sorted by the same comparator.** Not asked for by the change, and load-bearing: `unique` is the order the organiser plans moves in, so an unsorted `unique` decides by coin toss which of two files competing for one name gets `photo.jpg` and which gets `photo-1.jpg`. That is the same "same tree, same output" property, one list over.

The guarantee this buys is exact and is worth stating as a contract: **a run at `--threads 1` and a run at the default return byte-identical duplicate groups, the same retained original in each, the same `duplicates/NNN` numbering and the same plan order.** Only the pace changes. It is pinned at two levels — `test_one_thread_gives_the_same_answer_as_the_parallel_default` over the whole rendered `DedupResult`, and `one_thread_plans_exactly_what_the_parallel_default_plans`, which drives the real binary twice over one tree and compares the printed plan byte for byte.

### Flat, not group-at-a-time — which mattered more than `par_iter` did

The old cascade walked one size group and hashed within it. **Duplicate groups are typically pairs**, so parallelising *inside* a group would have capped the whole cascade at two-way concurrency on an eight-core machine, regardless of how many files were waiting. Both phases now hand their entire candidate set to one call, so the pool sees hundreds of files at once.

That flattening needed a bucket key, which is why `group_by` became `group_by_key`, generic over it:

| Phase | Key | Why |
|---|---|---|
| 2 — partial hash | `(size, digest)` | A partial hash is head-only below 128 KB, so two files of **different lengths** can share one — a truncated download and the file it came from. Keyed by digest alone, flattening would have re-merged files phase 1 had already separated by size, and bought each such pair a needless full read. |
| 3 — full hash | `digest` | Merges nothing. Equal full hashes mean equal content, which means equal size, so those files were in one size group already. |

### Phase 1 stays serial, by measurement

Grouping by size reads no file content and measured **12 µs against phase 3's 94 ms** — 0.012% of the cascade. There is nothing there to win. That is the baseline's finding, not an assumption made in advance.

### No shared counter, because the design removes the shared state

Rayon's `map(...).collect()` into a `Vec` yields results in **input order** regardless of completion order. So the skip counter is an ordinary local in a serial fold over that vector: no counter is shared across threads at all, which is stronger than incrementing one atomically, and it makes the warning order reproducible as a side effect. The per-file `Result` is Phase 02's resilience contract carried intact across the thread boundary — an unreadable file becomes an `Err` in the vector, never a `?` that would abandon every other file's finished work.

Bucketing a few hundred hash-map entries against a phase that reads gigabytes costs nothing measurable, so there is no case for a `Mutex<HashMap>`.

### The bound is a queue depth for the disk, not a use of the CPU

`--threads <N>` and the `hash_threads` config key, defaulting to **`min(available_parallelism(), 8)`**.

The cap is the substantive part, not the flag. Rayon's global pool defaults to `available_parallelism()`, so the first parallel version already fired one read per logical core: unchanged on an eight-core laptop, and sixty-four concurrent reads at somebody's photo library on a sixty-four-core workstation. Hashing is fast enough that the cascade spends most of its time waiting on reads, so the thread count is really a queue depth — and queue depth stops helping long before core count does. A spinning disk has **one head**, so every concurrent reader past the first turns a sequential read into a seek storm; a network share pays a round trip per thread. The users who will never pass a flag are protected by the ceiling, not by the flag.

`NonZeroUsize`, throughout, because **rayon reads `num_threads(0)` as "use your default"**. A `hash_threads = 0` merely passed through would not be a very small bound — it would be *no bound at all*, silently, on the one setting whose entire job is to be one. One shared `validate_hash_threads` refuses zero (and anything above 1024) at all three doors — the TOML deserialiser, the `MMM_HASH_THREADS` parser and clap's `value_parser` — with the same message, because a flag that gave a different reason from a config file for rejecting the same value sends the reader looking for a difference that is not there.

### A pool of the run's own, never the global one

`HashPool` wraps a dedicated `rayon::ThreadPool`, built in `main` and passed down.

Rayon's global pool is process-wide and built once, on first use, from whatever asked for it first. Configuring the hashing bound through it would mean a setting that could only be applied before anything else touched rayon, could not be changed afterwards, and could not be tested twice in one process at two different values. It also scopes the bound to the thing it is a bound *on*: `hash_threads` describes how hard to push the storage device during dedup, and nothing else in the process inherits it.

Building it in `main` rather than inside `find_duplicates` is deliberate too — a thread count that nothing can be spawned for stops the run before the cascade opens a single file.

The tests are what make the choice concrete rather than tidy: `test_a_pool_runs_on_no_more_workers_than_it_was_built_with` builds a two-thread pool and a one-thread pool **in the same process** and watches which worker indices actually pick work up. That is not expressible against a global pool.

### Phase 3 does not re-read what phase 2 read end to end

A file of 64 KB or less is covered entirely by its partial hash — the head read reaches the end of the file — so that digest *is* its full digest, and a group of such files is confirmed the moment phase 2 buckets it. The boundary is a named function with a table rather than two inline `>`s:

| Size | Hashed | `PartialCoverage` |
|---|---|---|
| `0 ..= 64 KB` | the whole file | `WholeFile` — the digest is promoted to a full hash |
| `64 KB+1 ..= 128 KB` | the first 64 KB only | `HeadOnly` — a tail read here would overlap the head |
| `128 KB+1 ..` | the first and last 64 KB | `HeadAndTail` |

The middle band is the surprising one and its behaviour is deliberately unchanged: changing which bytes are hashed there would repartition every existing user's library on upgrade.

**The promotion is a property of the read, not of `ScannedFile.size`.** Trusting scan-time metadata to decide "the partial hash covered this file" is trusting a number taken before the file was opened — a file that grew from 10 KB to 200 KB in between would have its first 64 KB published as its content hash. `partial_hash` therefore takes the scan size back as an *argument* and uses it as a check rather than as a length: it compares against the length of the open handle, refuses a mismatch as a per-file skip, and refuses again if a read returns fewer bytes than that length promised.

### The progress bar counts reads, not files

Under a flattened parallel cascade "this group is done" stopped being a milestone, so the bar counts what the operator is actually waiting for. Counting files would make it lie about where the time goes — phase 1 retires most of a library in microseconds, so a per-file bar leaps to 90% in the first millisecond and then spends the whole run on the last tenth. Each phase-2 candidate is charged two reads and the second is refunded for every file phase 2 rules out, so the position rises, the length only falls, and the fraction never goes backwards.

## Measured results

Apple M1 Pro, 8 logical cores (6 performance + 2 efficiency), internal NVMe-class SSD, APFS; same machine, OS build, toolchain, corpus and sampling settings for both columns. Full tables, confidence intervals and per-tier figures in [`hashing-baseline`](../research/hashing-baseline.md).

| Measurement | Serial | Parallel | Speedup |
|---|---|---|---|
| Whole cascade (`mixed`) | 102.49 ms | **39.174 ms** | **2.62×** (≥2.46× on the intervals) |
| Phase 3 — full hash | 94.347 ms | **37.181 ms** | **2.54×** |
| Phase 2 — partial hash | 3.6495 ms | **1.3194 ms** | **2.77×** |
| Tier `small_100k` | 2.7948 ms | **1.4233 ms** | 1.96× |
| Tier `medium_5m` | 15.471 ms | **5.6211 ms** | 2.75× |
| Tier `large_50m` | 73.179 ms | **36.082 ms** | 2.03× |

**2.62× on eight cores, not 8×**, for three reasons in order of size, none of which is a defect: the corpus is too small at the top end (phase 3 reads 18 files, two of them 50 MiB, so its tail is one thread finishing a large file while seven idle — a real library has thousands); six of the eight cores are fast and two are not; and BLAKE3 at 3.18 GiB/s against page-cached bytes is memory-bandwidth-bound before it is core-bound.

Two results are worth reporting rather than banking:

- **The tier flagged as at-risk improved, and the baseline's reasoning about it was wrong in the cautious direction.** `small_100k` was predicted possibly not to move, since at ~101 µs per file phase 2 is bound by `open`/`seek`/`read`/`close` and threads do not make a syscall cheaper. It went 1.96×, and phase 2 went 2.77× — better than phase 3 did. Threads do not make a syscall cheaper, but they do put several in flight at once. No tier regressed.
- **Phase 1 reads 1.61× faster and did not change.** `group_by_size` is byte-for-byte identical to the version that produced the 12.460 µs and is still serial. The serial interval was `[10.041, 16.070] µs` — a 48% spread the baseline already called timer noise — against a tight parallel `[7.6996, 7.7751]`. The honest reading is that the old number was unreliable, not that anything got faster.

The partial-hash promotion scores **exactly 0%** in that table, because the smallest benchmark tier is 100 KiB and the promotion only fires at 64 KiB or below. It was measured directly instead, on a tree built for it: 6 400 duplicate 40 KiB files (250 MB), page-cache warm, whole-run wall clock **0.61 s → 0.52 s**, with phase 3 going from 6 400 reads to zero.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Parallelise within each size group, one group at a time** | Duplicate groups are usually pairs, so this caps the cascade at two-way concurrency however many cores are free. It is also the version that *looks* like the obvious change, which is why it is recorded here rather than left as a road not taken. |
| **Parallelise phase 1 as well** | Measured at 12 µs — 0.012% of the cascade. A thread pool cannot make free work cheaper, and the phase would gain a concurrency hazard in exchange for nothing measurable. |
| **Use rayon's global pool** | The bound would be settable only before anything else in the process touched rayon, unchangeable afterwards, and untestable at two values in one process. Its default width is `available_parallelism()` uncapped, which is the seek-storm behaviour the ceiling exists to stop. |
| **An `AtomicUsize` skip counter** | Synchronises shared state that does not need to exist. `collect()` into a `Vec` preserves input order, so the count is a local in a serial fold — and the warnings come out reproducibly as a consequence, which an atomic would not give. |
| **A `Mutex<HashMap>` written from inside the parallel closure** | Buys contention on a few hundred inserts to save a serial pass that costs nothing measurable against a phase that reads gigabytes. It would also let completion order back into the output, which is the property this ADR spends its first section removing. |
| **Sort the output as it is produced rather than after collection** | There is no "as produced" under a work-stealing pool. Sorting after the barrier is what makes the answer independent of the pool's width, and the barrier is already there — `collect` is one. |
| **Leave the default unbounded at `available_parallelism()`** | Right for an NVMe SSD and wrong for the storage a photo library is most often actually on. The people it hurts are the ones with a 64-core workstation and an external drive, and they are exactly the people least likely to go looking for a flag. |
| **Clamp `hash_threads = 0` to 1 instead of refusing it** | Zero already means something to rayon — "use your default" — so clamping and passing through differ by the entire bound. A value with two plausible readings should be refused by name at the layer that read it, with a message naming the file. |
| **Charge the progress bar for a full read only once phase 2 confirms one** | The same arithmetic run the other way: the bar reaches 100% at the end of a phase worth 3.5% of the run, then falls back for the phase worth 92%. A bar that goes backwards is worse than one that is pessimistic. |
| **Add a sub-64 KiB tier to the benchmark corpus so the promotion shows up** | Every phase benchmark and the `mixed` cascade run over the whole corpus, so a fourth tier changes their input and retires the serial column as a comparison — spending the measurement in order to re-measure something already measured directly. The corpus's blindness is recorded instead. |

## Consequences

- **Breaking, library:** `find_duplicates`, `group_by_partial_hash` and `group_by_full_hash` all take a `&HashPool`; the two `group_by_*` functions also take a `&ProgressBar` as their second of three arguments. `find_duplicates` takes `&[ScannedFile]` rather than consuming a `Vec`, and owns its bar's position, length and message for the duration of the call. Pass `ProgressBar::hidden()` where there is nobody watching — the benches do, which also keeps them measuring the atomic increment a real run pays for.
- **A re-run over an already-deduplicated library may keep a different copy than the first run did.** The old choice was arbitrary, so this replaces an arbitrary behaviour rather than breaking a defined one — but somebody who has already organised a library and re-runs it will see files move. `duplicates/NNN` numbering changes for the same reason: it now follows the groups' digests.
- **`--threads` bounds the duplicate scan only.** Scanning, metadata extraction and the move phase are still serial, so the flag will not make them faster. Stated in the user guide as well, because "threads" reads as "the whole run" to anyone who has not read this.
- **The measured speedup is an upper bound on what a cold library sees.** Criterion re-runs over the same corpus, so everything is page-cache-warm and what is measured is BLAKE3 plus syscalls. On a spinning disk or a network share the concurrent seeks may cost more than the parallel hashing saves — which is what `--threads` exists to answer, and there is **no published figure** for what a given count buys on a given device. The honest advice, and what the guide says, is to time a preview run at two or three values.
- **A file whose length changed since the scan is now skipped rather than hashed.** It is reported like any other skipped file, with a warning naming both lengths. This closes the window between the scan and the hash, not the one after it — nothing can promise a file did not change after its last byte was read.
- **Files at or below 64 KB now carry a content digest into the journal** that they previously did not, because the promotion makes their partial hash a full one. `mmm undo` therefore verifies content rather than length for that part of a run.
- **Between 64 KB and 128 KB nothing improves.** Those files are fingerprinted on their opening bytes and still read a second time to confirm. The saving is real for small files, zero for large ones, and absent in that band.
- **The benchmark is the regression guard, and it has to be run to be one.** `cargo bench` takes about 109 s at ten samples; `cargo bench -- --test` runs every benchmark once for a fraction of that, which is what a CI gate should use.
