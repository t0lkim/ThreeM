//! Which wall clock a photograph's timestamp is read against.
//!
//! EXIF `DateTimeOriginal` is a naive wall-clock reading: `2024:03:15 23:30:00`
//! and nothing else. It says what the camera's clock read, not what instant that
//! was. Treating those digits as UTC — which is what `and_utc()` does, and what
//! this tool used to do — files a photograph taken at half past eleven at night
//! in Singapore under the *following* day, and one taken at seven in the morning
//! in Los Angeles under the *previous* one. Nothing about that is visible to the
//! person running the tool; the file simply appears in the wrong directory.
//!
//! The fix is in two halves, and only the first is user-visible.
//!
//! **Filing uses the wall clock.** A naive reading is filed under exactly the
//! digits the camera wrote. That is deterministic — it does not depend on the
//! machine, its zone, or this module — and it is what somebody flipping through
//! `2024-03-15/` expects to find there.
//!
//! **The offset is recorded, and its provenance with it.** A timestamp is still
//! turned into a real instant, because a duplicate taken across a zone boundary
//! and a video whose container stores UTC both need one. Where that offset came
//! from is carried alongside as a [`TimezoneSource`], so `--verbose` and the
//! dry-run listing can say *assumed* where the tool assumed.
//!
//! [`TimezonePolicy`] is the resolution order: the file's own offset tag first,
//! then a configured `default_timezone`, then the machine's zone, then UTC.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, FixedOffset, LocalResult, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use thiserror::Error;

/// How the offset attached to a file's timestamp was decided.
///
/// Ordered by how much the file itself had to say: the first variant is read out
/// of the file, the last is an admission that nothing anywhere knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimezoneSource {
    /// The file recorded its own UTC offset — an EXIF `OffsetTimeOriginal` /
    /// `OffsetTime` tag, or a `QuickTime` `com.apple.quicktime.creationdate`.
    /// This is the only variant that is evidence rather than inference.
    ExifOffsetTag,

    /// An XMP sidecar beside the file stated the offset — see [`crate::xmp`].
    ///
    /// Evidence rather than inference, like [`Self::ExifOffsetTag`], and kept
    /// apart from it because the two were written by different things in
    /// different files. A run that reports `[tz:exif]` for an offset it read out
    /// of a text file next to the photograph is telling a small lie about the
    /// one thing this enum exists to be honest about.
    SidecarOffset,

    /// Derived from the file's GPS coordinates.
    ///
    /// **Nothing produces this yet.** Mapping a latitude and longitude to a
    /// timezone needs a boundary database — the reverse geocoder this tool
    /// already carries resolves place *names*, not zones — and pulling one in is
    /// a multi-megabyte dependency that has not been justified. The variant is
    /// declared because it is the next rung of the resolution order and callers
    /// that match on this enum should be written to expect it; see
    /// `docs/decisions/adr-006-timezone-handling.md`.
    GpsDerived,

    /// The `default_timezone` setting, or `--timezone`.
    ConfiguredDefault,

    /// The zone of the machine the run happened on.
    SystemLocal,

    /// Nothing said, and the machine's own zone could not be resolved either.
    /// Reachable only for a wall-clock reading that falls in a daylight-saving
    /// gap, where the local time being read never actually occurred.
    AssumedUtc,
}

impl TimezoneSource {
    /// The short form used in the dry-run listing, where a line has no room for
    /// a sentence.
    pub fn tag(self) -> &'static str {
        match self {
            Self::ExifOffsetTag => "exif",
            Self::SidecarOffset => "sidecar",
            Self::GpsDerived => "gps",
            Self::ConfiguredDefault => "config",
            Self::SystemLocal => "system",
            Self::AssumedUtc => "utc",
        }
    }

    /// The sentence form, for summaries and logs.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ExifOffsetTag => "the file's own offset tag",
            Self::SidecarOffset => "the offset in the file's XMP sidecar",
            Self::GpsDerived => "the file's GPS coordinates",
            Self::ConfiguredDefault => "the configured default_timezone",
            Self::SystemLocal => "this machine's timezone",
            Self::AssumedUtc => "UTC, assumed",
        }
    }

    /// Whether the offset was recorded rather than inferred.
    ///
    /// Everything else is the tool's inference, and a user auditing a run wants
    /// that line drawn. A sidecar's offset falls on the recorded side of it for
    /// the same reason its date does — see [`crate::metadata::DateSource::is_recorded`].
    pub fn came_from_the_file(self) -> bool {
        matches!(
            self,
            Self::ExifOffsetTag | Self::SidecarOffset | Self::GpsDerived
        )
    }
}

impl fmt::Display for TimezoneSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tag())
    }
}

/// A timezone somebody named, in either of the two forms they may name it.
///
/// The distinction is not cosmetic. A fixed offset is the same all year; an IANA
/// zone is a function from instant to offset, so `Asia/Singapore` and `+08:00`
/// agree while `Europe/Lisbon` and `+00:00` part company every March.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Timezone {
    /// A constant offset from UTC: `+08:00`, `-0530`, `Z`.
    Fixed(FixedOffset),
    /// An IANA zone name: `Asia/Singapore`, `Europe/Lisbon`, `UTC`.
    Named(Tz),
}

impl fmt::Display for Timezone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(offset) => write!(f, "{offset}"),
            Self::Named(tz) => f.write_str(tz.name()),
        }
    }
}

/// A `default_timezone` that is not one.
#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "`{value}` is not a timezone — write a fixed offset like `+08:00` or `-05:30`, or an IANA \
     zone name like `Asia/Singapore`"
)]
pub struct TimezoneError {
    pub value: String,
}

impl FromStr for Timezone {
    type Err = TimezoneError;

    /// Fixed offsets first, then IANA names.
    ///
    /// The order matters for exactly one input: `UTC` parses as an IANA zone and
    /// not as an offset, so it lands in [`Timezone::Named`] and prints back as
    /// `UTC` rather than as `+00:00`. `Z` is handled by the offset branch, since
    /// it is the ISO 8601 spelling rather than a zone name.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        let refuse = || TimezoneError {
            value: text.to_string(),
        };

        if trimmed.is_empty() {
            return Err(refuse());
        }

        if matches!(trimmed, "Z" | "z") {
            return Ok(Self::Fixed(Utc.fix()));
        }

        if let Some(offset) = parse_offset(trimmed) {
            return Ok(Self::Fixed(offset));
        }

        Tz::from_str(trimmed).map(Self::Named).map_err(|_| refuse())
    }
}

/// Parse `+08:00`, `+0800` or `+08` into an offset.
///
/// `chrono` has no public offset parser, so this goes through a whole datetime
/// with `%#z` — the parse-only specifier that accepts all three spellings — and
/// keeps the offset. A leading sign is required, which is what keeps
/// `Asia/Singapore` out of this branch.
///
/// Public because an EXIF `OffsetTimeOriginal` tag is exactly this and nothing
/// else — `+08:00` with a sign, never a zone name — so [`crate::metadata`] wants
/// the narrow parser rather than the whole of [`Timezone`].
pub fn parse_offset(text: &str) -> Option<FixedOffset> {
    if !text.starts_with(['+', '-']) {
        return None;
    }
    DateTime::parse_from_str(
        &format!("1970-01-01 00:00:00 {text}"),
        "%Y-%m-%d %H:%M:%S %#z",
    )
    .ok()
    .map(|dt| *dt.offset())
}

/// The resolution order a run applies when a file does not carry its own offset.
///
/// Constructed once per run from the settings and threaded through the metadata
/// extractor. It holds only what was *configured*; the machine's zone and UTC
/// are the built-in tail of the order and need nothing stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimezonePolicy {
    configured: Option<Timezone>,
}

impl TimezonePolicy {
    /// The policy for a run that configured a `default_timezone`, or one that
    /// did not.
    pub fn new(configured: Option<Timezone>) -> Self {
        Self { configured }
    }

    /// What `default_timezone` was set to, for reporting.
    pub fn configured(&self) -> Option<&Timezone> {
        self.configured.as_ref()
    }

    /// The offset a naive wall-clock reading should be understood in.
    ///
    /// **This does not change which directory the file is filed under.** Filing
    /// reads the wall clock, and attaching an offset to a wall clock leaves the
    /// wall clock alone — that is the whole point of doing it this way round.
    /// What the offset decides is the *instant* the run records, which is what
    /// makes two readings from different zones comparable.
    pub fn for_wall_clock(&self, naive: NaiveDateTime) -> (FixedOffset, TimezoneSource) {
        match &self.configured {
            Some(Timezone::Fixed(offset)) => (*offset, TimezoneSource::ConfiguredDefault),
            Some(Timezone::Named(tz)) => (
                local_offset(tz, naive)
                    .unwrap_or_else(|| tz.offset_from_utc_datetime(&naive).fix()),
                TimezoneSource::ConfiguredDefault,
            ),
            None => local_offset(&chrono::Local, naive)
                .map_or((Utc.fix(), TimezoneSource::AssumedUtc), |offset| {
                    (offset, TimezoneSource::SystemLocal)
                }),
        }
    }

    /// The offset a known instant should be *read* in.
    ///
    /// The counterpart of [`Self::for_wall_clock`] for a timestamp that is
    /// already unambiguous — an MP4 `mvhd` creation time, or a filesystem
    /// timestamp. Here the offset does move the file: an instant has to be
    /// converted to somebody's wall clock before it can name a directory, and
    /// converting it to UTC's is the same defect in a different disguise.
    pub fn for_instant(&self, instant: DateTime<Utc>) -> (FixedOffset, TimezoneSource) {
        let naive = instant.naive_utc();
        match &self.configured {
            Some(Timezone::Fixed(offset)) => (*offset, TimezoneSource::ConfiguredDefault),
            Some(Timezone::Named(tz)) => (
                tz.offset_from_utc_datetime(&naive).fix(),
                TimezoneSource::ConfiguredDefault,
            ),
            // Always defined: every instant has exactly one local reading, even
            // where a *local* reading may have none.
            None => (
                chrono::Local.offset_from_utc_datetime(&naive).fix(),
                TimezoneSource::SystemLocal,
            ),
        }
    }
}

/// The offset `tz` gives a local reading, where it gives one.
///
/// An ambiguous reading — the hour a zone repeats when the clocks go back —
/// takes the earlier of the two. A reading in the gap the clocks skip forward
/// over never occurred at all, and gets `None`; the callers above each say what
/// they do with that.
fn local_offset<T: TimeZone>(tz: &T, naive: NaiveDateTime) -> Option<FixedOffset> {
    match tz.offset_from_local_datetime(&naive) {
        LocalResult::Single(offset) | LocalResult::Ambiguous(offset, _) => Some(offset.fix()),
        LocalResult::None => None,
    }
}

/// Attach an offset to a wall clock without moving the wall clock.
///
/// `naive.and_utc().with_timezone(&offset)` would move it — that reads the
/// digits as UTC and then re-renders them somewhere else, which is the original
/// bug written in a longer form. This keeps `dt.naive_local() == naive` for
/// every offset, which is the invariant filing depends on.
pub fn attach_offset(naive: NaiveDateTime, offset: FixedOffset) -> DateTime<FixedOffset> {
    let utc = naive - chrono::TimeDelta::seconds(i64::from(offset.local_minus_utc()));
    DateTime::from_naive_utc_and_offset(utc, offset)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate, Timelike};

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    fn offset(hours: i32) -> FixedOffset {
        FixedOffset::east_opt(hours * 3600).unwrap()
    }

    // -----------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------

    #[test]
    fn a_fixed_offset_parses_in_all_three_spellings() {
        for text in ["+08:00", "+0800", "+08"] {
            assert_eq!(
                Timezone::from_str(text).unwrap(),
                Timezone::Fixed(offset(8)),
                "{text} is +08:00"
            );
        }
    }

    #[test]
    fn a_negative_offset_with_minutes_parses() {
        assert_eq!(
            Timezone::from_str("-05:30").unwrap(),
            Timezone::Fixed(FixedOffset::east_opt(-(5 * 3600 + 30 * 60)).unwrap())
        );
    }

    #[test]
    fn z_is_utc() {
        assert_eq!(Timezone::from_str("Z").unwrap(), Timezone::Fixed(Utc.fix()));
    }

    #[test]
    fn an_iana_name_parses_and_prints_back_as_itself() {
        let tz = Timezone::from_str("Asia/Singapore").unwrap();
        assert_eq!(tz, Timezone::Named(Tz::Asia__Singapore));
        assert_eq!(tz.to_string(), "Asia/Singapore");
    }

    #[test]
    fn nonsense_is_refused_by_name() {
        for text in ["", "  ", "Mars/Olympus", "+99:99", "eight"] {
            let error = Timezone::from_str(text).expect_err("{text} is not a timezone");
            assert!(
                error.to_string().contains("is not a timezone"),
                "got {error}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Attaching an offset must not move the clock
    // -----------------------------------------------------------------

    /// The invariant the whole fix rests on. If this breaks, evening
    /// photographs start moving days again.
    #[test]
    fn attaching_an_offset_leaves_the_wall_clock_alone() {
        let wall = naive(2024, 3, 15, 23, 30);
        for hours in [-11, -5, 0, 8, 14] {
            let dt = attach_offset(wall, offset(hours));
            assert_eq!(dt.naive_local(), wall, "offset {hours} moved the clock");
            assert_eq!(dt.day(), 15);
            assert_eq!(dt.hour(), 23);
        }
    }

    /// And the instant it produces is the right one — the wall clock is not
    /// merely preserved, it is preserved *as a reading of that offset*.
    #[test]
    fn attaching_an_offset_produces_the_matching_instant() {
        let dt = attach_offset(naive(2024, 3, 15, 23, 30), offset(8));
        assert_eq!(dt.naive_utc(), naive(2024, 3, 15, 15, 30));
    }

    // -----------------------------------------------------------------
    // Resolution order
    // -----------------------------------------------------------------

    #[test]
    fn a_configured_fixed_offset_answers_a_wall_clock() {
        let policy = TimezonePolicy::new(Some(Timezone::Fixed(offset(8))));
        let (resolved, source) = policy.for_wall_clock(naive(2024, 3, 15, 23, 30));
        assert_eq!(resolved, offset(8));
        assert_eq!(source, TimezoneSource::ConfiguredDefault);
    }

    /// An IANA zone is resolved *at the reading*, which is the only reason to
    /// support names at all.
    #[test]
    fn a_configured_iana_zone_resolves_per_reading() {
        let policy = TimezonePolicy::new(Some(Timezone::Named(Tz::Europe__Lisbon)));

        let (winter, _) = policy.for_wall_clock(naive(2024, 1, 15, 12, 0));
        let (summer, _) = policy.for_wall_clock(naive(2024, 7, 15, 12, 0));

        assert_eq!(winter, offset(0), "Lisbon is on UTC in January");
        assert_eq!(summer, offset(1), "and an hour ahead in July");
    }

    /// A zone with no daylight saving gives the same answer either way, which
    /// makes it the one case a test can assert without knowing the machine.
    #[test]
    fn a_configured_iana_zone_answers_an_instant_too() {
        let policy = TimezonePolicy::new(Some(Timezone::Named(Tz::Asia__Singapore)));
        let (resolved, source) = policy.for_instant(naive(2024, 3, 15, 15, 30).and_utc());
        assert_eq!(resolved, offset(8));
        assert_eq!(source, TimezoneSource::ConfiguredDefault);
    }

    /// Nothing configured falls to the machine. The offset is whatever this
    /// machine is on — untestable — but the *source* is not, and neither is the
    /// promise that it never silently claims UTC.
    #[test]
    fn nothing_configured_falls_to_the_machine_and_says_so() {
        let policy = TimezonePolicy::default();
        let (_, source) = policy.for_instant(naive(2024, 3, 15, 15, 30).and_utc());
        assert_eq!(
            source,
            TimezoneSource::SystemLocal,
            "an instant always has a local reading, so this cannot degrade to UTC"
        );

        let (_, source) = policy.for_wall_clock(naive(2024, 3, 15, 23, 30));
        assert!(
            matches!(
                source,
                TimezoneSource::SystemLocal | TimezoneSource::AssumedUtc
            ),
            "a wall clock resolves against the machine, or admits it could not"
        );
    }

    /// A daylight-saving gap is the one path to `AssumedUtc` — a local reading
    /// that never happened. Asserted through a named zone, since the machine's
    /// own zone may have no gap at all.
    #[test]
    fn a_wall_clock_in_a_daylight_saving_gap_still_resolves() {
        // 02:30 on 31 March 2024 does not exist in Lisbon: the clocks go
        // straight from 01:00 to 02:00.
        let policy = TimezonePolicy::new(Some(Timezone::Named(Tz::Europe__Lisbon)));
        let (resolved, source) = policy.for_wall_clock(naive(2024, 3, 31, 2, 30));
        assert_eq!(
            source,
            TimezoneSource::ConfiguredDefault,
            "the zone was configured; only the offset within it degraded"
        );
        assert!(
            resolved == offset(0) || resolved == offset(1),
            "got {resolved}, which is neither side of the gap"
        );
    }

    #[test]
    fn the_source_labels_separate_evidence_from_inference() {
        assert!(TimezoneSource::ExifOffsetTag.came_from_the_file());
        for inferred in [
            TimezoneSource::ConfiguredDefault,
            TimezoneSource::SystemLocal,
            TimezoneSource::AssumedUtc,
        ] {
            assert!(!inferred.came_from_the_file(), "{inferred} is an inference");
        }
    }
}
