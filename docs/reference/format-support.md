---
type: reference
title: Format Support Matrix
created: 2026-08-08
tags:
  - formats
  - exif
  - raw
  - video
  - heic
related:
  - '[[adr-006-timezone-handling]]'
  - '[[configuration]]'
  - '[[USER-GUIDE]]'
  - '[[CHANGELOG]]'
---

# Format Support Matrix

`mmm` accepts thirty-two file extensions. It can read an embedded date out of
four container families. Those are not the same set, and the gap between them is
what this page exists to state.

Where a date cannot be read, `mmm` does **not** fail — it falls back to the
file's filesystem timestamp and organises it anyway. That is usually the right
behaviour and occasionally a disaster: on a library that has been copied between
disks, restored from a backup, or synced through a cloud service, the filesystem
timestamp is the date of the *copy*, not the date of the photograph. Since
version 0.1.0 the run says which files this happened to; before that it did not.

## Reading the table

- **Date source verified** — a synthesised fixture of that container was fed to
  the real extractor and the exact datetime written into it came back out.
- **Filesystem only** — the container is not one the EXIF parser recognises, so
  the date comes from the file's timestamps. Reported as
  `[FS: UNSUPPORTED]` per file and tallied in the summary.
- A container `mmm` *does* read, whose metadata will not parse — a truncated
  write, a corrupted card, a `DateTimeOriginal` that is not a datetime — is a
  third case, reported `[FS: UNREADABLE]` and tallied on its own line. It is not
  a property of the format, so it has no column here; it is a property of the
  individual file.
- **Untested** — the extension is accepted by the scanner, but no fixture of that
  container exists yet, so nothing here is a claim about it either way.

## Images

| Extension | Container | Date source verified | GPS verified | Notes |
|---|---|---|---|---|
| `.jpg`, `.jpeg` | JPEG (`APP1`/EXIF) | ✅ `DateTimeOriginal`, `OffsetTimeOriginal` | ✅ | The reference case. `CreateDate` is read when `DateTimeOriginal` is absent. |
| `.heic` | ISO-BMFF, brand `heic` | ✅ EXIF item via `meta`/`iinf`/`iloc` | ✅ | Every iPhone photograph since 2017. |
| `.heif` | ISO-BMFF, brand `mif1` | ✅ | ✅ (same path) | Accepted on the major brand or on a compatible brand. |
| `.avif` | ISO-BMFF, brand `avif` | ✅ | ✅ (same path) | Works because real AVIF files list `mif1` as a compatible brand. An AVIF that lists neither would be filesystem-only. |
| `.dng` | TIFF/IFD | ❌ **filesystem only** — ✅ from an `.xmp` beside it | ❌ | The date *is* in the file, in an Exif `SubIFD`. `nom-exif` does not read a bare TIFF. |
| `.nef`, `.arw`, `.rw2`, `.raf`, `.srw`, `.pef`, `.orf`, `.raw` | TIFF/IFD (vendor variants) | ❌ **filesystem only** — ✅ from an `.xmp` beside it | ❌ | Same structure, same gap. |
| `.cr2` | TIFF/IFD + `CR\x02\x00` signature | ❌ **filesystem only** — ✅ from an `.xmp` beside it | ❌ | Verified as unreadable with a fixture carrying the Canon signature. |
| `.cr3` | ISO-BMFF (Canon), major brand `crx ` | ⚠️ untested — and the outcome is genuinely open | ⚠️ untested | A different container from CR2 despite the name. Canon puts EXIF in a custom `CMT1` box, not the HEIF `Exif` item, so *EXIF* will not be read. But `crx ` is in none of the parser's brand lists while `isom` — which a CR3 lists as a compatible brand — is, so it may be taken for an MP4 and dated from `moov/mvhd` instead. Reading the parser cannot settle which; only a fixture can. |
| `.tiff`, `.tif` | TIFF/IFD | ❌ **filesystem only** | ❌ | Same gap as the RAW families. |
| `.png` | PNG | ❌ **filesystem only** | ❌ | Not an EXIF container the parser knows. A PNG `eXIf` chunk is not read. |
| `.webp` | RIFF | ❌ **filesystem only** | ❌ | Not recognised. |
| `.bmp` | BMP | ❌ **filesystem only** | ❌ | Carries no date to read in any case. |

**The RAW rows have a second answer, and for a darktable or Lightroom library it
is the one that applies.** The date column above is about what is *inside* the
file. A file with no readable date of its own takes one from the `.xmp` sidecar
beside it — which is where the RAW families' dates actually live, because a RAW
must never be written into and so every edited frame has one. Such a file is
tagged `[SIDECAR]`, not `[FS: UNSUPPORTED]`, and `--require-exif` admits it. See
[Sidecar dates](#sidecar-dates) below.

## Videos

| Extension | Container | Date source verified | GPS verified | Notes |
|---|---|---|---|---|
| `.mp4`, `.m4v` | ISO-BMFF, brands `mp42`/`isom`/… | ✅ `moov/mvhd` creation time | ⚠️ untested — the Android `moov/udta/©xyz` path has no fixture | `mvhd` is a UTC instant; see [Timezones](#timezones) below. |
| `.mov` | QuickTime, brand `qt  ` | ✅ `moov/mvhd`, and `com.apple.quicktime.creationdate` when present | ✅ `com.apple.quicktime.location.ISO6709` | The Apple key wins over `mvhd`, because only it knows where the camera stood. |
| `.3gp` | ISO-BMFF, brand `3gp4` | ✅ `moov/mvhd` creation time | ⚠️ untested | Same parser path as MP4. |
| `.avi` | RIFF | ❌ **filesystem only** | ❌ | Not recognised. |
| `.mkv`, `.webm` | Matroska | ❌ **filesystem only** | ❌ | Not recognised. |
| `.wmv` | ASF | ❌ **filesystem only** | ❌ | Not recognised. |
| `.flv` | FLV | ❌ **filesystem only** | ❌ | Not recognised. |
| `.mts`, `.m2ts` | MPEG-2 transport stream | ❌ **filesystem only** | ❌ | AVCHD camcorder footage. The date lives in a sidecar or a stream descriptor, neither of which is read. |

## How "unsupported" is decided

Not from the extension. When a file yields no embedded date, `mmm` asks the EXIF
parser to identify the container and reports `Unsupported` only when it cannot.
That means the table above is a description of the tool's behaviour rather than a
list somebody has to remember to update: the day the parser learns TIFF, those
rows change on their own.

The consequence to be aware of is that a `.jpg` full of arbitrary bytes — a
truncated download, a renamed text file — is also reported `Unsupported`, because
it is not a container the tool can read. A JPEG that is genuinely a JPEG and
simply has no EXIF in it reports the ordinary filesystem fallback, and a JPEG
whose EXIF block is there and will not parse reports `Unreadable`. Three
outcomes, one directory: they are told apart in the report because they are not
told apart by the output tree.

## Sidecar dates

The date column is about what a container holds. When it holds nothing usable,
`mmm` reads the `.xmp` sidecar beside the file — `exif:DateTimeOriginal`, then
`photoshop:DateCreated`, then `xmp:CreateDate` — and tags the file `[SIDECAR]`.
Both RDF serialisations are read, and a property is matched on its namespace URI
under whatever prefix a writer chose.

This is the whole of the RAW answer for anyone using an editor: no TIFF-based RAW
is a container this tool reads, and every one of them that has been edited has an
`.xmp` beside it, because a RAW must never be written into. `--require-exif`
admits a sidecar date for that reason — it refuses *filesystem* timestamps, and a
date somebody's editor wrote down is not one.

What it will not do: override a date the file itself recorded, read anything but
a `.xmp` (an `.aae` is a binary property list, a `.thm` is a thumbnail with
metadata of its own), read coordinates, or accept a value coarser than a whole
day. A malformed sidecar is a warning and a skip.

For a RAW library with no sidecars, `--require-exif` is the conservative posture:
every row marked *filesystem only* above goes to `unsorted/` under its own
filename rather than being filed under a modification time.

## Timezones

Three different things a file can say about when it was made, and each is
treated differently. See
[`adr-006-timezone-handling`](../decisions/adr-006-timezone-handling.md) for the
full resolution order and the reasoning.

| The file says | Example | How it is read |
|---|---|---|
| A wall clock with no zone | EXIF `DateTimeOriginal`, no `OffsetTime*` tag | Filed under exactly those digits, on any machine. The offset is resolved for the *recorded instant* only. |
| A wall clock with its own offset | `OffsetTimeOriginal`, or `com.apple.quicktime.creationdate` | Believed, over `--timezone` and over the machine's zone. |
| An instant | `moov/mvhd`, or a filesystem timestamp | Converted to the run's zone before the directory is derived, because an instant has no wall clock of its own. |

### Known gaps

- **An Apple `creationdate` of exactly `+00:00` is read as an instant, not as a
  recording in the UTC zone.** The two arrive from the parser as the same value
  under the same key, and only a non-zero offset proves the file wrote it. The
  ambiguity costs a video shot in the UTC zone its wall clock when the run's zone
  is not UTC. The alternative — believing `+00:00` — would misfile *every* video
  that has only an `mvhd`, which is nearly all of them.
- **`TimezoneSource::GpsDerived` is declared and never produced.** Turning
  coordinates into a zone needs a boundary database; a place-name lookup is not
  one.
- **RAW dates are not read out of the RAW itself**, per the table above. A second
  parser would be needed. The `.xmp` beside it is read instead — see
  [Sidecar dates](#sidecar-dates).

## Verifying this page

Every ✅ above corresponds to a test, and every ❌ in the RAW rows corresponds to a
test asserting the *absence* — that the file is reported as an unsupported format
rather than silently dated from the filesystem.

**The remaining ❌ rows — PNG, WebP, BMP, TIFF, AVI, MKV, WebM, WMV, FLV,
MTS/M2TS — have no fixture of their own, and are derived rather than measured.**
The derivation is sound and worth stating so a reader can judge it: the parser
recognises exactly four containers, so a format that is none of them cannot be
read, and the tool decides that by asking the parser rather than by consulting a
list. A fixture per row would re-measure the same fact eleven times. What it
would additionally catch is a container being *mis*-recognised — a RIFF file
mistaken for something readable — which is the CR3 case one row up, and the
reason that row is ⚠️ rather than ❌.

The fixtures are synthesised byte
by byte in `code/tests/common/mod.rs`; there are no checked-in binary assets, so
the matrix can be re-verified offline on any machine, and it is re-verified under
`TZ=Europe/London` and `TZ=Pacific/Apia` as well as locally.

| Claim | Test |
|---|---|
| JPEG date, offset tag, GPS | `fixture_selftest::*`, `metadata_formats::an_offset_tag_files_an_evening_photograph_under_its_own_wall_clock` |
| HEIC date + GPS through the HEIF item indirection | `metadata_formats::a_heic_yields_its_exif_datetime_and_coordinates` |
| `heic` / `mif1` / `avif` brands | `metadata_formats::the_heif_family_is_read_under_each_brand_the_scanner_claims` |
| MP4, MOV and 3GP `mvhd` clocks | `metadata_formats::an_mp4_or_mov_container_clock_is_read_as_the_utc_instant_it_is` |
| Apple `creationdate` + ISO 6709 location | `metadata_formats::an_apple_creationdate_is_believed_over_the_runs_own_timezone` |
| `mvhd`-only video takes the run's zone | `metadata_formats::a_video_without_a_recording_offset_still_takes_the_runs_zone` |
| TIFF RAW (DNG, NEF, ARW, CR2) reported unsupported | `metadata_formats::a_tiff_based_raw_is_reported_as_unsupported_rather_than_silently_degrading` |
| …and the RAW fixtures are not merely malformed | `metadata_formats::the_raw_fixtures_carry_a_date_the_tool_would_read_in_any_other_container` |
| The three filesystem fallbacks are told apart | `metadata_formats::the_three_ways_of_falling_back_to_the_filesystem_are_told_apart` |
| A RAW dated from its sidecar | `sidecars::a_raw_takes_its_date_from_the_xmp_beside_it` |

The anti-vacuity control in the second RAW row matters: it asserts that the
identical EXIF block parses when it is put in a JPEG, so "the container is the
blocker" is a measurement rather than a story about a possibly-broken fixture.

The ⚠️ cells are honest gaps in this page, not necessarily in the tool: `.cr3`,
the Android `©xyz` GPS path and GPS in a `.3gp` have no fixture, so nothing above
is claimed about them.
