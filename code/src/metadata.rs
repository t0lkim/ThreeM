use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use nom_exif::{parse_exif, parse_metadata, EntryValue, Exif, ExifTag};
use tracing::{debug, warn};

/// Extracted metadata from a media file
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub date: Option<DateTime<Utc>>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub date_source: DateSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DateSource {
    Exif,
    Filesystem,
    None,
}

/// Extract metadata from an image or video file
pub fn extract_metadata(path: &Path, is_video: bool) -> Result<FileMetadata> {
    if is_video {
        match extract_video_metadata(path) {
            Ok(meta) if meta.date.is_some() => return Ok(meta),
            Ok(_) => debug!(path = %path.display(), "video metadata found but no date"),
            Err(e) => {
                debug!(path = %path.display(), error = %e, "video metadata extraction failed")
            }
        }
    } else {
        match extract_image_metadata(path) {
            Ok(meta) if meta.date.is_some() => return Ok(meta),
            Ok(_) => debug!(path = %path.display(), "EXIF found but no date"),
            Err(e) => debug!(path = %path.display(), error = %e, "EXIF extraction failed"),
        }
    }

    extract_filesystem_metadata(path)
}

fn extract_image_metadata(path: &Path) -> Result<FileMetadata> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let iter =
        parse_exif(reader, None).with_context(|| format!("parsing EXIF for {}", path.display()))?;

    let iter = match iter {
        Some(i) => i,
        None => {
            return Ok(FileMetadata {
                date: None,
                latitude: None,
                longitude: None,
                date_source: DateSource::None,
            });
        }
    };

    // Collect into Exif struct for easy tag access
    let exif: Exif = iter.into();

    // Extract date
    let date = exif
        .get(ExifTag::DateTimeOriginal)
        .or_else(|| exif.get(ExifTag::CreateDate))
        .and_then(entry_to_datetime);

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

    Ok(FileMetadata {
        date,
        latitude,
        longitude,
        date_source: if date.is_some() {
            DateSource::Exif
        } else {
            DateSource::None
        },
    })
}

fn extract_video_metadata(path: &Path) -> Result<FileMetadata> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);

    let entries = parse_metadata(reader)
        .with_context(|| format!("parsing video metadata for {}", path.display()))?;

    let mut date: Option<DateTime<Utc>> = None;
    let mut latitude: Option<f64> = None;
    let mut longitude: Option<f64> = None;

    for (key, value) in &entries {
        match key.as_str() {
            "CreateDate" | "DateTimeOriginal" | "com.apple.quicktime.creationdate" => {
                if date.is_none() {
                    date = entry_to_datetime(value);
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

    Ok(FileMetadata {
        date,
        latitude,
        longitude,
        date_source: if date.is_some() {
            DateSource::Exif
        } else {
            DateSource::None
        },
    })
}

fn extract_filesystem_metadata(path: &Path) -> Result<FileMetadata> {
    let meta = fs::metadata(path)
        .with_context(|| format!("reading filesystem metadata for {}", path.display()))?;

    let date = meta
        .created()
        .ok()
        .or_else(|| meta.modified().ok())
        .map(DateTime::<Utc>::from);

    if date.is_none() {
        warn!(path = %path.display(), "no date available from filesystem");
    }

    Ok(FileMetadata {
        date,
        latitude: None,
        longitude: None,
        date_source: if date.is_some() {
            DateSource::Filesystem
        } else {
            DateSource::None
        },
    })
}

/// Convert an EntryValue to a DateTime<Utc>
fn entry_to_datetime(value: &EntryValue) -> Option<DateTime<Utc>> {
    match value {
        EntryValue::Time(dt) => Some(dt.with_timezone(&Utc)),
        EntryValue::Text(s) => parse_date_string(s),
        _ => None,
    }
}

/// Parse various date string formats
fn parse_date_string(s: &str) -> Option<DateTime<Utc>> {
    use chrono::NaiveDateTime;

    // EXIF standard: "YYYY:MM:DD HH:MM:SS"
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y:%m:%d %H:%M:%S") {
        return Some(dt.and_utc());
    }
    // ISO 8601
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc());
    }
    // RFC 3339
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
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
        .map(|i| {
            if i == 0 {
                // This is the sign of longitude itself, find the next one
                lon_part[1..]
                    .find(|c: char| ['+', '-'].contains(&c))
                    .map(|j| &lon_part[..j + 1])
                    .unwrap_or(lon_part)
            } else {
                &lon_part[..i]
            }
        })
        .unwrap_or(lon_part);

    let lat: f64 = lat_str.parse().ok()?;
    let lon: f64 = lon_str.parse().ok()?;
    Some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_date_string_exif() {
        let dt = parse_date_string("2024:01:15 14:30:00").unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-01-15");
    }

    #[test]
    fn test_parse_date_string_iso() {
        let dt = parse_date_string("2024-01-15T14:30:00").unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-01-15");
    }

    #[test]
    fn test_parse_date_string_rfc3339() {
        let dt = parse_date_string("2024-02-02T08:09:57+00:00").unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-02-02");
    }

    #[test]
    fn test_parse_date_string_invalid() {
        assert!(parse_date_string("not a date").is_none());
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

    #[test]
    fn test_filesystem_fallback() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"test data").unwrap();

        let meta = extract_filesystem_metadata(tmp.path()).unwrap();
        assert!(meta.date.is_some());
        assert_eq!(meta.date_source, DateSource::Filesystem);
    }
}
