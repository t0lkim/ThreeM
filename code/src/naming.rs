//! How `ThreeM` spells the names it derives.
//!
//! Two rules live here, and they live here rather than in the modules that
//! apply them because more than one module applies each. Both were previously
//! implicit — one was a private helper in [`crate::geocoder`] that only the
//! location suffix went through, and the other was not written down anywhere at
//! all, which is why a year outside four digits produced a directory called
//! `-44`.
//!
//! Above those two rules sit the *formats* — [`DateDirectoryFormat`] and
//! [`FilenameFormat`], paired as a [`Scheme`] — which let a config file choose
//! the layout and the filename spelling. Both are parse-don't-validate types:
//! the only way to obtain one is through a constructor that has already refused
//! every pattern that could produce a path outside the output tree, so a
//! `Scheme` in hand is a scheme that cannot break containment. That is why they
//! live beside [`sanitise_for_filename`] rather than in `settings` — the safety
//! argument is a naming argument, and it is made once, here, for every caller.

use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, TimeZone, Utc};
use thiserror::Error;

/// The years `YYYY-MM-DD` can spell.
///
/// Not a plausibility judgement — that would be inventing policy, and a
/// scanned negative from 1890 is as real a photograph as one from last week.
/// It is the range the *format* can represent in four digits, which is the only
/// line the naming scheme itself draws.
///
/// Year 0 is inside it: `{:04}` renders it `0000`, and `0000:01:01 00:00:00` is
/// what a camera with a dead clock writes into EXIF. Filing those under
/// `0000-01-01` keeps the original date visible and the file findable, which is
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

/// The name given to a file whose format rendered to nothing at all.
///
/// The same word `unsorted/unknown.jpg` already uses, and for the same reason:
/// it is the tool's one admission that it has no name for a file. Reachable
/// only through a format made entirely of tokens that can all be empty — see
/// [`one_component`].
pub const UNNAMED: &str = "unknown";

/// A format string that was refused, and the reason it cannot be used.
///
/// Every variant names the key the reader has to go and edit and quotes the
/// pattern back at them, because the pattern is the thing they typed and a
/// message about "the format" would send them looking through four config
/// layers for which one.
///
/// None of these is a warning. A pattern that could put a file outside the
/// output tree, or leave every file without an extension, is refused at load
/// time rather than discovered halfway through moving somebody's library.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FormatError {
    /// An empty pattern: there would be nothing left to name the file or the
    /// directory.
    #[error("`{key}` is empty — there would be nothing left to name")]
    Empty { key: &'static str },

    /// A null byte, which ends the string as far as the operating system is
    /// concerned.
    #[error("`{key}` contains a null byte, which no filesystem will accept: {pattern:?}")]
    NullByte { key: &'static str, pattern: String },

    /// A directory pattern rooted at the filesystem, not at the output.
    #[error(
        "`{key}` must be relative to the output directory, and {pattern:?} is an absolute path — \
         it would file photographs at the root of the filesystem"
    )]
    Absolute { key: &'static str, pattern: String },

    /// A directory pattern that walks back out of the output tree.
    #[error(
        "`{key}` must not contain `..`, and {pattern:?} does — it would file photographs outside \
         the output directory"
    )]
    ParentDir { key: &'static str, pattern: String },

    /// A `%` that `strftime` does not recognise.
    #[error(
        "`date_directory_format` is not a valid strftime pattern: {pattern:?} — every `%` must be \
         followed by a known specifier (%Y, %m, %d, %H, %M, %S, %j, %B, %% and so on)"
    )]
    Strftime { pattern: String },

    /// A pattern that renders to nothing, so there would be no directory.
    #[error("`date_directory_format` produces no directory name at all: {pattern:?}")]
    Unrenderable { pattern: String },

    /// A path separator in a filename pattern.
    #[error(
        "`filename_format` must be a single filename and {pattern:?} contains a path separator — \
         the directories are `date_directory_format`'s job"
    )]
    Separator { pattern: String },

    /// A filename pattern that would strip every file's extension.
    #[error(
        "`filename_format` must contain {{ext}} and {pattern:?} does not — every organised file \
         would lose its extension"
    )]
    MissingExtension { pattern: String },

    /// A token [`Token::parse`] does not know.
    #[error(
        "`filename_format` uses the unknown token {{{token}}} in {pattern:?} — the tokens are \
         {{date}}, {{time}}, {{location}}, {{ext}} and {{original_stem}}"
    )]
    UnknownToken { pattern: String, token: String },

    /// A `{` with no `}` after it.
    #[error("`filename_format` has an unclosed `{{` in {pattern:?}")]
    UnclosedToken { pattern: String },

    /// A `}` with no `{` before it.
    #[error("`filename_format` has a `}}` with no `{{` before it in {pattern:?}")]
    UnopenedToken { pattern: String },

    /// A filename pattern that would hide every organised file.
    #[error(
        "`filename_format` must not begin with a dot and {pattern:?} does — every organised file \
         would be hidden"
    )]
    LeadingDot { pattern: String },
}

/// Refuse the characters no pattern of either kind may contain.
fn reject_shared(key: &'static str, pattern: &str) -> Result<(), FormatError> {
    if pattern.is_empty() {
        return Err(FormatError::Empty { key });
    }
    if pattern.contains('\0') {
        return Err(FormatError::NullByte {
            key,
            pattern: pattern.to_string(),
        });
    }
    Ok(())
}

/// Refuse a directory pattern that would not stay below the output tree.
///
/// The two shapes a person actually types when they mean to point somewhere
/// else — a leading `/` and a `..` — refused by name so the error says which
/// one it was. Everything that survives is still put through
/// [`sanitise_for_filename`] component by component before it becomes a path.
fn reject_escaping(key: &'static str, pattern: &str) -> Result<(), FormatError> {
    if pattern.starts_with('/') || Path::new(pattern).is_absolute() {
        return Err(FormatError::Absolute {
            key,
            pattern: pattern.to_string(),
        });
    }
    if pattern.contains("..") {
        return Err(FormatError::ParentDir {
            key,
            pattern: pattern.to_string(),
        });
    }
    Ok(())
}

/// Split a relative pattern into sanitised, non-empty path components.
///
/// The same reduction [`DateDirectoryFormat::render`] applies to a rendered
/// date, and for the same reason: whatever the string carries — spaces, colons,
/// a `\`, a leading dot — each component comes out as one ordinary name.
/// Doubled and trailing separators collapse rather than producing a nameless
/// directory.
fn sanitised_components(pattern: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for component in pattern.split('/').filter(|piece| !piece.is_empty()) {
        path.push(sanitise_for_filename(component));
    }
    path
}

/// A validated `strftime` pattern for the dated directory a file is filed under.
///
/// Slashes are the one piece of structure the pattern is allowed to carry:
/// `%Y-%m-%d` is one directory per day and `%Y/%m/%d` is a nested tree, and
/// choosing between those is the whole reason the setting exists. Everything
/// else that could make a path out of the result is refused up front — an
/// absolute pattern, a `..`, a null byte — and whatever survives is sanitised
/// component by component when it is rendered.
///
/// Both halves are load-bearing. The refusals catch the mistakes a person makes
/// while typing a config file, when there is still somebody there to read the
/// error; the sanitising catches what a *specifier* can expand to on a date
/// nobody thought about, which no amount of reading the pattern would reveal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateDirectoryFormat {
    pattern: String,
}

impl DateDirectoryFormat {
    /// Validate a pattern, or say why it cannot be one.
    ///
    /// # Errors
    ///
    /// [`FormatError`] for an empty pattern, a null byte, an absolute path, a
    /// `..`, an unknown `%` specifier, or a pattern that renders to nothing.
    pub fn new(pattern: &str) -> Result<Self, FormatError> {
        reject_shared("date_directory_format", pattern)?;
        reject_escaping("date_directory_format", pattern)?;

        if StrftimeItems::new(pattern).any(|item| matches!(item, Item::Error)) {
            return Err(FormatError::Strftime {
                pattern: pattern.to_string(),
            });
        }

        let format = Self {
            pattern: pattern.to_string(),
        };

        // Rendered once, here, against a date with nothing special about it.
        // A pattern that produces no directory at all is a config error and
        // belongs in this error message, not in a surprise at move time.
        if format.render(&sample_instant()).is_none() {
            return Err(FormatError::Unrenderable {
                pattern: pattern.to_string(),
            });
        }

        Ok(format)
    }

    /// The pattern as written, for `mmm config show` and for error messages.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The directory `dt` files under, or `None` if the pattern renders to
    /// nothing for this instant.
    ///
    /// The return is always a relative path of ordinary components. Splitting on
    /// `/` and sanitising each piece is what makes that true for *any* pattern
    /// that got past [`Self::new`], including one whose specifiers expand to
    /// something the pattern does not show — `%c` carries spaces and colons,
    /// `%D` carries slashes of its own. An empty piece is dropped, so a doubled
    /// or trailing separator collapses rather than producing a nameless
    /// directory.
    ///
    /// `None` rather than a substituted default: the caller
    /// ([`crate::organiser::build_target_path`]) routes those files to
    /// `unsorted/`, which is the bucket that already means "no filing we can
    /// trust", instead of inventing a directory nobody asked for.
    /// Generic over the timezone rather than pinned to UTC, because the
    /// datetime that reaches here is a *local* one — see [`crate::timezone`].
    /// `dt.format_with_items` reads the wall clock of whatever offset it
    /// carries, which is precisely what the dated directory has to spell.
    pub fn render<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> Option<PathBuf>
    where
        Tz::Offset: std::fmt::Display,
    {
        let mut rendered = String::new();
        // `write!` rather than `to_string`: `DelayedFormat`'s `Display` returns
        // an error for a pattern it cannot render, and `to_string` turns that
        // error into a panic. This module runs while somebody's photo library is
        // half-moved.
        write!(
            rendered,
            "{}",
            dt.format_with_items(StrftimeItems::new(&self.pattern))
        )
        .ok()?;

        let path = sanitised_components(&rendered);
        (path.components().count() > 0).then_some(path)
    }
}

/// A validated directory name below the output tree — `unsorted/`,
/// `duplicates/`, or whatever a config file renamed them to.
///
/// The same containment argument [`DateDirectoryFormat`] makes, for the two
/// directories that are chosen rather than rendered. It matters just as much:
/// `unsorted_dir = "/etc"` and `duplicates_dir = "../../elsewhere"` are one line
/// of config each, and both would file somebody's photographs outside the tree
/// they pointed the run at. Neither is a plausible typo, which is exactly why
/// the guarantee has to be structural rather than trusted — a `Layout` in hand
/// is the proof that both were checked.
///
/// Nested names are allowed (`"_review/undated"`): every component is sanitised
/// separately, so nesting costs nothing and refusing it would be a rule with no
/// reason behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSubdir {
    pattern: String,
    path: PathBuf,
}

impl OutputSubdir {
    /// Validate a directory name, or say why it cannot be one.
    ///
    /// `key` is the setting the reader has to go and edit, and appears in every
    /// error this returns.
    ///
    /// # Errors
    ///
    /// [`FormatError`] for an empty name, a null byte, an absolute path, a `..`,
    /// or a name whose components all sanitise away to nothing.
    pub fn new(key: &'static str, pattern: &str) -> Result<Self, FormatError> {
        reject_shared(key, pattern)?;
        reject_escaping(key, pattern)?;

        let path = sanitised_components(pattern);
        // Belt and braces: every non-empty relative string has at least one
        // component today, because a string of nothing but separators is
        // absolute and was refused above. If that ever stops being true, the
        // failure is an error rather than an empty path silently meaning "the
        // output directory itself".
        if path.components().count() == 0 {
            return Err(FormatError::Empty { key });
        }

        Ok(Self {
            pattern: pattern.to_string(),
            path,
        })
    }

    /// The name as written, for error messages.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The directory itself: always relative, always ordinary components.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The instant every pattern is trial-rendered against at construction.
///
/// Nothing distinguished about it beyond being a real date with a two-digit day,
/// month, hour, minute and second — 2024-03-15 10:10:45 UTC — so that a pattern
/// which renders for this one renders for the shapes the rest of the year takes.
///
/// The fallback is the epoch, which is also a perfectly good sample; it is here
/// only because `from_timestamp` is fallible and this module may not unwrap.
fn sample_instant() -> DateTime<Utc> {
    DateTime::from_timestamp(1_710_497_445, 0).unwrap_or(DateTime::UNIX_EPOCH)
}

/// One piece of a parsed [`FilenameFormat`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Text copied through as written.
    Literal(String),
    /// A `{token}` replaced with the file's own value.
    Token(Token),
}

/// A `{token}` in a filename pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Date,
    Time,
    Location,
    Ext,
    OriginalStem,
}

impl Token {
    /// The token this name spells, if it spells one.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "date" => Some(Self::Date),
            "time" => Some(Self::Time),
            "location" => Some(Self::Location),
            "ext" => Some(Self::Ext),
            "original_stem" => Some(Self::OriginalStem),
            _ => None,
        }
    }
}

/// What a single file supplies to a [`FilenameFormat`].
///
/// Every field is arbitrary text as far as this type is concerned — the
/// extension comes from a filename, the location from a geocoding dataset, the
/// stem from whatever the file was called — so [`FilenameFormat::render`]
/// sanitises all five rather than trusting any of them. Two of the five are
/// generated by the organiser and would survive untouched; sanitising them
/// anyway costs nothing and means the guarantee belongs to this function rather
/// than to the discipline of its callers.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilenameParts<'a> {
    /// `YYYY-MM-DD`.
    pub date: &'a str,
    /// `HHMMSS`.
    pub time: &'a str,
    /// The geocoded place, carrying its own leading separator, or empty.
    pub location: &'a str,
    /// The file's extension, without a dot.
    pub extension: &'a str,
    /// The file's name before the extension.
    pub original_stem: &'a str,
}

/// A validated token pattern for the name a file is given.
///
/// The tokens are `{date}`, `{time}`, `{location}`, `{ext}` and
/// `{original_stem}`, enumerated once in [`Token::parse`] and once in the error
/// a mistyped one produces. A pattern that is not one filename —
/// one carrying a path separator, or beginning with a dot — is refused, as is
/// one that drops `{ext}`, because a photo library whose files have lost their
/// extensions is a photo library no other program will open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilenameFormat {
    pattern: String,
    pieces: Vec<Piece>,
}

impl FilenameFormat {
    /// Parse a pattern, or say why it cannot be one.
    ///
    /// # Errors
    ///
    /// [`FormatError`] for an empty pattern, a null byte, a path separator, a
    /// leading dot, an unbalanced brace, an unknown token, or a missing
    /// `{ext}`.
    pub fn new(pattern: &str) -> Result<Self, FormatError> {
        reject_shared("filename_format", pattern)?;

        if pattern.contains('/') || pattern.contains('\\') {
            return Err(FormatError::Separator {
                pattern: pattern.to_string(),
            });
        }
        if pattern.starts_with('.') {
            return Err(FormatError::LeadingDot {
                pattern: pattern.to_string(),
            });
        }

        let pieces = Self::parse_pieces(pattern)?;

        if !pieces.contains(&Piece::Token(Token::Ext)) {
            return Err(FormatError::MissingExtension {
                pattern: pattern.to_string(),
            });
        }

        Ok(Self {
            pattern: pattern.to_string(),
            pieces,
        })
    }

    /// Split a pattern into literals and tokens, refusing anything else.
    fn parse_pieces(pattern: &str) -> Result<Vec<Piece>, FormatError> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut rest = pattern;

        while let Some(open) = rest.find('{') {
            let (before, from_brace) = rest.split_at(open);
            if before.contains('}') {
                return Err(FormatError::UnopenedToken {
                    pattern: pattern.to_string(),
                });
            }
            literal.push_str(before);

            let body = &from_brace[1..];
            let Some(close) = body.find('}') else {
                return Err(FormatError::UnclosedToken {
                    pattern: pattern.to_string(),
                });
            };
            let name = &body[..close];
            let Some(token) = Token::parse(name) else {
                return Err(FormatError::UnknownToken {
                    pattern: pattern.to_string(),
                    token: name.to_string(),
                });
            };

            if !literal.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(&mut literal)));
            }
            pieces.push(Piece::Token(token));
            rest = &body[close + 1..];
        }

        if rest.contains('}') {
            return Err(FormatError::UnopenedToken {
                pattern: pattern.to_string(),
            });
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }

        Ok(pieces)
    }

    /// The pattern as written, for `mmm config show` and for error messages.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The name `parts` is given.
    ///
    /// Always a single ordinary path component — see [`one_component`], which
    /// closes the two gaps validation cannot: a pattern whose tokens all happen
    /// to be empty for this one file, and a pattern whose first token is empty
    /// and exposes the dot behind it.
    pub fn render(&self, parts: &FilenameParts<'_>) -> String {
        let mut name = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => name.push_str(text),
                Piece::Token(token) => {
                    let value = match token {
                        Token::Date => parts.date,
                        Token::Time => parts.time,
                        Token::Location => parts.location,
                        Token::Ext => parts.extension,
                        Token::OriginalStem => parts.original_stem,
                    };
                    name.push_str(&sanitise_for_filename(value));
                }
            }
        }
        one_component(name)
    }
}

/// Make a rendered name into exactly one ordinary path component.
///
/// Two cases survive [`FilenameFormat::new`], because both depend on the *file*
/// rather than on the pattern:
///
/// * The name is empty. Only reachable when every token in the pattern rendered
///   empty — `{location}` for a photograph with no coordinates, `{ext}` for a
///   file with no extension. [`UNNAMED`] is the same answer `unsorted/unknown.jpg`
///   already gives.
/// * The name begins with a dot, which hides the file. A pattern beginning with
///   a literal dot is refused; one beginning with `{location}` is not, and
///   renders to a leading dot the moment a photograph has no coordinates. The
///   dot becomes `_`, which is what [`sanitise_for_filename`] does with every
///   other dangerous character.
fn one_component(name: String) -> String {
    if name.is_empty() {
        return UNNAMED.to_string();
    }
    match name.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => name,
    }
}

/// The pair of formats a run names its files with, plus whether locations are
/// spelled at all.
///
/// One type rather than three arguments threaded through the organiser, because
/// the three are decided together at startup and are then constant for the whole
/// run. Holding a `Scheme` is the proof that both patterns were validated:
/// there is no way to build one from a pattern that was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheme {
    date_directory: DateDirectoryFormat,
    filename: FilenameFormat,
    include_location: bool,
}

impl Scheme {
    /// Validate both patterns and pair them.
    ///
    /// # Errors
    ///
    /// The first [`FormatError`] either pattern produces.
    pub fn new(
        date_directory_format: &str,
        filename_format: &str,
        include_location: bool,
    ) -> Result<Self, FormatError> {
        Ok(Self {
            date_directory: DateDirectoryFormat::new(date_directory_format)?,
            filename: FilenameFormat::new(filename_format)?,
            include_location,
        })
    }

    /// The dated directory for `dt`, or `None` if there is none to give.
    pub fn date_directory<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> Option<PathBuf>
    where
        Tz::Offset: std::fmt::Display,
    {
        self.date_directory.render(dt)
    }

    /// The filename for `parts`.
    pub fn filename(&self, parts: &FilenameParts<'_>) -> String {
        self.filename.render(parts)
    }

    /// Whether a geocoded place name is spelled into filenames at all.
    ///
    /// Read by the organiser before the lookup rather than after it, so turning
    /// locations off also stops paying for them.
    pub fn include_location(&self) -> bool {
        self.include_location
    }
}

/// Everything about the shape of the output tree, validated together.
///
/// A [`Scheme`] says how a *dated* file is filed and named. A run also has to
/// answer two questions the scheme cannot: where a file with no usable date
/// goes, and where a duplicate goes. Both are settings, both are directories
/// below the output tree, and both are subject to the same containment rule as
/// the dated path — so they are validated by the same module and carried by one
/// value that the whole pipeline reads.
///
/// One type rather than three arguments threaded separately, and for the reason
/// the [`Scheme`] doc gives: they are decided together at startup and constant
/// for the run. Holding a `Layout` is the proof that every part of it was
/// checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    scheme: Scheme,
    unsorted: OutputSubdir,
    duplicates: OutputSubdir,
}

impl Layout {
    /// Pair a validated scheme with the two validated directories.
    pub fn new(scheme: Scheme, unsorted: OutputSubdir, duplicates: OutputSubdir) -> Self {
        Self {
            scheme,
            unsorted,
            duplicates,
        }
    }

    /// How dated files are filed and named.
    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    /// Where a file with no usable date goes, relative to the output.
    pub fn unsorted(&self) -> &Path {
        self.unsorted.path()
    }

    /// Where relocated duplicates are grouped, relative to the output.
    pub fn duplicates(&self) -> &Path {
        self.duplicates.path()
    }

    /// The dated directory for `dt`, or `None` if there is none to give.
    pub fn date_directory<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> Option<PathBuf>
    where
        Tz::Offset: std::fmt::Display,
    {
        self.scheme.date_directory(dt)
    }

    /// The filename for `parts`.
    pub fn filename(&self, parts: &FilenameParts<'_>) -> String {
        self.scheme.filename(parts)
    }

    /// Whether a geocoded place name is spelled into filenames at all.
    pub fn include_location(&self) -> bool {
        self.scheme.include_location()
    }
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

    // -----------------------------------------------------------------
    // date_directory_format
    // -----------------------------------------------------------------

    fn march() -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .expect("a real date")
            .and_hms_opt(10, 30, 0)
            .expect("a real time")
            .and_utc()
    }

    #[test]
    fn the_flat_and_the_nested_layouts_are_both_expressible() {
        assert_eq!(
            DateDirectoryFormat::new("%Y-%m-%d")
                .unwrap()
                .render(&march()),
            Some(PathBuf::from("2024-03-15"))
        );
        assert_eq!(
            DateDirectoryFormat::new("%Y/%m/%d")
                .unwrap()
                .render(&march()),
            Some(PathBuf::from("2024/03/15"))
        );
        assert_eq!(
            DateDirectoryFormat::new("%Y/%Y-%m")
                .unwrap()
                .render(&march()),
            Some(PathBuf::from("2024/2024-03"))
        );
    }

    /// The two refusals task 5 names, plus the two that make the type safe to
    /// hold at all.
    #[test]
    fn a_date_format_that_could_leave_the_output_tree_is_refused() {
        assert!(matches!(
            DateDirectoryFormat::new("/%Y/%m"),
            Err(FormatError::Absolute { .. })
        ));
        assert!(matches!(
            DateDirectoryFormat::new("%Y/../%m"),
            Err(FormatError::ParentDir { .. })
        ));
        assert!(matches!(
            DateDirectoryFormat::new(""),
            Err(FormatError::Empty { .. })
        ));
        assert!(matches!(
            DateDirectoryFormat::new("%Y\0%m"),
            Err(FormatError::NullByte { .. })
        ));
    }

    /// A `%` nobody recognises is a typo, and a typo that rendered itself
    /// verbatim would put `%Q` in the name of every directory in the library.
    #[test]
    fn an_unknown_strftime_specifier_is_refused() {
        assert!(matches!(
            DateDirectoryFormat::new("%Y/%Q"),
            Err(FormatError::Strftime { .. })
        ));
        assert!(matches!(
            DateDirectoryFormat::new("%Y/%"),
            Err(FormatError::Strftime { .. })
        ));
        assert!(
            DateDirectoryFormat::new("%Y%%%m").is_ok(),
            "%% is a literal"
        );
    }

    /// Not every specifier renders something. `%.f` is the fractional second,
    /// which prints nothing at all when the fraction is zero — so a pattern made
    /// only of it is a valid strftime pattern with no directory in it. Caught by
    /// the trial render, which is why there is one.
    #[test]
    fn a_date_format_that_renders_to_nothing_is_refused() {
        assert!(matches!(
            DateDirectoryFormat::new("%.f"),
            Err(FormatError::Unrenderable { .. })
        ));
        assert!(
            DateDirectoryFormat::new("%Y%.f").is_ok(),
            "the same specifier alongside one that does render is fine"
        );
    }

    /// What the pattern shows and what a specifier expands to are different
    /// questions. `%c` carries spaces and colons; the rendering is sanitised
    /// component by component, so neither reaches the filesystem.
    #[test]
    fn a_rendered_component_is_sanitised_whatever_the_specifier_expands_to() {
        let rendered = DateDirectoryFormat::new("%c")
            .unwrap()
            .render(&march())
            .unwrap();
        let name = rendered.to_string_lossy();
        assert!(!name.contains(' '), "got {name}");
        assert!(!name.contains(':'), "got {name}");
        assert_eq!(rendered.components().count(), 1);
    }

    /// A doubled or trailing separator collapses rather than producing a
    /// directory with no name.
    #[test]
    fn empty_components_collapse() {
        assert_eq!(
            DateDirectoryFormat::new("%Y//%m/")
                .unwrap()
                .render(&march()),
            Some(PathBuf::from("2024/03"))
        );
    }

    // -----------------------------------------------------------------
    // filename_format
    // -----------------------------------------------------------------

    fn parts<'a>(location: &'a str, extension: &'a str) -> FilenameParts<'a> {
        FilenameParts {
            date: "2024-03-15",
            time: "103000",
            location,
            extension,
            original_stem: "IMG_0001",
        }
    }

    #[test]
    fn the_default_pattern_spells_the_name_the_organiser_has_always_produced() {
        let format = FilenameFormat::new("{date}-{time}{location}.{ext}").unwrap();
        assert_eq!(
            format.render(&parts("-London-GB", "jpg")),
            "2024-03-15-103000-London-GB.jpg"
        );
        assert_eq!(format.render(&parts("", "jpg")), "2024-03-15-103000.jpg");
    }

    #[test]
    fn every_token_is_substituted() {
        let format = FilenameFormat::new("{original_stem}_{date}_{time}{location}.{ext}").unwrap();
        assert_eq!(
            format.render(&parts("-Oslo-NO", "png")),
            "IMG_0001_2024-03-15_103000-Oslo-NO.png"
        );
    }

    #[test]
    fn a_filename_format_that_is_not_one_filename_is_refused() {
        assert!(matches!(
            FilenameFormat::new("{date}/{time}.{ext}"),
            Err(FormatError::Separator { .. })
        ));
        assert!(matches!(
            FilenameFormat::new("{date}\\{time}.{ext}"),
            Err(FormatError::Separator { .. })
        ));
        assert!(matches!(
            FilenameFormat::new(".{date}.{ext}"),
            Err(FormatError::LeadingDot { .. })
        ));
        assert!(matches!(
            FilenameFormat::new(""),
            Err(FormatError::Empty { .. })
        ));
    }

    #[test]
    fn a_filename_format_without_the_extension_token_is_refused() {
        assert!(matches!(
            FilenameFormat::new("{date}-{time}"),
            Err(FormatError::MissingExtension { .. })
        ));
    }

    /// A mistyped token is the `deny_unknown_fields` argument one level down: a
    /// `{stem}` copied through verbatim would appear in the name of every file
    /// in the library, and the person who typed it would find out later.
    #[test]
    fn an_unknown_or_unbalanced_token_is_refused() {
        let error = FilenameFormat::new("{stem}-{date}.{ext}").unwrap_err();
        assert!(matches!(&error, FormatError::UnknownToken { token, .. } if token == "stem"));
        assert!(
            error.to_string().contains("{original_stem}"),
            "the message must name the tokens that do exist: {error}"
        );

        assert!(matches!(
            FilenameFormat::new("{date-{time}.{ext}"),
            Err(FormatError::UnknownToken { .. })
        ));
        assert!(matches!(
            FilenameFormat::new("{date}.{ext"),
            Err(FormatError::UnclosedToken { .. })
        ));
        assert!(matches!(
            FilenameFormat::new("date}.{ext}"),
            Err(FormatError::UnopenedToken { .. })
        ));
    }

    /// The two cases validation cannot reach, because both depend on the file
    /// rather than on the pattern.
    #[test]
    fn a_render_is_one_component_even_when_every_token_is_empty() {
        let bare = FilenameFormat::new("{location}{ext}").unwrap();
        assert_eq!(bare.render(&parts("", "")), UNNAMED);

        let leading = FilenameFormat::new("{location}.{ext}").unwrap();
        assert_eq!(
            leading.render(&parts("", "jpg")),
            "_jpg",
            "a leading dot would hide the file"
        );
    }

    /// Every token value is arbitrary text as far as this type is concerned,
    /// and the guarantee belongs to the function rather than to its callers.
    #[test]
    fn a_hostile_token_value_cannot_add_a_separator() {
        let format = FilenameFormat::new("{original_stem}.{ext}").unwrap();
        let rendered = format.render(&FilenameParts {
            date: "",
            time: "",
            location: "",
            extension: "../../etc/passwd",
            original_stem: "../..",
        });
        assert!(!rendered.contains('/'), "got {rendered}");
        assert_eq!(rendered, "_____.______etc_passwd");
    }

    // -----------------------------------------------------------------
    // Scheme
    // -----------------------------------------------------------------

    #[test]
    fn a_scheme_reports_the_patterns_it_was_built_from() {
        let scheme = Scheme::new("%Y/%m", "{date}.{ext}", false).unwrap();
        assert_eq!(
            scheme.date_directory(&march()),
            Some(PathBuf::from("2024/03"))
        );
        assert_eq!(
            scheme.filename(&parts("-London-GB", "jpg")),
            "2024-03-15.jpg"
        );
        assert!(!scheme.include_location());
    }

    // -----------------------------------------------------------------
    // OutputSubdir
    // -----------------------------------------------------------------

    #[test]
    fn a_subdir_keeps_the_name_it_was_given() {
        let dir = OutputSubdir::new("unsorted_dir", "undated").unwrap();
        assert_eq!(dir.path(), Path::new("undated"));
        assert_eq!(dir.pattern(), "undated");
    }

    /// Nesting is allowed, because every component is sanitised separately and
    /// refusing it would be a rule with nothing behind it.
    #[test]
    fn a_subdir_may_be_nested() {
        assert_eq!(
            OutputSubdir::new("duplicates_dir", "_review/copies")
                .unwrap()
                .path(),
            Path::new("_review/copies")
        );
    }

    /// The whole reason the type exists. `unsorted_dir = "/etc"` is one line of
    /// config away from filing photographs outside the tree the run was pointed
    /// at, and the error has to name the key the reader must go and edit.
    #[test]
    fn a_subdir_that_could_leave_the_output_tree_is_refused() {
        let absolute = OutputSubdir::new("unsorted_dir", "/etc").unwrap_err();
        assert!(matches!(absolute, FormatError::Absolute { .. }));
        assert!(
            absolute.to_string().contains("unsorted_dir"),
            "the refusal must name the key: {absolute}"
        );

        assert!(matches!(
            OutputSubdir::new("duplicates_dir", "../elsewhere"),
            Err(FormatError::ParentDir { .. })
        ));
        assert!(matches!(
            OutputSubdir::new("unsorted_dir", ""),
            Err(FormatError::Empty { .. })
        ));
        assert!(matches!(
            OutputSubdir::new("unsorted_dir", "un\0sorted"),
            Err(FormatError::NullByte { .. })
        ));
    }

    /// What the refusals cannot catch, the sanitising does: a name that is
    /// accepted still comes out as ordinary components, whatever it carried.
    #[test]
    fn an_accepted_subdir_is_still_sanitised_component_by_component() {
        assert_eq!(
            OutputSubdir::new("unsorted_dir", ".hidden dir/a:b")
                .unwrap()
                .path(),
            Path::new("_hidden-dir/a_b"),
            "a leading dot hides the directory and a colon is not a filename character"
        );
        assert_eq!(
            OutputSubdir::new("unsorted_dir", "a//b/").unwrap().path(),
            Path::new("a/b"),
            "doubled and trailing separators collapse rather than making a nameless directory"
        );
        assert_eq!(
            OutputSubdir::new("unsorted_dir", "a\\b").unwrap().path(),
            Path::new("a_b"),
            "a backslash is one component's text, not a separator"
        );
    }

    // -----------------------------------------------------------------
    // Layout
    // -----------------------------------------------------------------

    #[test]
    fn a_layout_carries_the_scheme_and_both_directories() {
        let layout = Layout::new(
            Scheme::new("%Y/%m", "{date}.{ext}", false).unwrap(),
            OutputSubdir::new("unsorted_dir", "undated").unwrap(),
            OutputSubdir::new("duplicates_dir", "copies").unwrap(),
        );

        assert_eq!(
            layout.date_directory(&march()),
            Some(PathBuf::from("2024/03"))
        );
        assert_eq!(
            layout.filename(&parts("-London-GB", "jpg")),
            "2024-03-15.jpg"
        );
        assert!(!layout.include_location());
        assert_eq!(layout.unsorted(), Path::new("undated"));
        assert_eq!(layout.duplicates(), Path::new("copies"));
        // The scheme is reachable whole, not only through the three methods
        // `Layout` forwards.
        assert_eq!(
            layout.scheme().date_directory(&march()),
            layout.date_directory(&march())
        );
    }

    /// The first failure wins, and it is the one the reader has to fix.
    #[test]
    fn a_scheme_refuses_either_bad_half() {
        assert!(matches!(
            Scheme::new("../%Y", "{date}.{ext}", true),
            Err(FormatError::ParentDir { .. })
        ));
        assert!(matches!(
            Scheme::new("%Y", "{date}", true),
            Err(FormatError::MissingExtension { .. })
        ));
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
