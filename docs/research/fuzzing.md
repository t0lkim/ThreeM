---
type: analysis
title: Fuzzing Report
created: 2026-08-09
tags:
  - testing
  - fuzzing
  - quality
  - security
related:
  - '[[coverage-report]]'
  - '[[mutation-testing]]'
  - '[[journal-format]]'
  - '[[format-support]]'
---

# Fuzzing Report

Measured with [`cargo-fuzz`](https://rust-fuzz.github.io/book/cargo-fuzz.html)
0.13.2 (Homebrew) over `code/fuzz/`, on macOS 15 (aarch64), toolchain
`nightly-2026-05-01` (rustc 1.97.0-nightly), AddressSanitizer on,
`debug-assertions` and `overflow-checks` on.

Reproduce with:

```sh
cd code
brew install cargo-fuzz
rustup toolchain install nightly-2026-05-01 --profile minimal
cargo +nightly-2026-05-01 fuzz run parse_iso6709 \
  fuzz/scratch/parse_iso6709 fuzz/corpus/parse_iso6709 -- -max_total_time=180
```

Setup, the corpus convention and the reason the nightly is pinned are in
[`code/fuzz/README.md`](../../code/fuzz/README.md).

## Why these four

Coverage says a line ran. Mutation testing says a test noticed when a line
changed. Neither says what happens when the *input* is something nobody wrote on
purpose — and four of this tool's inputs are produced by somebody else's
software:

| Input | Written by | Parsed by |
|---|---|---|
| EXIF / `QuickTime` date strings | camera firmware | `metadata::parse_wall_clock` |
| ISO 6709 location string | video container | `metadata::parse_iso6709` |
| XMP sidecar | Adobe, darktable, Lightroom | `xmp::parse` |
| Journal line | a previous `mmm` run, possibly interrupted | `journal::parse_line` |

A panic in any of them stops a run part-way through moving a library, which is
the state this project exists to avoid leaving somebody in.

## Runs

Every target starts from the checked-in corpus. Executions are libFuzzer's own
count.

| Target | Duration | Executions | Exec/s | Peak RSS | Crashes |
|---|---:|---:|---:|---:|---:|
| `parse_iso6709` (before the fix) | 3s | 164 | — | 56 MB | **1** |
| `parse_iso6709` (after) | 181s | 9,772,769 | 53,993 | 552 MB | 0 |
| `parse_wall_clock` | 181s | 8,088,221 | 44,686 | 511 MB | 0 |
| `journal_line` | 181s | 6,831,584 | 37,743 | 915 MB | 0 |
| `xmp_sidecar` | 241s | 4,948,246 | 20,532 | 846 MB | 0 |

Roughly 29.6 million executions in total. Peak RSS is libFuzzer holding its own
in-memory corpus, not the parser: the slowest single unit across all four runs
was under a second, and no run tripped the default 2 GB allocation limit, so
nothing here shows an unbounded allocation.

## What was found

### A coordinate that is not a coordinate — fixed

`-33.8688+302.2093/`, found after 164 executions.

`parse_iso6709` ended in two `f64::from_str` calls and returned whatever they
produced. `f64::from_str` is not a coordinate validator: it accepts any
magnitude, so `+302.2093` degrees of longitude parsed as readily as `+002.2093`,
and it accepts the strings `NaN`, `inf` and `-inf` verbatim.

The consequence is not a crash, which is why no test had caught it and why it
needed an assertion rather than a panic to surface. The pair goes to
`geocoder::GeoLookup::lookup`, which does not reject them either — measured
directly, a `NaN` latitude and an infinite latitude **both** return the first
record in the GeoNames dataset:

```
lookup(NaN, NaN) -> LocationInfo { city: "El Tarter", country: "AD", … }
lookup(inf, inf) -> LocationInfo { city: "El Tarter", country: "AD", … }
```

So a video with a corrupt location tag came out filed and named after Andorra,
with nothing in the output to distinguish an invented location from a read one.

`parse_iso6709` now returns `None` for a non-finite coordinate or one outside
ISO 6709's own bounds (latitude ±90, longitude ±180). `None` is the same answer
a video carrying no location tag gives, which is the truth. Covered by
`metadata::tests::a_coordinate_that_is_not_a_coordinate_is_refused`, with
`the_extremes_of_the_coordinate_system_are_accepted` beside it so a later
tightening cannot quietly exclude the poles and the antimeridian.

The crashing input is checked in as
`fuzz/corpus/parse_iso6709/regression-longitude-past-antimeridian`. With the fix
backed out it reproduces in under a second, which is how the CI gate below was
verified in the failing direction as well as the passing one.

### The predicted UTF-8 panic — measured, and it is not there

The Phase 07 task predicted that `parse_iso6709` "currently slices byte offsets
into a `&str`, which will panic on multi-byte UTF-8 input". It does slice byte
offsets, and it does not panic, and the reason is worth writing down rather than
leaving as a lucky escape: every offset it slices at is the position of a `+` or
a `-`, both ASCII, and **an ASCII byte can never occur inside a multi-byte UTF-8
sequence** — continuation bytes all have the high bit set. So each of those
offsets is a character boundary by construction.

The one place that looked wrong on reading — `&lon_part[..=j]`, where `j` is an
offset into `lon_part[1..]` — is correct for the same reason: `j` is a boundary
in the sub-slice, so `j + 1` is a boundary in the parent, and `..=j` is exactly
`..j + 1`.

Eight million executions of `parse_wall_clock` and nine and a half million of
`parse_iso6709`, both taking `&str` built from the input's longest valid UTF-8
prefix and so routinely containing multi-byte characters, produced no such
panic. The prediction was reasonable and the code is fine; recorded here so the
next reader does not re-derive it.

### Nothing in the other three

`parse_wall_clock`, `xmp_sidecar` and `journal_line` produced no crash, hang or
allocation failure at these budgets. That is a weaker statement than it looks —
see the gaps below — but it does cover `chrono`'s strftime parser, `quick-xml`'s
namespace resolution and entity unescaping, and `serde_json`'s recursion limit,
all under sanitiser.

The `journal_line` target asserts more than the absence of a panic: a parsed
entry is re-serialised and re-parsed, and the two must be equal. Undo acts on
what it reads, so a value the format can express but not preserve is a file that
would not be restored. Six point eight million executions found no such value.

## The CI gate

`.github/workflows/ci.yml` gains a `fuzz` job: sixty seconds per target on every
push and pull request, starting from `fuzz/corpus/`, with `-timeout=10` so an
input that loops forever is reported rather than hanging the runner until GitHub
kills it. Crash inputs are uploaded as an artifact on failure.

Sixty seconds is not a campaign and is not meant to be. It catches the two things
a pull request can break: a checked-in regression seed crashing again, and a
shallow new panic reachable from the seeds. The loop was dry-run locally
verbatim before being committed, and the gate was confirmed to fail — not merely
to pass — by backing the coordinate fix out and re-running the regression seed.

## Gaps, stated plainly

* **Nothing runs long.** The longest run here is four minutes; the CI gate is one
  minute. Real fuzzing campaigns run for hours or days, and the defect classes
  they find are the ones that need a deep, structured input to reach. Nobody has
  run one, and there is no scheduled job that would.
* **`xmp_sidecar` was explored at libFuzzer's default 4 KB maximum input.** A
  real Lightroom sidecar with a full edit history is larger than that. The
  behaviour of the parser on a document big enough to matter for memory is
  untested by this.
* **Corpus depth is shallow.** Three to six hand-written seeds per target. A
  campaign seeded from a few thousand real sidecars and real journals off a disk
  would start somewhere much better than this does.
* **Three parsers are not fuzzed at all.** The TOML settings loader
  (`settings.rs`, `config.rs`) reads a file the user wrote — less hostile, but
  still parsed — and `undo.rs` consumes what `journal.rs` produces. Neither has a
  target. The scanner's path handling has none either.
* **The journal target reaches deserialisation, not framing.** `Journal::read`'s
  own logic — the truncated-tail rule, the schema-version check, the "a bad line
  anywhere but the last is corruption" rule — is covered by unit tests and not by
  this. A target over a whole multi-line journal file would be strictly better.
* **No structure-aware fuzzing.** The targets take raw bytes and `&str`. Deriving
  `Arbitrary` for `JournalEntry` would let the fuzzer build well-formed entries
  and explore the *values* rather than spending most of its budget failing to
  produce valid JSON.
* **The toolchain is pinned to a nightly four months old** because the current
  one does not compile `nom-exif` 1.5.2. That is recorded in
  `code/fuzz/README.md` with the exact error; it is a dependency problem, not a
  fuzzing one, but it does mean the sanitiser in use is not the newest available.
