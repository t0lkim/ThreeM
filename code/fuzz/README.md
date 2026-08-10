# Fuzzing the parsers that read untrusted bytes

Five of `mmm`'s inputs are written by somebody else: a camera's EXIF date
strings, the offset tag beside them, a video container's ISO 6709 location
string, an XMP sidecar, and a journal line left on a disk by a previous run that
may have been power-cycled mid-write. Every one of them is parsed, and a parser
that panics part-way through a library leaves the library part-way organised.

The unit tests cover the shapes we thought of. These targets cover the ones we
did not.

| Target | Parser | What it is looking for |
|---|---|---|
| `parse_wall_clock` | `metadata::parse_wall_clock` | The five date spellings, and `chrono`'s strftime parser underneath them |
| `parse_offset` | `timezone::parse_offset` | An `OffsetTimeOriginal` tag interpolated into a datetime string, and an offset that will not survive being written back out |
| `parse_iso6709` | `metadata::parse_iso6709` | Hand-rolled byte-offset slicing, and coordinates that are not coordinates |
| `xmp_sidecar` | `xmp::parse` | Namespace resolution, entity unescaping and decoding over malformed RDF/XML |
| `journal_line` | `journal::parse_line` | That any byte sequence errors rather than panics — and that a parsed entry survives a round trip |

The entry points live in `src/fuzz.rs`. A fuzz target is a separate crate, so
the parsers it drives have to be reachable from outside the library; gathering
the five in one module says plainly what that visibility is for and keeps the
parsers themselves `pub(crate)`. `timezone::parse_offset` is the exception —
already `pub` for `metadata`'s benefit — and it is re-exported through the module
anyway, because a harness that reaches past `fuzz.rs` for one parser and through
it for the others leaves the next reader unsure which parsers are fuzzed.

## Running them

```sh
cd code
cargo +nightly-2026-05-01 fuzz run parse_iso6709 \
  fuzz/scratch/parse_iso6709 fuzz/corpus/parse_iso6709 -- -max_total_time=180
```

Two corpus directories, in that order, and the order is the point: libFuzzer
writes every new unit it discovers into the **first** one. `scratch/` is
gitignored and takes the machine-generated growth; `corpus/` is checked in and
holds only what a person put there.

`cargo fuzz list` names the five targets, and the CI job loops over its output
rather than over a hard-coded list — so a target added here is fuzzed on the next
push with nothing else edited. `cargo fuzz build` compiles all of them without
running anything, which is the fastest check that a change to the library has not
broken the harness.

## The pinned nightly

`cargo-fuzz` needs a nightly toolchain — libFuzzer is reached through
`-Z sanitizer=address`, which stable does not have. The version is pinned to
**`nightly-2026-05-01`** rather than floating on `nightly`, because the current
nightly (1.99.0, 2026-08-07) does not compile `nom-exif` 1.5.2 at all:

```
error[E0308]: mismatched types
  --> nom-exif-1.5.2/src/exif/io.rs:65:45
   |   expected `std::ops::Range<usize>`, found `std::range::Range<usize>`
```

That is a change in what a range expression means on nightly, in a dependency,
and it has nothing to do with fuzzing — `cargo +nightly check` fails the same
way. `nom-exif` 1.5.2 is the last of the 1.x line, so there is no patch release
to take, and moving to 3.x is a major upgrade that would invalidate the
format-support matrix and the mutation-testing baseline, both of which are
measured against 1.x.

Unpin when either the dependency is upgraded or nightly settles. **Re-checked
2026-08-10** against nightly 1.99.0 (`969b803cb`, 2026-08-09): the same error at
the same line, and crates.io still shows 1.5.2 as the newest 1.x with the line
having moved on to 3.6.2. The pin holds. Record the date of each such check here
rather than re-deriving whether it is still needed.

## The corpus

`corpus/<target>/` is checked in and hand-curated. Two kinds of file live there:

* **Seeds** — one valid input per shape the parser accepts, named for the shape
  (`quicktime-offset`, `darktable-element-form`, `run-header`). They give the
  fuzzer somewhere to start; from a blank corpus it spends its budget
  rediscovering that a date has dashes in it.
* **Regressions** — an input that once crashed a parser, promoted out of
  `artifacts/` by hand and given a name that says what it was
  (`regression-longitude-past-antimeridian`). A crash input nobody can identify
  in a directory listing is not a regression test.

`artifacts/` and `scratch/` are gitignored. When a run crashes, libFuzzer writes
the offending input to `artifacts/<target>/crash-<sha1>`; fix the parser, then
copy the file into `corpus/<target>/` under a descriptive name so the fix stays
fixed.

## What has been found

`docs/research/fuzzing.md` records each run, its execution count, and every
defect found and fixed.
