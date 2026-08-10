//! The offset tag a camera writes beside its date.
//!
//! `OffsetTimeOriginal` (EXIF 0x9011) and `OffsetTime` (0x9010) are strings in
//! the EXIF block, written by the same firmware that wrote `DateTimeOriginal`,
//! and they decide **which day a photograph is filed under**. A file shot at
//! 23:30 in Singapore is the 15th or the 16th depending on nothing but this
//! parser, so a wrong answer here is a photograph in the wrong directory rather
//! than a crash.
//!
//! It reaches `chrono` by an indirect route that is the reason this target
//! exists: `chrono` publishes no offset parser, so `parse_offset` splices the
//! text into a whole datetime string — `format!("1970-01-01 00:00:00 {text}")` —
//! and parses that under `%#z`. The input is therefore **interpolated into a
//! format string's subject**, which is a wider surface than the three spellings
//! the doc comment names.
//!
//! Takes `&str` for the same reason `parse_wall_clock` does: the caller hands it
//! a `String` that `nom-exif` already decoded out of the EXIF block.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Some(offset) = mmm::fuzz::parse_offset(text) else {
        return;
    };

    // The offset is attached to a naive reading to decide which calendar day the
    // photograph belongs to. `FixedOffset` is defined over ±24h, and a value
    // outside it would fail at attachment on something this parser just called
    // good — the same invariant `parse_wall_clock` holds for the offsets it
    // returns, asserted here because this parser reaches `chrono` by a different
    // route and could not inherit it.
    let seconds = chrono::Offset::fix(&offset).local_minus_utc();
    assert!(
        seconds.abs() < 86_400,
        "parsed an offset of {seconds}s from {text:?}"
    );

    // A parsed offset gets written back out — into the RFC 3339 timestamps the
    // journal records and the report prints — and anything written out has to be
    // readable again by the thing that undoes the run. Round-tripping through
    // `Display` is the narrowest statement of that: whatever this parser accepts
    // must survive being formatted and re-read as the same offset.
    assert_eq!(
        mmm::fuzz::parse_offset(&offset.to_string()),
        Some(offset),
        "offset from {text:?} did not survive a round trip through {offset}"
    );
});
