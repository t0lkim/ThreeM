---
type: analysis
title: Fuzzing Report
created: 2026-08-10
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

Re-measured 2026-08-10 against the v0.3.1 tree, replacing the 2026-08-09 report
rather than patching its numbers. Measured with
[`cargo-fuzz`](https://rust-fuzz.github.io/book/cargo-fuzz.html) 0.13.2
(Homebrew) over `code/fuzz/`, on macOS 26.5.2 (aarch64, build 25F84), toolchain
`nightly-2026-05-01` (rustc 1.97.0-nightly `f53b654a8`), AddressSanitizer on,
`debug-assertions` and `overflow-checks` on.

Reproduce with:

```sh
cd code
brew install cargo-fuzz
rustup toolchain install nightly-2026-05-01 --profile minimal
host=$(rustc -vV | sed -n 's/^host: //p')
cargo +nightly-2026-05-01 fuzz run --target "$host" parse_iso6709 \
  fuzz/scratch/parse_iso6709 fuzz/corpus/parse_iso6709 -- -max_total_time=180
```

Setup, the corpus convention and the reason the nightly is pinned are in
[`code/fuzz/README.md`](../../code/fuzz/README.md).

## Why these five

Coverage says a line ran. Mutation testing says a test noticed when a line
changed. Neither says what happens when the *input* is something nobody wrote on
purpose — and five of this tool's inputs are produced by somebody else's
software:

| Input | Written by | Parsed by |
|---|---|---|
| EXIF / `QuickTime` date strings | camera firmware | `metadata::parse_wall_clock` |
| `OffsetTimeOriginal` / `OffsetTime` tag | camera firmware | `timezone::parse_offset` |
| ISO 6709 location string | video container | `metadata::parse_iso6709` |
| XMP sidecar | Adobe, darktable, Lightroom | `xmp::parse` |
| Journal line | a previous `mmm` run, possibly interrupted | `journal::parse_line` |

A panic in any of them stops a run part-way through moving a library, which is
the state this project exists to avoid leaving somebody in.

The offset tag is the row that is new, and it is new because **the previous
report was wrong to say there were four.** See
[the fifth input, found by counting](#the-fifth-input-found-by-counting).

## Runs

Every target starts from the checked-in corpus. Executions are libFuzzer's own
count. Each target ran 180 seconds; the reported 181 is libFuzzer rounding up its
final unit.

| Target | Duration | Executions | Exec/s | Peak RSS | New units | Crashes |
|---|---:|---:|---:|---:|---:|---:|
| `parse_iso6709` | 181s | 5,535,414 | 30,582 | 194 MB | 103 | 0 |
| `parse_wall_clock` | 181s | 4,785,525 | 26,439 | 162 MB | 137 | 0 |
| `parse_offset` (new) | 181s | 4,381,968 | 24,209 | 250 MB | 379 | 0 |
| `journal_line` | 181s | 4,065,603 | 22,461 | 463 MB | 2,600 | 0 |
| `xmp_sidecar` | 181s | 1,490,229 | 8,233 | 473 MB | 3,370 | 0 |

**15.9 million executions, fifteen minutes, no crash, no hang, no allocation
failure.** The slowest single unit across all five was under a second and none
tripped libFuzzer's default 2 GB allocation limit, so nothing here shows an
unbounded allocation. `fuzz/artifacts/` holds exactly one file after the run and
it is the 2026-08-09 coordinate crash — SHA-1 `858033d6…`, byte-identical to the
promoted seed `corpus/parse_iso6709/regression-longitude-past-antimeridian`,
checked rather than assumed.

**The exec/s figures are not comparable to the 2026-08-09 ones and the fall is
not a regression.** That run was the first on this machine and started from an
empty `scratch/`; this one started from the 347–3,482 machine-generated units the
earlier run left behind, so every iteration now costs more corpus to carry. The
comparable statement is the coverage one: `parse_iso6709` sat at `cov: 99
ft: 221` and added 103 units to a corpus of 82 — it is still finding new edges,
not spinning. CI starts cold from `corpus/` on every run, so the CI gate's
throughput is the *older* profile, not this one.

## The fifth input, found by counting

`timezone::parse_offset` reads an EXIF `OffsetTimeOriginal` (0x9011) or
`OffsetTime` (0x9010) tag — `metadata.rs:553` fetches the tag,
`metadata.rs:808` hands its text to the parser. That is bytes written by camera
firmware going through a parser, which is the exact definition the other four
targets were selected by, and **it had no target and was not named as a gap.**
It landed on 2026-08-08 in `55cfb91`, one day before the report that overlooked
it, which is how: the parser was newer than the survey and nobody re-ran the
survey.

It is worth a target on its own terms rather than for symmetry. `chrono`
publishes no offset parser, so `parse_offset` splices the tag into a whole
datetime string — `format!("1970-01-01 00:00:00 {text}")` — and parses *that*
under `%#z`. The input is interpolated into the subject of a format string, which
is a wider surface than the three spellings (`+08:00`, `+0800`, `+08`) the doc
comment names. And the value decides **which day a photograph is filed under**: a
frame shot at 23:30 in Singapore is the 15th or the 16th on the strength of this
parser alone, so a wrong answer is a photograph in the wrong directory rather
than a crash — the failure mode a fuzzer only catches if the target asserts
something beyond "did not panic".

`fuzz_targets/parse_offset.rs` asserts two things:

* **The offset is within `FixedOffset`'s ±24h domain.** `parse_wall_clock`'s
  target holds the same invariant for the offsets it returns; this parser reaches
  `chrono` by a different route and could not inherit it.
* **It survives a round trip through `Display`.** A parsed offset is written back
  out — into the RFC 3339 timestamps the journal records and the report prints —
  and undo acts on what it reads. Whatever this parser accepts must be re-readable
  as the same offset.

**4,381,968 executions found no counterexample to either.** Seven seeds are
checked in, including the specification's all-spaces "unknown" spelling, which
correctly parses as nothing and falls through to the configured resolution.

**Verified in the failing direction, not merely observed to pass.** With
`parse_offset` altered to return the parsed offset plus one second, the target
failed in under a second on a seed-derived input: `offset from "+11" did not
survive a round trip through +11:00:01`. `%#z` does not accept a seconds
component, so the shifted offset formats to something the parser then refuses —
which is the class of defect the round-trip assertion exists for. The source was
restored and `git diff` confirmed byte-identical.

## What the earlier runs found

Carried forward from 2026-08-09, still true, not re-derived.

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

### The journal round trip

The `journal_line` target asserts more than the absence of a panic: a parsed
entry is re-serialised and re-parsed, and the two must be equal. Undo acts on
what it reads, so a value the format can express but not preserve is a file that
would not be restored. Now eleven million cumulative executions with no such
value.

## What was checked and needed nothing

**`src/fixtures.rs` needs no target — verified rather than assumed.** The Phase
07 task asserts it "parses nothing", and that is nearly right rather than
exactly: it *writes* the bytes of its fixtures, but it also reads them back in
two places. `duplicate_of` reads a file it declared moments earlier in order to
copy it byte-identically, and `marker_of` scans a file for the embedded
provenance marker with `find_subslice` and hand-rolled index arithmetic — which
is the shape that would deserve a target if the bytes came from outside.

They do not. Both read files this generator itself stamped, inside a harness, in
a directory the user nominated for disposal. And the arithmetic is safe by
construction: `find_subslice` only returns a position at which the needle fully
fits, so `start = pos + MARKER_PREFIX.len()` can at most equal `bytes.len()`,
`bytes[start..]` is therefore always a valid slice, and the UTF-8 conversion goes
through `String::from_utf8`'s `Result` rather than an unchecked one. No target.

**The `--target "$host"` line is still in `.github/workflows/ci.yml`** (line 316),
and so are both action pins that the v0.3.1 work put there:
`taiki-e/install-action` at the `v2` SHA `6c6fd71` rather than a `main` SHA, and
the toolchain pinned to `nightly-2026-05-01`. This is worth confirming by reading
rather than by watching CI go green, because **neither failure mode announces
itself as what it is**: a `main` build of install-action falls back to
`cargo-binstall`, which fetches a musl-static prebuilt, and a static libc cannot
carry ASan — the error arrives two commits later as `sanitizer is incompatible
with statically linked libc`. `cargo fuzz` defaulting to the triple it was
*built* for produces an equally misdirecting complaint about a missing std.

**The CI loop enumerates `cargo fuzz list`**, so `parse_offset` is fuzzed on the
next push with no workflow edit. The job's displayed name still reads "60s per
target", which remains accurate; the wall-clock cost of the job rises from four
minutes to five per platform.

**The pinned nightly is still required — checked 2026-08-10.** `cargo +nightly
check` against nightly 1.99.0 (`969b803cb`, 2026-08-09) fails identically to the
recorded error: `expected std::ops::Range<usize>, found std::range::Range<usize>`
at `nom-exif-1.5.2/src/exif/io.rs:65`. crates.io shows no new 1.x release —
1.5.2 remains the last of the line, with the crate having moved to 3.6.2 (2026-07-29).
Neither unpin condition is met, so the pin stays and the date of the check is
recorded here rather than the question being re-derived next time.

## Gaps, stated plainly

* **Nothing runs long.** The longest run here is three minutes; the CI gate is
  one minute. Real fuzzing campaigns run for hours or days, and the defect
  classes they find are the ones that need a deep, structured input to reach.
  Nobody has run one, and there is no scheduled job that would.
* **`xmp_sidecar` was explored at libFuzzer's default 4 KB maximum input.** A
  real Lightroom sidecar with a full edit history is larger than that. The
  behaviour of the parser on a document big enough to matter for memory is
  untested by this. It is also the slowest target by a factor of three, so it
  gets the fewest executions per second of budget — the target that most needs a
  long run is the one getting the least out of a short one.
* **Corpus depth is shallow.** Three to seven hand-written seeds per target. A
  campaign seeded from a few thousand real sidecars and real journals off a disk
  would start somewhere much better than this does.
* **Three parsers are still not fuzzed.** The TOML settings loader
  (`settings.rs`, `config.rs`) reads a file the user wrote — less hostile, but
  still parsed — and `undo.rs` consumes what `journal.rs` produces. Neither has a
  target. The scanner's path handling has none either. Unchanged from the last
  report, and named again rather than assumed to be remembered.
* **The survey that picks targets is not itself automated.** The fifth input was
  found by re-reading `src/` for parsers, and it had been reachable for a day
  before the report that missed it. Nothing fails when a new parser of
  third-party bytes lands without a target — no test asserts that the set of
  `pub fn`s in `src/fuzz.rs` covers the parsers `metadata` and `xmp` call. That
  is a gap in the *process*, and it is the one most likely to repeat.
* **The journal target reaches deserialisation, not framing.** `Journal::read`'s
  own logic — the truncated-tail rule, the schema-version check, the "a bad line
  anywhere but the last is corruption" rule — is covered by unit tests and not by
  this. A target over a whole multi-line journal file would be strictly better.
* **No structure-aware fuzzing.** The targets take raw bytes and `&str`. Deriving
  `Arbitrary` for `JournalEntry` would let the fuzzer build well-formed entries
  and explore the *values* rather than spending most of its budget failing to
  produce valid JSON.
* **The toolchain is a nightly three months old** because the current one does
  not compile `nom-exif` 1.5.2, re-confirmed above. It is a dependency problem,
  not a fuzzing one, but it does mean the sanitiser in use is not the newest
  available — and the gap widens by a month every time this check is repeated
  and the answer is the same.
