//! The parsers that read untrusted bytes, reachable from outside the crate.
//!
//! Four of this tool's inputs are not written by this tool. A media file's EXIF
//! date strings, its ISO 6709 location string, an XMP sidecar and a journal line
//! all arrive as bytes somebody else produced — a camera firmware, an exporter,
//! or the last run of `mmm` on a disk that has since been power-cycled. Every
//! one of them is parsed, and a parser that panics on a malformed input is a
//! run that dies part-way through moving a photo library.
//!
//! The unit tests exercise these with the shapes we thought of. The fuzz targets
//! under `code/fuzz/` exercise them with the shapes we did not, which is the
//! whole point — but a fuzz target is a separate crate, so it can only reach
//! what the library makes public.
//!
//! ## Why a module rather than making the parsers themselves `pub`
//!
//! Because they are not API. `parse_iso6709` has one caller and no business
//! being called by anything else; publishing it would make an internal helper
//! part of the crate's surface for the sake of a test harness. Gathering the
//! four here instead states plainly what this is for, keeps the internals
//! `pub(crate)` where they belong, and gives the harness a single place to look.
//!
//! Nothing here is a stable interface. It exists for `code/fuzz/`, and the
//! signatures follow whatever the parsers underneath them do.

use std::path::Path;

use anyhow::Result;
use chrono::{FixedOffset, NaiveDateTime};

use crate::journal::{JournalEntry, RunHeader};
use crate::xmp::SidecarDate;

/// The date strings a camera writes: EXIF `YYYY:MM:DD HH:MM:SS`, ISO 8601, and
/// RFC 3339 with the `QuickTime` colon-less offset spelling.
///
/// See [`crate::metadata::parse_wall_clock`].
pub fn parse_wall_clock(s: &str) -> Option<(NaiveDateTime, Option<FixedOffset>)> {
    crate::metadata::parse_wall_clock(s)
}

/// The ISO 6709 location string a video container carries, as
/// `+48.8577+002.295/`.
///
/// See [`crate::metadata::parse_iso6709`].
pub fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    crate::metadata::parse_iso6709(s)
}

/// An XMP sidecar, read from bytes rather than from a path.
///
/// The filesystem is not the interesting part — the parse is — so this hands the
/// bytes straight to the reader that [`crate::xmp::read_date`] would have opened
/// a file for.
pub fn xmp_date(bytes: &[u8]) -> Option<SidecarDate> {
    crate::xmp::parse(bytes, Path::new("<fuzz>"))
}

/// One journal header line, as [`crate::journal::Journal::read`] parses its
/// first line.
///
/// # Errors
///
/// Returns an error when the line is not valid UTF-8 or is not a `RunHeader`,
/// which for a fuzz target is the ordinary outcome and not a finding.
pub fn journal_header_line(line: &[u8]) -> Result<RunHeader> {
    crate::journal::parse_line(line)
}

/// One journal entry line, as [`crate::journal::Journal::read`] parses every
/// line after the first.
///
/// # Errors
///
/// As [`journal_header_line`] — a malformed line is the expected result, not a
/// crash.
pub fn journal_entry_line(line: &[u8]) -> Result<JournalEntry> {
    crate::journal::parse_line(line)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;

    /// Each wrapper reaches the parser it claims to, on one input whose answer
    /// is known. A re-export that quietly points at the wrong function would
    /// leave the fuzz targets running against nothing.
    #[test]
    fn every_entry_point_reaches_its_parser() {
        let (naive, offset) = parse_wall_clock("2024:01:15 14:30:00").unwrap();
        assert_eq!(naive.format("%Y-%m-%d").to_string(), "2024-01-15");
        assert_eq!(offset, None);

        let (lat, lon) = parse_iso6709("+48.8577+002.295/").unwrap();
        assert!((lat - 48.8577).abs() < 1e-6);
        assert!((lon - 2.295).abs() < 1e-6);

        let sidecar = xmp_date(
            br#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                                 xmlns:xmp="http://ns.adobe.com/xap/1.0/"
                                 xmp:CreateDate="2024-03-15T23:30:00+08:00"/>"#,
        )
        .unwrap();
        assert_eq!(
            sidecar.naive.format("%Y-%m-%d %H:%M").to_string(),
            "2024-03-15 23:30"
        );

        assert!(journal_header_line(b"{\"not\": \"a header\"}").is_err());
        assert!(journal_entry_line(b"not json at all").is_err());
    }
}
