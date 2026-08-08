use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, Utc};
use nom_exif::{parse_exif, parse_metadata, EntryValue, Exif, ExifTag};
use tracing::{debug, warn};

use crate::timezone::{attach_offset, TimezonePolicy, TimezoneSource};

/// Extracted metadata from a media file
///
/// `date` carries an offset rather than being pinned to UTC, and the offset is
/// the *local* one — so `date.year()`, `date.day()` and `date.hour()` read the
/// wall clock a person would have seen on the camera. Everything downstream
/// derives directories and filenames from those accessors, which is what makes
/// an evening photograph stay on its own day. See [`crate::timezone`].
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub date: Option<DateTime<FixedOffset>>,
    /// How the offset on `date` was decided. `Some` exactly when `date` is.
    pub timezone_source: Option<TimezoneSource>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub date_source: DateSource,
}

impl FileMetadata {
    /// The empty answer: nothing in this file said when it was made.
    fn undated(date_source: DateSource) -> Self {
        Self {
            date: None,
            timezone_source: None,
            latitude: None,
            longitude: None,
            date_source,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DateSource {
    Exif,
    Filesystem,
    None,
}

/// What a file said about when it was made, before a policy is applied.
///
/// The three variants are three different amounts of knowledge, and conflating
/// them is the defect this type exists to prevent. Only [`Reading::WallClock`]
/// is naive; only [`Reading::Zoned`] is the file's own testimony about its
/// offset; [`Reading::Instant`] is unambiguous but says nothing about where the
/// camera was standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// A wall clock with no zone attached — an EXIF `DateTimeOriginal` with no
    /// `OffsetTime*` tag beside it. Filed under exactly these digits.
    WallClock(NaiveDateTime),
    /// A wall clock the file stamped with its own UTC offset.
    Zoned(DateTime<FixedOffset>),
    /// An instant with no local information at all — an MP4 `mvhd` creation
    /// time, which the container specifies as UTC, or a filesystem timestamp.
    Instant(DateTime<Utc>),
}

impl Reading {
    /// Turn a reading into a local datetime, and say how the offset was chosen.
    fn resolve(self, tz: &TimezonePolicy) -> (DateTime<FixedOffset>, TimezoneSource) {
        match self {
            Self::WallClock(naive) => {
                let (offset, source) = tz.for_wall_clock(naive);
                (attach_offset(naive, offset), source)
            }
            Self::Zoned(dt) => (dt, TimezoneSource::ExifOffsetTag),
            Self::Instant(utc) => {
                let (offset, source) = tz.for_instant(utc);
                (utc.with_timezone(&offset), source)
            }
        }
    }
}

/// Extract metadata from an image or video file
///
/// EXIF/video-container extraction is best-effort: if it fails or yields no
/// date, this falls back to filesystem timestamps.
///
/// `tz` decides only what happens when the file itself did not record an
/// offset; a file that did is believed over any configuration.
///
/// # Errors
///
/// Returns an error only if the filesystem fallback itself fails — i.e. the
/// file's own metadata cannot be read.
pub fn extract_metadata(path: &Path, is_video: bool, tz: &TimezonePolicy) -> Result<FileMetadata> {
    if is_video {
        match extract_video_metadata(path, tz) {
            Ok(meta) if meta.date.is_some() => return Ok(meta),
            Ok(_) => debug!(path = %path.display(), "video metadata found but no date"),
            Err(e) => {
                debug!(path = %path.display(), error = %e, "video metadata extraction failed");
            }
        }
    } else {
        match extract_image_metadata(path, tz) {
            Ok(meta) if meta.date.is_some() => return Ok(meta),
            Ok(_) => debug!(path = %path.display(), "EXIF found but no date"),
            Err(e) => debug!(path = %path.display(), error = %e, "EXIF extraction failed"),
        }
    }

    extract_filesystem_metadata(path, tz)
}

/// Assemble the answer for a file whose date was found, logging the resolution.
///
/// The log line is the `--verbose` surface the dry-run listing complements: at
/// `-vv` every file says which wall clock it was filed under and where that
/// decision came from, which is the only way to audit a run after the fact.
fn dated(
    path: &Path,
    reading: Reading,
    tz: &TimezonePolicy,
    latitude: Option<f64>,
    longitude: Option<f64>,
) -> FileMetadata {
    let (date, timezone_source) = reading.resolve(tz);
    debug!(
        path = %path.display(),
        local = %date.naive_local(),
        offset = %date.offset(),
        timezone = timezone_source.tag(),
        "resolved the timezone of a metadata date"
    );
    FileMetadata {
        date: Some(date),
        timezone_source: Some(timezone_source),
        latitude,
        longitude,
        date_source: DateSource::Exif,
    }
}

fn extract_image_metadata(path: &Path, tz: &TimezonePolicy) -> Result<FileMetadata> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let iter =
        parse_exif(reader, None).with_context(|| format!("parsing EXIF for {}", path.display()))?;

    let Some(iter) = iter else {
        return Ok(FileMetadata::undated(DateSource::None));
    };

    // Collect into Exif struct for easy tag access
    let exif: Exif = iter.into();

    // Extract GPS
    let (latitude, longitude) = match exif.get_gps_info() {
        Ok(Some(gps)) => {
            let lat = gps.latitude.0.as_float()
                + gps.latitude.1.as_float() / 60.0
                + gps.latitude.2.as_float() / 3600.0;
            let lon = gps.longitude.0.as_float()
                + gps.longitude.1.as_float() / 60.0
                + gps.longitude.2.as_float() / 3600.0;
            let lat = if gps.latitude_ref == 'S' { -lat } else { lat };
            let lon = if gps.longitude_ref == 'W' { -lon } else { lon };
            (Some(lat), Some(lon))
        }
        _ => (None, None),
    };

    // 0x9011 first, then 0x9010: `OffsetTimeOriginal` belongs to the moment the
    // shutter fired, which is the moment `DateTimeOriginal` records.
    // `OffsetTime` is the camera's clock setting at the time the file was
    // written, and cameras that write both can disagree between them.
    let offset_tag = exif
        .get(ExifTag::OffsetTimeOriginal)
        .or_else(|| exif.get(ExifTag::OffsetTime))
        .and_then(entry_to_offset);

    let Some(reading) = exif
        .get(ExifTag::DateTimeOriginal)
        .or_else(|| exif.get(ExifTag::CreateDate))
        .and_then(entry_to_wall_clock)
        .filter(|naive| spellable(naive.year()))
        .map(|naive| match offset_tag {
            Some(offset) => Reading::Zoned(attach_offset(naive, offset)),
            None => Reading::WallClock(naive),
        })
    else {
        return Ok(FileMetadata {
            latitude,
            longitude,
            ..FileMetadata::undated(DateSource::None)
        });
    };

    Ok(dated(path, reading, tz, latitude, longitude))
}

fn extract_video_metadata(path: &Path, tz: &TimezonePolicy) -> Result<FileMetadata> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let entries = parse_metadata(reader)
        .with_context(|| format!("parsing video metadata for {}", path.display()))?;

    // Two candidates, kept apart rather than raced. Apple's `creationdate` is a
    // string carrying the offset the phone was on — `2024-03-15T23:30:00+08:00`
    // — while `CreateDate` comes from the container's `mvhd` box, which the
    // specification pins to UTC. Preferring the first is preferring the only one
    // of the two that knows where the camera was.
    let mut recorded: Option<Reading> = None;
    let mut container: Option<Reading> = None;
    let mut latitude: Option<f64> = None;
    let mut longitude: Option<f64> = None;

    for (key, value) in &entries {
        match key.as_str() {
            "com.apple.quicktime.creationdate" => {
                if recorded.is_none() {
                    recorded = entry_to_reading(value);
                }
            }
            "CreateDate" | "DateTimeOriginal" => {
                if container.is_none() {
                    container = entry_to_reading(value);
                }
            }
            "com.apple.quicktime.location.ISO6709" => {
                if let EntryValue::Text(loc) = value {
                    if let Some((lat, lon)) = parse_iso6709(loc) {
                        latitude = Some(lat);
                        longitude = Some(lon);
                    }
                }
            }
            _ => {}
        }
    }

    let Some(reading) = recorded
        .or(container)
        .filter(|reading| spellable(reading_year(*reading)))
    else {
        return Ok(FileMetadata {
            latitude,
            longitude,
            ..FileMetadata::undated(DateSource::None)
        });
    };

    Ok(dated(path, reading, tz, latitude, longitude))
}

fn extract_filesystem_metadata(path: &Path, tz: &TimezonePolicy) -> Result<FileMetadata> {
    let meta = fs::metadata(path)
        .with_context(|| format!("reading filesystem metadata for {}", path.display()))?;

    // A filesystem timestamp is a genuine instant — but it still has to be read
    // against a wall clock before it can name a directory, and reading it
    // against UTC's is the same defect the EXIF path exists to avoid.
    let Some(instant) = meta
        .created()
        .ok()
        .or_else(|| meta.modified().ok())
        .map(DateTime::<Utc>::from)
    else {
        warn!(path = %path.display(), "no date available from filesystem");
        return Ok(FileMetadata::undated(DateSource::None));
    };

    let (date, timezone_source) = Reading::Instant(instant).resolve(tz);

    Ok(FileMetadata {
        date: Some(date),
        timezone_source: Some(timezone_source),
        latitude: None,
        longitude: None,
        date_source: DateSource::Filesystem,
    })
}

/// Whether a year can be written in the four digits the naming scheme has.
///
/// A date whose year cannot be is treated as no date at all, so the caller falls
/// through to the filesystem timestamp. `chrono` will parse `-0044:03:15
/// 10:00:00` out of an EXIF `DateTimeOriginal` without complaint, and a
/// photograph is better filed under the date its file was written than under a
/// year that is not a date. The alternative — letting it through for
/// [`crate::organiser::build_target_path`] to route to `unsorted/` — is correct
/// but lossy: everything in `unsorted/` is named `unknown.jpg` and the file's
/// own name is gone.
fn spellable(year: i32) -> bool {
    if crate::naming::year_is_representable(year) {
        return true;
    }
    warn!(
        year,
        "ignoring a metadata date whose year cannot be written in four digits"
    );
    false
}

/// The local year of a reading, for the check above.
///
/// A [`Reading::Instant`] is checked in UTC rather than after resolution, which
/// can differ by a day at the turn of a year. That is deliberate: the check
/// exists to catch a corrupt `0000`-or-negative year, and a boundary that moved
/// with the machine's zone would make "was this file rejected?" a question about
/// where the run happened.
fn reading_year(reading: Reading) -> i32 {
    match reading {
        Reading::WallClock(naive) => naive.year(),
        Reading::Zoned(dt) => dt.year(),
        Reading::Instant(utc) => utc.year(),
    }
}

/// The wall clock an EXIF datetime entry carries.
///
/// `nom-exif` has already turned `DateTimeOriginal` into a `DateTime<FixedOffset>`
/// by the time it reaches here — and if the file had no `OffsetTime*` tag it did
/// so by applying **the machine's own timezone**, silently. That offset is not
/// evidence about anything, so it is discarded and only the wall clock kept;
/// `naive_local()` returns exactly the digits the camera wrote. The caller
/// re-attaches an offset it can account for.
fn entry_to_wall_clock(value: &EntryValue) -> Option<NaiveDateTime> {
    match value {
        EntryValue::Time(dt) => Some(dt.naive_local()),
        EntryValue::Text(s) => parse_wall_clock(s).map(|(naive, _)| naive),
        _ => None,
    }
}

/// A video-container entry, keeping any offset it carried.
///
/// Unlike the EXIF path above, an offset here is real: it is either a string the
/// camera wrote with the offset in it, or an `mvhd` timestamp the container
/// specification defines as UTC.
fn entry_to_reading(value: &EntryValue) -> Option<Reading> {
    match value {
        EntryValue::Time(dt) => Some(Reading::Instant(dt.to_utc())),
        EntryValue::Text(s) => Some(match parse_wall_clock(s)? {
            (naive, Some(offset)) => Reading::Zoned(attach_offset(naive, offset)),
            (naive, None) => Reading::WallClock(naive),
        }),
        _ => None,
    }
}

/// An `OffsetTimeOriginal` / `OffsetTime` tag: `"+08:00"`, or `"-05:00"`.
///
/// The specification also allows all-spaces to mean "unknown", which parses as
/// nothing here and so falls through to the configured resolution — which is
/// what "unknown" should do.
fn entry_to_offset(value: &EntryValue) -> Option<FixedOffset> {
    let EntryValue::Text(text) = value else {
        return None;
    };
    let offset = crate::timezone::parse_offset(text.trim());
    if offset.is_none() {
        debug!(offset_tag = text, "ignoring an offset tag that is not one");
    }
    offset
}

/// Parse the date strings a media file may carry, and any offset in them.
fn parse_wall_clock(s: &str) -> Option<(NaiveDateTime, Option<FixedOffset>)> {
    // EXIF standard: "YYYY:MM:DD HH:MM:SS"
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S") {
        return Some((dt, None));
    }
    // ISO 8601 without a zone
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some((dt, None));
    }
    // RFC 3339, and the QuickTime spelling that omits the colon in the offset.
    // `%#z` accepts `+08:00`, `+0800` and `+08` alike.
    for pattern in ["%Y-%m-%dT%H:%M:%S%#z", "%Y-%m-%d %H:%M:%S%#z"] {
        if let Ok(dt) = DateTime::parse_from_str(s, pattern) {
            return Some((dt.naive_local(), Some(*dt.offset())));
        }
    }

    warn!(date_string = s, "unable to parse date");
    None
}

/// Parse ISO 6709 location string like "+48.8577+002.295/" or "+48.8577-002.295+35.6/"
fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let s = s.trim_end_matches('/');
    // Find the second +/- (start of longitude)
    let bytes = s.as_bytes();
    let mut split_pos = None;
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if b == b'+' || b == b'-' {
            split_pos = Some(i);
            break;
        }
    }

    let pos = split_pos?;
    let lat_str = &s[..pos];
    // Longitude may be followed by altitude
    let lon_part = &s[pos..];
    let lon_str: &str = lon_part
        .find(|c: char| ['+', '-'].contains(&c))
        .map_or(lon_part, |i| {
            if i == 0 {
                // This is the sign of longitude itself, find the next one
                lon_part[1..]
                    .find(|c: char| ['+', '-'].contains(&c))
                    .map_or(lon_part, |j| &lon_part[..=j])
            } else {
                &lon_part[..i]
            }
        });

    let lat: f64 = lat_str.parse().ok()?;
    let lon: f64 = lon_str.parse().ok()?;
    Some((lat, lon))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;

    use crate::timezone::Timezone;
    use std::str::FromStr as _;

    /// A run that was told which zone to assume, so the assertions below do not
    /// depend on the machine they run on.
    fn policy(name: &str) -> TimezonePolicy {
        TimezonePolicy::new(Some(Timezone::from_str(name).unwrap()))
    }

    #[test]
    fn test_parse_date_string_exif() {
        let (naive, offset) = parse_wall_clock("2024:01:15 14:30:00").unwrap();
        assert_eq!(naive.format("%Y-%m-%d").to_string(), "2024-01-15");
        assert_eq!(offset, None, "an EXIF datetime carries no zone");
    }

    #[test]
    fn test_parse_date_string_iso() {
        let (naive, offset) = parse_wall_clock("2024-01-15T14:30:00").unwrap();
        assert_eq!(naive.format("%Y-%m-%d").to_string(), "2024-01-15");
        assert_eq!(offset, None);
    }

    #[test]
    fn test_parse_date_string_rfc3339() {
        let (naive, offset) = parse_wall_clock("2024-02-02T08:09:57+00:00").unwrap();
        assert_eq!(naive.format("%Y-%m-%d").to_string(), "2024-02-02");
        assert_eq!(offset, FixedOffset::east_opt(0));
    }

    /// `QuickTime` writes the offset without a colon, and the whole point of
    /// reading it is that it is the camera's own testimony.
    #[test]
    fn test_parse_date_string_quicktime_offset() {
        for text in ["2024-03-15T23:30:00+0800", "2024-03-15T23:30:00+08:00"] {
            let (naive, offset) = parse_wall_clock(text).unwrap();
            assert_eq!(
                naive.format("%Y-%m-%d %H:%M").to_string(),
                "2024-03-15 23:30"
            );
            assert_eq!(offset, FixedOffset::east_opt(8 * 3600), "{text}");
        }
    }

    #[test]
    fn test_parse_date_string_invalid() {
        assert!(parse_wall_clock("not a date").is_none());
    }

    // -----------------------------------------------------------------
    // The defect this phase exists to fix
    // -----------------------------------------------------------------

    /// The regression in one line: a naive EXIF reading is filed under the
    /// digits the camera wrote, whatever offset the run resolves for it.
    ///
    /// Half past eleven at night on the 15th used to become half past three in
    /// the afternoon on the 15th *in UTC* — which is the same day here and the
    /// wrong one for anyone the other side of Greenwich. The property that makes
    /// it right everywhere is that the local reading never moves at all.
    #[test]
    fn a_naive_reading_keeps_its_wall_clock_whatever_the_policy_says() {
        let (naive, _) = parse_wall_clock("2024:03:15 23:30:00").unwrap();

        for zone in [
            "+08:00",
            "-11:00",
            "UTC",
            "Asia/Singapore",
            "America/Denver",
        ] {
            let (resolved, _) = Reading::WallClock(naive).resolve(&policy(zone));
            assert_eq!(
                resolved.format("%Y-%m-%d %H:%M").to_string(),
                "2024-03-15 23:30",
                "{zone} moved the wall clock"
            );
        }
    }

    /// The offset is not cosmetic even so: it is what makes the recorded instant
    /// right, and what two files from different zones are compared on.
    #[test]
    fn a_naive_reading_takes_the_configured_offset_for_its_instant() {
        let (naive, _) = parse_wall_clock("2024:03:15 23:30:00").unwrap();
        let (resolved, source) = Reading::WallClock(naive).resolve(&policy("+08:00"));

        assert_eq!(resolved.naive_utc().to_string(), "2024-03-15 15:30:00");
        assert_eq!(source, TimezoneSource::ConfiguredDefault);
    }

    /// The file's own testimony outranks the configuration, which is the whole
    /// reason the offset tags are read.
    #[test]
    fn an_offset_the_file_recorded_is_believed_over_the_configuration() {
        let (naive, offset) = parse_wall_clock("2024-03-15T23:30:00+08:00").unwrap();
        let reading = Reading::Zoned(attach_offset(naive, offset.unwrap()));

        let (resolved, source) = reading.resolve(&policy("America/Denver"));
        assert_eq!(resolved.format("%Y-%m-%d").to_string(), "2024-03-15");
        assert_eq!(resolved.offset().local_minus_utc(), 8 * 3600);
        assert_eq!(source, TimezoneSource::ExifOffsetTag);
    }

    /// An instant is the other half: it *has* to move to be read locally, and
    /// reading it in UTC is the original bug wearing a different hat. 15:30 UTC
    /// is half eleven at night in Singapore, so the day is the 15th either way —
    /// the hour is what proves the conversion happened.
    #[test]
    fn an_instant_is_read_against_the_configured_zone() {
        let instant = "2024-03-15T15:30:00+00:00";
        let (naive, offset) = parse_wall_clock(instant).unwrap();
        let reading = Reading::Instant(attach_offset(naive, offset.unwrap()).to_utc());

        let (resolved, source) = reading.resolve(&policy("Asia/Singapore"));
        assert_eq!(
            resolved.format("%Y-%m-%d %H:%M").to_string(),
            "2024-03-15 23:30"
        );
        assert_eq!(source, TimezoneSource::ConfiguredDefault);

        // And the day really does move for a zone far enough west.
        let (resolved, _) = reading.resolve(&policy("-11:00"));
        assert_eq!(resolved.format("%Y-%m-%d").to_string(), "2024-03-15");
        let (resolved, _) = reading.resolve(&policy("+14:00"));
        assert_eq!(resolved.format("%Y-%m-%d").to_string(), "2024-03-16");
    }

    /// An `OffsetTimeOriginal` tag is a string in the EXIF block, and a camera
    /// that writes the specification's "unknown" spelling must not be read as
    /// having said `+00:00`.
    #[test]
    fn an_offset_tag_is_read_when_it_is_one_and_ignored_when_it_is_not() {
        assert_eq!(
            entry_to_offset(&EntryValue::Text("+08:00".into())),
            FixedOffset::east_opt(8 * 3600)
        );
        assert_eq!(
            entry_to_offset(&EntryValue::Text("-05:30".into())),
            FixedOffset::east_opt(-(5 * 3600 + 30 * 60))
        );
        for unusable in ["   ", "", "not an offset", "08:00"] {
            assert_eq!(
                entry_to_offset(&EntryValue::Text(unusable.into())),
                None,
                "{unusable:?} is not an offset and must fall through to the policy"
            );
        }
    }

    #[test]
    fn test_parse_iso6709_basic() {
        let (lat, lon) = parse_iso6709("+48.8577+002.295/").unwrap();
        assert!((lat - 48.8577).abs() < 0.001);
        assert!((lon - 2.295).abs() < 0.001);
    }

    #[test]
    fn test_parse_iso6709_negative() {
        let (lat, lon) = parse_iso6709("-33.8688+151.2093/").unwrap();
        assert!((lat - (-33.8688)).abs() < 0.001);
        assert!((lon - 151.2093).abs() < 0.001);
    }

    /// `chrono` accepts these; the naming scheme cannot spell them.
    ///
    /// A year outside four digits has to be rejected *here* rather than left to
    /// the organiser, because `extract_metadata` only falls back to the
    /// filesystem timestamp when the EXIF date is `None`. Letting a year-44 date
    /// through means the file reaches `unsorted/` as `unknown.jpg` with its own
    /// name discarded, when a perfectly good filesystem date was available.
    #[test]
    fn test_an_unspellable_year_is_treated_as_no_date() {
        for s in ["-0044:03:15 10:00:00", "-0001-01-01T00:00:00"] {
            let (naive, _) = parse_wall_clock(s).expect("chrono parses these");
            assert!(
                !spellable(naive.year()),
                "{s} parses to a year that cannot be written in four digits, \
                 so it must not be offered as a date"
            );
        }
    }

    /// The other side of that line — the ones it must keep. Year 0 and year 44
    /// are a flat camera battery, not a missing date, and `0000-01-01` says so
    /// where `unsorted/unknown.jpg` would not.
    #[test]
    fn test_a_low_but_spellable_year_is_kept() {
        for s in ["0000:01:01 00:00:00", "0044:03:15 10:00:00"] {
            let (naive, _) = parse_wall_clock(s).expect("chrono parses these");
            assert!(
                spellable(naive.year()),
                "{s} is spellable in four digits and must be kept"
            );
        }
    }

    #[test]
    fn test_filesystem_fallback() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"test data").unwrap();

        let meta = extract_filesystem_metadata(tmp.path(), &policy("+08:00")).unwrap();
        assert!(meta.date.is_some());
        assert_eq!(meta.date_source, DateSource::Filesystem);
        assert_eq!(
            meta.timezone_source,
            Some(TimezoneSource::ConfiguredDefault),
            "a filesystem timestamp is still read against a wall clock, and the run has to \
             say which one"
        );
    }

    /// A dated file always says how its offset was chosen, and an undated one
    /// never claims to have chosen one.
    #[test]
    fn the_timezone_source_is_present_exactly_when_a_date_is() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"test data").unwrap();

        let dated = extract_filesystem_metadata(tmp.path(), &TimezonePolicy::default()).unwrap();
        assert_eq!(dated.date.is_some(), dated.timezone_source.is_some());

        let undated = FileMetadata::undated(DateSource::None);
        assert!(undated.date.is_none() && undated.timezone_source.is_none());
    }
}
