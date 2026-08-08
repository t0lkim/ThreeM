---
type: decision
title: Timezone handling
created: 2026-08-08
tags:
  - timezone
  - exif
  - metadata
  - breaking-change
related:
  - '[[format-support]]'
  - '[[configuration]]'
  - '[[USER-GUIDE]]'
  - '[[CHANGELOG]]'
---

# ADR-006: Local wall-clock filing, and where an offset comes from

**Status:** Accepted
**Date:** 2026-08-08

## Problem

`parse_date_string` called `and_utc()` on an EXIF `DateTimeOriginal`, and every dated path was then derived from the resulting instant. `DateTimeOriginal` is not an instant. It is nineteen characters of wall clock — `2024:03:15 23:30:00` — recording what the camera's own display read, with nothing anywhere in the tag about where the camera was standing.

Reading those digits as UTC and then rendering them somewhere else moves them. A photograph taken at half past eleven at night in Singapore was filed under `2024-03-16/` and renamed `2024-03-16-153000.jpg`; one taken at seven in the morning in Los Angeles went to the previous day. Nothing about this was visible to the person running the tool — no warning, no flag, no failure. The photograph simply appeared in the wrong directory, under a filename stamped with an hour it was not taken at, and the only way to notice was to know what the answer should have been.

Fixing it means settling three questions, none of which the old code asked:

1. **What kind of thing is a timestamp?** An EXIF wall clock, an Apple `com.apple.quicktime.creationdate` and an MP4 `mvhd` box are three different amounts of knowledge, and the old code held all three in a `DateTime<Utc>` — a type that can only represent the third.
2. **Which of them names the directory?** A directory name is a *local* fact: somebody flipping through `2024-03-15/` expects to find what they photographed that day, in the place they were standing.
3. **Where does an offset come from when the file did not state one?** Something has to answer, most files do not carry an `OffsetTime*` tag, and whatever answers is guessing — so the guess has to be visible.

## Decision

**Filing reads the local wall clock. The offset is resolved separately, recorded, and reported with its provenance.**

### The wall clock names the directory

`FileMetadata.date` is a `DateTime<FixedOffset>` whose *local* reading is the one the file recorded, produced by `timezone::attach_offset`:

```rust
pub fn attach_offset(naive: NaiveDateTime, offset: FixedOffset) -> DateTime<FixedOffset> {
    let utc = naive - chrono::TimeDelta::seconds(i64::from(offset.local_minus_utc()));
    DateTime::from_naive_utc_and_offset(utc, offset)
}
```

The invariant is `dt.naive_local() == naive` for every offset, and it is asserted in both `timezone.rs` and `metadata.rs`. Attaching an offset to a wall clock does not move the wall clock — `naive.and_utc().with_timezone(&offset)` does, and that is the original defect written out in a longer form.

Two consequences fall out of this, and they are the reason for doing it this way round rather than storing an instant and converting late. A naive EXIF reading is filed **under exactly the digits the camera wrote, on every machine** — the output tree does not depend on where the run happened, on `$TZ`, or on `--timezone`. And no code on the path from a reading to a destination converts to UTC, so there is no second place for the bug to come back.

### Three readings, kept apart

An internal `Reading` enum in `metadata.rs` holds the distinction the old `DateTime<Utc>` erased:

| Variant | What it is | How it resolves |
|---|---|---|
| `WallClock(NaiveDateTime)` | EXIF `DateTimeOriginal` with no offset tag; an XMP date with no offset | Offset resolved by the order below and *attached*. The digits do not move. |
| `Zoned(DateTime<FixedOffset>)` | `OffsetTimeOriginal`, an XMP offset, an Apple `creationdate` | Believed as-is. The file is the witness. |
| `Instant(DateTime<Utc>)` | `moov/mvhd`, a filesystem timestamp | Converted to the run's zone before it names anything. An instant has no wall clock of its own, and rendering it in UTC is the same defect in a different disguise. |

The video path is where this earns its keep. A real iPhone recording carries **both** an `mvhd` box (UTC by specification, always) and a `com.apple.quicktime.creationdate` string (local, with the offset the phone was standing in), and they disagree by exactly the phone's distance from Greenwich. The old code raced them and took whichever the parser yielded first. `creationdate` now wins, because it is the only one of the two that knows where the camera was.

### The resolution order

For a `WallClock` reading, in order, each recorded as a `TimezoneSource` and reported:

1. **The file's own offset** — EXIF `OffsetTimeOriginal` (0x9011), else `OffsetTime` (0x9010); an offset stated in an XMP sidecar; an Apple `creationdate`. Reported `[tz:exif]`, or `[tz:sidecar]` for the sidecar. `OffsetTimeOriginal` is preferred because it belongs to the moment the shutter fired, which is the moment `DateTimeOriginal` records, while `OffsetTime` is the camera's clock setting when the file was written — cameras that write both can disagree.
2. **`default_timezone` / `--timezone`** — `[tz:config]`.
3. **The machine's own zone** — `[tz:system]`.
4. **UTC** — `[tz:utc]`.

`TimezoneSource::GpsDerived` sits between 1 and 2 in the enum and **nothing produces it**; see Consequences.

Only steps 1 and 2 of that order change what the tool records; **none of the four changes where a naive reading is filed.** They decide the instant, which is what makes two photographs taken in different zones comparable, and what a duplicate check across a zone boundary needs.

### `nom-exif`'s offset is not evidence

The parser resolves `DateTimeOriginal` into a `DateTime<FixedOffset>` before `mmm` sees it — and when the file carries no `OffsetTime*` tag, it does so by applying **the machine's own timezone**, silently (`values.rs`, `Local.from_local_datetime`). An offset arriving that way is the run's own machine wearing the file's clothes.

So for images the parser's offset is discarded with `naive_local()` and re-resolved by the policy. That is not defensive tidying: without it `[tz:exif]` would be reported for the majority of files that state no offset at all, and the marker whose whole job is to say *assumed* where the tool assumed would be lying on most lines of the report. The offset tags are read directly rather than inferred from what the parser produced.

### IANA names, resolved per reading

`--timezone` and `default_timezone` accept a fixed offset (`+08:00`, `-05:30`, `+0800`, `Z`) or an IANA zone name (`Asia/Singapore`, `Europe/Lisbon`), the latter via `chrono-tz`. A name is resolved **per reading**, not once per run: a fixed offset is simply wrong for half the year anywhere that observes daylight saving, so `Europe/Lisbon` gives `+00:00` in January and `+01:00` in July for the readings that fall there.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Keep `DateTime<Utc>`, convert to local only when rendering a path** | The directory a photograph lands in would then depend on the machine that ran the tool. The same library organised on a laptop in Lisbon and a server in Singapore would produce two different trees from the same files, and neither would be wrong in a way anybody could point at. |
| **Store a bare `NaiveDateTime` and drop the offset entirely** | Filing would be correct and everything else would lose. Comparing two readings taken in different zones, and reading an `mvhd` instant at all, both need a real instant; so does anything later that wants to sort a library chronologically across a move. The wall clock is what names the directory, not what the tool should be able to know. |
| **Believe the offset `nom-exif` returns** | It is the machine's zone for every file with no offset tag, applied silently and indistinguishably from a real one. This is the same defect as `and_utc()` with a different constant, and it would additionally *report* itself as the file's own testimony. |
| **Fixed offsets only; skip `chrono-tz`** | `--timezone +00:00` is wrong in London for seven months of the year, and the people most likely to reach for the flag are the ones organising a library shot somewhere they no longer are. A zone name is what a person actually knows about where they were. |
| **Assume UTC when nothing else answers, with no system-local step** | Reintroduces the original bug wherever it still bites: an `mvhd` clock and a filesystem timestamp are genuine instants, and rendering them in UTC on a machine that is not on UTC files them under the wrong day. The machine's zone is a guess, but it is a guess that is right for the common case of organising your own photographs where you took them. |
| **Believe an Apple `creationdate` of exactly `+00:00`** | The parser hands that over indistinguishably from the `mvhd` clock, which is *always* `+00:00`. Believing it would treat every mp4 in existence as a recording made in the UTC zone. Only a non-zero offset proves the file wrote it — a deliberate bet, and the residual gap is recorded in [`format-support`](../reference/format-support.md). |
| **Let `--timezone` change which day a naive reading is filed under** | It would make the output tree a function of a flag, which is exactly the property this ADR exists to remove. A camera clock reading 23:30 means 23:30 wherever it is read; a run that could re-file it under a different day on the strength of a command-line argument is one whose directories mean nothing in particular. |
| **Derive the zone from GPS coordinates** | Correct, and the right next rung — but it needs a timezone *boundary* database. The reverse geocoder already bundled resolves place names, not zones, and a coordinates-to-zone dataset is several megabytes for a refinement that does not change where a single file is filed. Declared as a variant, not implemented. |
| **Ask the user per file, or fail on an ambiguous date** | A library is tens of thousands of files. The answer is to make the assumption visible — a `[tz:…]` marker per line and a tally in the summary — not to make it interactive. |

## Consequences

- **This changes output paths, and there is no migration.** A library organised by an earlier build on a machine that was not on UTC has files under the wrong dates and filenames stamped with the wrong times. Re-running does not move them: the correct path is a *different* path, so a second run creates it alongside the first. `mmm undo` the old run if a journal exists for it, or move the files by hand. Runs made on a UTC machine, and files carrying an `OffsetTimeOriginal` tag, are unaffected.
- **The same fix applied to the video path a second time.** An Apple `creationdate` was being converted to an instant and re-read against the run's zone, so the identical `.mov` filed at `2024-03-15-233000` in Singapore and `2024-03-15-153000` in London — the opening defect surviving where the offset arrived already applied rather than in a separate tag. Its output paths change too.
- **`--timezone` needs `allow_hyphen_values`.** Every offset west of Greenwich starts with a hyphen, and without it `--timezone -05:30` was refused as an unknown flag `-0` — the flag worked for half the world. The cost, accepted: `--timezone` immediately followed by another flag takes that flag as its value, so `mmm --timezone --commit ~/Photos` fails with "`--commit` is not a timezone".
- **`TimezoneSource::GpsDerived` is declared and never constructed.** A caller matching on the enum should expect it; a report will never show `[tz:gps]`. Filing is unaffected either way — a naive reading is filed under its own wall clock regardless — but the *instant* recorded for a file that carries coordinates and no offset tag may be wrong by the difference between the two zones.
- **`TimezoneSource::AssumedUtc` is reachable only through a daylight-saving gap**, where the local time being read never actually occurred and the machine's own zone therefore has no offset to give. An ambiguous reading — the hour a zone repeats when the clocks go back — takes the earlier of the two rather than failing.
- **The timezone tally is preview-only.** Per-file `[tz:…]` markers appear in the dry-run listing; the breakdown by source is printed there and not in the closing summary, so a committing run does not report it. The date-*source* counts were moved into the summary and the timezone counts were not — the same gap one dimension over, stated here rather than left to be discovered.
- **A run's zone is visible without `--verbose`.** Every line of the dry-run listing carries its `[tz:…]` marker beside the date-source tag, and `-vv` logs the resolved local time, offset and source per file. A run that fell back to the machine's zone for three thousand files says so once, plainly.
