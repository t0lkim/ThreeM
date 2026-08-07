//! How `ThreeM` spells the names it derives.
//!
//! Two rules live here, and they live here rather than in the modules that
//! apply them because more than one module applies each. Both were previously
//! implicit — one was a private helper in [`crate::geocoder`] that only the
//! location suffix went through, and the other was not written down anywhere at
//! all, which is why a year outside four digits produced a directory called
//! `-44`.

use std::ops::RangeInclusive;

/// The years `YYYY/MM/DD` can spell.
///
/// Not a plausibility judgement — that would be inventing policy, and a
/// scanned negative from 1890 is as real a photograph as one from last week.
/// It is the range the *format* can represent in four digits, which is the only
/// line the naming scheme itself draws.
///
/// Year 0 is inside it: `{:04}` renders it `0000`, and `0000:01:01 00:00:00` is
/// what a camera with a dead clock writes into EXIF. Filing those under
/// `0000/01/01` keeps the original date visible and the file findable, which is
/// more use to whoever goes looking than the alternative below.
pub const REPRESENTABLE_YEARS: RangeInclusive<i32> = 0..=9999;

/// Whether a year can be written as exactly four digits.
///
/// A year outside this range has no correct spelling in the scheme, and there
/// is no honest way to force one: truncating invents a date, clamping invents a
/// different one, and printing it as-is produces `-44/03/15` — a directory at
/// the top of somebody's photo library whose name every command-line tool reads
/// as a flag. Callers route these to `unsorted/` instead.
pub fn year_is_representable(year: i32) -> bool {
    REPRESENTABLE_YEARS.contains(&year)
}

/// Reduce arbitrary text to something safe to paste into a filename.
///
/// One character in, one character out — spaces become hyphens, anything that
/// is not alphanumeric, `-` or `_` becomes an underscore. The one-for-one part
/// matters: dropping characters instead could turn a location name into the
/// empty string, and a filename is being assembled around the result.
///
/// The characters this exists to stop are `/` and `\` (which end a path
/// component), `\0` (which ends the whole string as far as the OS is concerned)
/// and a leading `.` (which hides the file). All four are non-alphanumeric, so
/// all four become `_`.
///
/// Applied to the geocoded location suffix, which is arbitrary text from the
/// `GeoNames` dataset, and to the file extension, which is arbitrary text from
/// whatever the caller passed in.
pub fn sanitise_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else if c == ' ' {
                '-'
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitise_for_filename() {
        assert_eq!(sanitise_for_filename("New York-US"), "New-York-US");
        assert_eq!(sanitise_for_filename("São Paulo/BR"), "São-Paulo_BR");
    }

    /// The four characters the function exists for.
    #[test]
    fn test_sanitise_defuses_the_dangerous_characters() {
        assert_eq!(sanitise_for_filename("a/b"), "a_b");
        assert_eq!(sanitise_for_filename("a\\b"), "a_b");
        assert_eq!(sanitise_for_filename("a\0b"), "a_b");
        assert_eq!(sanitise_for_filename(".hidden"), "_hidden");
        assert_eq!(sanitise_for_filename(".."), "__");
    }

    #[test]
    fn test_representable_years_are_the_four_digit_ones() {
        assert!(year_is_representable(0), "0000 is four digits");
        assert!(year_is_representable(1));
        assert!(year_is_representable(2024));
        assert!(year_is_representable(9999));

        assert!(!year_is_representable(-1));
        assert!(
            !year_is_representable(-44),
            "the EXIF year chrono will parse"
        );
        assert!(!year_is_representable(10_000));
    }
}
