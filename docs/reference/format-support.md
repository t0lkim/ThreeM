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
| `.dng` | TIFF/IFD | ❌ **filesystem only** | ❌ | The date *is* in the file, in an Exif `SubIFD`. `nom-exif` does not read a bare TIFF. |
| `.nef`, `.arw`, `.rw2`, `.raf`, `.srw`, `.pef`, `.orf`, `.raw` | TIFF/IFD (vendor variants) | ❌ **filesystem only** | ❌ | Same structure, same gap. |
| `.cr2` | TIFF/IFD + `CR\x02\x00` signature | ❌ **filesystem only** | ❌ | Verified as unreadable with a fixture carrying the Canon signature. |
| `.cr3` | ISO-BMFF (Canon) | ⚠️ untested | ⚠️ untested | A different container from CR2 despite the name. Canon stores EXIF in a custom `CMT1` box, which is not the HEIF `Exif` item, so it is unlikely to read. |
| `.tiff`, `.tif` | TIFF/IFD | ❌ **filesystem only** | ❌ | Same gap as the RAW families. |
| `.png` | PNG | ❌ **filesystem only** | ❌ | Not an EXIF container the parser knows. A PNG `eXIf` chunk is not read. |
| `.webp` | RIFF | ❌ **filesystem only** | ❌ | Not recognised. |
| `.bmp` | BMP | ❌ **filesystem only** | ❌ | Carries no date to read in any case. |

## Videos

| Extension | Container | Date source verified | GPS verified | Notes |
|---|---|---|---|---|
| `.mp4`, `.m4v` | ISO-BMFF, brands `mp42`/`isom`/… | ✅ `moov/mvhd` creation time | ✅ via `moov/udta/©xyz` (Android) — untested | `mvhd` is a UTC instant; see [Timezones](#timezones) below. |
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

## Timezones

Three different things a file can say about when it was made, and each is
treated differently. See `adr-006-timezone-handling.md` for the full resolution
order.

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
  parser would be needed. What *is* read is the `.xmp` sidecar beside it: a file
  with no usable embedded date takes `exif:DateTimeOriginal`,
  `photoshop:DateCreated` or `xmp:CreateDate` from its sidecar and is tagged
  `[SIDECAR]` — which for a darktable or Lightroom library covers the whole of
  the gap, since a RAW file must never be written into and so always has one.
  `--require-exif` admits a sidecar date for that reason, and remains the
  conservative posture for a RAW library without sidecars: every row marked
  *filesystem only* goes to `unsorted/` under its own filename rather than being
  filed under a modification time.

## Verifying this page

Every ✅ above corresponds to a test in `code/tests/metadata_formats.rs`, and
every ❌ in the RAW rows corresponds to a test asserting the *absence* — that the
file is reported as an unreadable format rather than silently dated from the
filesystem. The fixtures are synthesised byte by byte in
`code/tests/common/mod.rs`; there are no checked-in binary assets, so the matrix
can be re-verified offline on any machine.

The ⚠️ cells are honest gaps in this page, not necessarily in the tool: `.cr3`,
the Android `©xyz` GPS path and GPS in a `.3gp` have no fixture, so nothing above
is claimed about them.
