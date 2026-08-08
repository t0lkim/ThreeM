//! What a run is configured to do, and how the layers that say so combine.
//!
//! Two types, and the distinction between them is the whole design. [`Settings`]
//! is the *resolved* answer — every tunable has a value, nothing is optional,
//! and the pipeline reads it without ever asking "was that set?". [`PartialSettings`]
//! is one *layer's opinion*: every field is `Option`, and `None` means "this
//! layer said nothing", which is not the same as "this layer said the default".
//!
//! That difference is the reason a config file can work at all. A layer that
//! could not distinguish silence from agreement would have every file it read
//! overwrite every file read before it with values nobody wrote down, and the
//! lowest-priority layer would win — which is precisely backwards.
//!
//! Layers combine with [`PartialSettings::merge`], lowest priority first, and
//! the accumulated opinion becomes a [`Settings`] via [`Settings::resolve`],
//! which applies the built-in defaults **last** — to the fields still `None`
//! after every layer has spoken, and to no others.
//!
//! The merge algebra above deliberately knows nothing about *where* layers come
//! from. Below it sits the loader — file discovery, TOML parsing, and the `MMM_`
//! environment variables — which all produce a `PartialSettings` and nothing
//! else. Every discovery function takes what it depends on as an argument
//! ([`LoadOptions`] carries the working directory, the user config path and the
//! environment), so the whole of it is testable against a temporary directory
//! without a single process-wide `set_var`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::naming::{
    DateDirectoryFormat, FilenameFormat, FormatError, Layout, OutputSubdir, Scheme,
};
use crate::organiser::DatePolicy;
use crate::reporter::FallbackWarning;
use crate::scanner::{PatternError, ScanFilter, IMAGE_EXTENSIONS, VIDEO_EXTENSIONS};
use crate::timezone::{Timezone, TimezoneError, TimezonePolicy};

/// Files processed between prompts, when nothing says otherwise.
pub const DEFAULT_CHUNK_SIZE: usize = 100;

/// The dated directory each file is filed under.
///
/// One directory per day — `2024-03-15`, not `2024/03/15`. This is the layout
/// the organiser produces today, and the default has to keep producing it: a
/// default that quietly re-nested the tree would leave every existing library
/// split across two structures on the first run after an upgrade, with half the
/// photographs under `2024-03-15/` and half under `2024/03/15/` and nothing
/// having asked.
///
/// Anyone who wants the nested form now has a supported way to ask for it,
/// which is the point of the setting.
pub const DEFAULT_DATE_DIRECTORY_FORMAT: &str = "%Y-%m-%d";

/// The name each file is given.
///
/// The tokens are `{date}`, `{time}`, `{location}`, `{ext}` and
/// `{original_stem}`. `{location}` carries its own leading separator and
/// expands to nothing at all when a file has no coordinates or when
/// [`Settings::include_location`] is off — which is why the default has no
/// hyphen in front of it, and why a file without a location does not come out
/// with a trailing `-`.
pub const DEFAULT_FILENAME_FORMAT: &str = "{date}-{time}{location}.{ext}";

/// Where relocated duplicates are grouped, below the output tree.
pub const DEFAULT_DUPLICATES_DIR: &str = "duplicates";

/// Where files with no usable date go, below the output tree.
pub const DEFAULT_UNSORTED_DIR: &str = "unsorted";

/// How much of a run may be dated from the filesystem before it says so.
///
/// A fifth, because a library where one file in five has no readable date is
/// past the point of being a few stray screenshots — it is either a folder of
/// scans, or a format this tool cannot read, and both are worth a sentence. Set
/// it to `0` to hear about every single one, or to `100` never to hear about it.
pub const DEFAULT_FILESYSTEM_DATE_WARNING_PERCENT: u8 = 20;

/// Which file extensions count as media.
///
/// A pair rather than one list because the two are treated differently
/// downstream — a video's date is read out of a different metadata container
/// than a photograph's — so a caller adding `.insv` has to say which kind of
/// thing it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extensions {
    /// Lowercase, no leading dot.
    pub image: Vec<String>,
    /// Lowercase, no leading dot.
    pub video: Vec<String>,
}

impl Default for Extensions {
    fn default() -> Self {
        Self {
            image: IMAGE_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
            video: VIDEO_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
        }
    }
}

/// Everything a run can be told, with every question answered.
///
/// # `commit` is not here, and must not be added
///
/// Moving files is opt-in at the command line and nowhere else. Phase 01 made
/// `--commit` the single switch between "print the plan" and "move this
/// person's photo library", and the value of that switch is that it has to be
/// typed, deliberately, by whoever is standing there — a run cannot become
/// destructive because of a file somebody wrote months ago, or one that came
/// along with a copied project directory, or one inherited from `$HOME` by a
/// script that only meant to preview.
///
/// A `commit` key in a config file would undo that in one line, silently, for
/// every invocation on the machine. So it is absent by design, the loader
/// rejects the key rather than ignoring it, and this comment is here so that
/// nobody later reads the omission as an oversight and "completes" the struct.
///
/// The same reasoning covers `no_journal` and `i_know_what_im_doing`: both exist
/// to make an unreversible run harder to ask for, and both would be defeated by
/// being settable from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Where organised files are written. `None` means "the first input
    /// directory", which is not knowable until the command line has been
    /// parsed — so this stays optional even in the resolved type rather than
    /// being defaulted to something wrong.
    pub output_dir: Option<PathBuf>,

    /// Files moved between prompts.
    pub chunk_size: usize,

    /// Do not ask at chunk boundaries.
    pub no_prompt: bool,

    /// Log verbosity, counted as `-v` is on the command line.
    pub verbose: u8,

    /// Where this run's journal is written. `None` means the default location
    /// inside the output tree — see [`crate::config::journal_dir_for`].
    ///
    /// It is `None` rather than a computed path for the same reason as
    /// `output_dir`: the output tree is not known here.
    pub journal_dir: Option<PathBuf>,

    /// A strftime-style pattern for the dated directory.
    pub date_directory_format: String,

    /// A token pattern for the derived filename.
    pub filename_format: String,

    /// Whether to append the geocoded place name to filenames.
    pub include_location: bool,

    /// The directory name duplicates are grouped under, relative to the output.
    pub duplicates_dir: PathBuf,

    /// The directory name undateable files go to, relative to the output.
    pub unsorted_dir: PathBuf,

    /// Which extensions the scanner admits.
    pub extensions: Extensions,

    /// Paths the scan passes over. Empty by default: skipping a photograph
    /// somebody expected to be organised is a surprise, so every skip has to be
    /// asked for.
    pub skip_patterns: Vec<String>,

    /// Which wall clock an undated EXIF timestamp is read against.
    ///
    /// `None` — like `output_dir`, and unlike every other field here — because
    /// its fallback is not a value this module could name: it is the machine's
    /// own timezone, which is not knowable until the run happens and is not
    /// something a default should pretend to have decided. `None` means "nobody
    /// configured one", and [`crate::timezone::TimezonePolicy`] takes it from
    /// there.
    pub default_timezone: Option<String>,

    /// Refuse to file any photograph under a date it did not record itself.
    ///
    /// Unlike `commit`, this is settable from a config file, and the difference
    /// is the direction it points. `commit = true` in a file would make a run
    /// destructive that nobody asked to be; `require_exif = true` makes a run
    /// more conservative than it would otherwise have been. A setting that can
    /// only cost you a file staying where it was is one a file may make on your
    /// behalf.
    pub require_exif: bool,

    /// The share of dated files that may take their date from the filesystem
    /// before the run says so, as a whole percentage.
    ///
    /// `0` warns whenever a single file fell back, `100` never warns.
    pub filesystem_date_warning_percent: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output_dir: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            no_prompt: false,
            verbose: 0,
            journal_dir: None,
            date_directory_format: DEFAULT_DATE_DIRECTORY_FORMAT.to_string(),
            filename_format: DEFAULT_FILENAME_FORMAT.to_string(),
            include_location: true,
            duplicates_dir: PathBuf::from(DEFAULT_DUPLICATES_DIR),
            unsorted_dir: PathBuf::from(DEFAULT_UNSORTED_DIR),
            extensions: Extensions::default(),
            skip_patterns: Vec::new(),
            default_timezone: None,
            require_exif: false,
            filesystem_date_warning_percent: DEFAULT_FILESYSTEM_DATE_WARNING_PERCENT,
        }
    }
}

/// One layer's opinion: the `[extensions]` table of a config file.
///
/// Split from the parent so that a project config naming only `video` leaves
/// the user config's `image` list alone. Merging the table wholesale would mean
/// adding one video extension silently discarded twenty image ones, and the
/// person who did it would find out when a scan came back empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialExtensions {
    pub image: Option<Vec<String>>,
    pub video: Option<Vec<String>>,
}

impl PartialExtensions {
    /// Take each field from `higher_priority` where it has one.
    #[must_use]
    pub fn merge(self, higher_priority: Self) -> Self {
        Self {
            image: higher_priority.image.or(self.image),
            video: higher_priority.video.or(self.video),
        }
    }
}

/// One layer's opinion of the settings: user config, project config,
/// environment, or command line.
///
/// `deny_unknown_fields` is doing real work. A config file is written once and
/// read forever, usually by someone who will not run the tool again for months,
/// and a mistyped `chunck_size` that is quietly ignored looks exactly like a
/// setting that does not work — the user changes the value, sees no difference,
/// and concludes the feature is broken. Refusing the file and naming the key
/// costs one error message and saves that entire investigation. It is also the
/// mechanism that makes `commit = true` a refusal rather than a no-op.
///
/// Lists **replace** rather than append. A layer that said
/// `skip_patterns = ["*.tmp"]` and got the lower layer's entries as well could
/// never be used to *narrow* a list, and there would be no way to express
/// "actually, scan everything" from a project file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialSettings {
    pub output_dir: Option<PathBuf>,
    pub chunk_size: Option<usize>,
    pub no_prompt: Option<bool>,
    pub verbose: Option<u8>,
    pub journal_dir: Option<PathBuf>,
    #[serde(default, deserialize_with = "de_date_directory_format")]
    pub date_directory_format: Option<String>,
    #[serde(default, deserialize_with = "de_filename_format")]
    pub filename_format: Option<String>,
    pub include_location: Option<bool>,
    #[serde(default, deserialize_with = "de_duplicates_dir")]
    pub duplicates_dir: Option<PathBuf>,
    #[serde(default, deserialize_with = "de_unsorted_dir")]
    pub unsorted_dir: Option<PathBuf>,
    pub extensions: Option<PartialExtensions>,
    #[serde(default, deserialize_with = "de_skip_patterns")]
    pub skip_patterns: Option<Vec<String>>,
    #[serde(default, deserialize_with = "de_default_timezone")]
    pub default_timezone: Option<String>,
    pub require_exif: Option<bool>,
    #[serde(default, deserialize_with = "de_percentage")]
    pub filesystem_date_warning_percent: Option<u8>,
}

impl PartialSettings {
    /// Combine two layers, `higher_priority` winning wherever it has an opinion.
    ///
    /// Field-wise, never wholesale: a layer that sets one key does not blank the
    /// eleven it said nothing about. `[extensions]` recurses into
    /// [`PartialExtensions::merge`] for the same reason one level down.
    #[must_use]
    pub fn merge(self, higher_priority: Self) -> Self {
        Self {
            output_dir: higher_priority.output_dir.or(self.output_dir),
            chunk_size: higher_priority.chunk_size.or(self.chunk_size),
            no_prompt: higher_priority.no_prompt.or(self.no_prompt),
            verbose: higher_priority.verbose.or(self.verbose),
            journal_dir: higher_priority.journal_dir.or(self.journal_dir),
            date_directory_format: higher_priority
                .date_directory_format
                .or(self.date_directory_format),
            filename_format: higher_priority.filename_format.or(self.filename_format),
            include_location: higher_priority.include_location.or(self.include_location),
            duplicates_dir: higher_priority.duplicates_dir.or(self.duplicates_dir),
            unsorted_dir: higher_priority.unsorted_dir.or(self.unsorted_dir),
            extensions: match (self.extensions, higher_priority.extensions) {
                (Some(lower), Some(higher)) => Some(lower.merge(higher)),
                (lower, higher) => higher.or(lower),
            },
            skip_patterns: higher_priority.skip_patterns.or(self.skip_patterns),
            default_timezone: higher_priority.default_timezone.or(self.default_timezone),
            require_exif: higher_priority.require_exif.or(self.require_exif),
            filesystem_date_warning_percent: higher_priority
                .filesystem_date_warning_percent
                .or(self.filesystem_date_warning_percent),
        }
    }

    /// Whether this layer said anything at all.
    ///
    /// Used by `mmm config path` and the source annotations to tell a file that
    /// was read and was empty from one that set something.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Read `date_directory_format`, refusing a pattern that is not one.
///
/// Validation hangs off deserialisation rather than off a pass over the resolved
/// [`Settings`] for two reasons, and the second is the one that matters. The
/// first is *when*: a broken pattern is a broken config file whether or not a
/// higher layer would have overridden it, which is the same rule
/// `deny_unknown_fields` applies one line up. The second is *where*: an error
/// raised here carries the TOML span of the value that caused it, so
/// [`parse_layer`] reports `mmm.toml:7:25` and the reader is looking at the
/// right line. A check run after the fold would know only that some layer,
/// somewhere, had said something wrong.
fn de_date_directory_format<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let pattern = Option::<String>::deserialize(deserializer)?;
    if let Some(pattern) = &pattern {
        DateDirectoryFormat::new(pattern).map_err(serde::de::Error::custom)?;
    }
    Ok(pattern)
}

/// Read `filename_format`, refusing a pattern that is not one.
///
/// See [`de_date_directory_format`] for why this is a deserialiser.
fn de_filename_format<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let pattern = Option::<String>::deserialize(deserializer)?;
    if let Some(pattern) = &pattern {
        FilenameFormat::new(pattern).map_err(serde::de::Error::custom)?;
    }
    Ok(pattern)
}

/// Read `duplicates_dir`, refusing a name that would leave the output tree.
///
/// See [`de_date_directory_format`] for why this is a deserialiser. The
/// containment argument is the same one, and it applies here for a blunter
/// reason: `duplicates_dir = "/"` is a single line that would scatter a photo
/// library across the root of the filesystem.
fn de_duplicates_dir<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    de_subdir(deserializer, "duplicates_dir")
}

/// Read `unsorted_dir`, refusing a name that would leave the output tree.
///
/// See [`de_duplicates_dir`].
fn de_unsorted_dir<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    de_subdir(deserializer, "unsorted_dir")
}

/// The shared body of the two subdirectory deserialisers.
fn de_subdir<'de, D>(deserializer: D, key: &'static str) -> Result<Option<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let name = Option::<String>::deserialize(deserializer)?;
    if let Some(name) = &name {
        OutputSubdir::new(key, name).map_err(serde::de::Error::custom)?;
    }
    Ok(name.map(PathBuf::from))
}

/// Read `default_timezone`, refusing a value that is not a timezone.
///
/// See [`de_date_directory_format`] for why this is a deserialiser. The
/// consequence of not refusing it here is particular: a `default_timezone` that
/// silently failed to apply would leave the run falling back to the machine's
/// zone, which on the machine the config was written on is very often the same
/// answer — so the setting would look like it worked, and would quietly stop
/// working the moment the library was organised from anywhere else.
fn de_default_timezone<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let name = Option::<String>::deserialize(deserializer)?;
    if let Some(name) = &name {
        name.parse::<Timezone>().map_err(serde::de::Error::custom)?;
    }
    Ok(name)
}

/// Read `filesystem_date_warning_percent`, refusing a value that is not a
/// percentage.
///
/// A `u8` already refuses `300` and `-1` — TOML deserialisation says so in its
/// own words — but it accepts everything from 101 to 255, and each of those is a
/// threshold no run can ever cross. Somebody who writes one has asked for the
/// warning to be off, which `100` already spells, and telling them so is better
/// than silently agreeing.
///
/// See [`de_date_directory_format`] for why this hangs off deserialisation.
fn de_percentage<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u8>::deserialize(deserializer)?;
    if let Some(value) = value {
        if value > 100 {
            return Err(serde::de::Error::custom(format!(
                "`filesystem_date_warning_percent` is a percentage, and {value} is not one — use \
                 100 to turn the warning off"
            )));
        }
    }
    Ok(value)
}

/// Read `skip_patterns`, refusing an entry that is not a glob.
///
/// See [`de_date_directory_format`] for why this is a deserialiser. A pattern
/// that will not compile is refused rather than dropped, because a skip that
/// silently matches nothing is indistinguishable from a setting that does not
/// work — and the operator would only find out by noticing that files they
/// asked to pass over were organised anyway.
fn de_skip_patterns<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let patterns = Option::<Vec<String>>::deserialize(deserializer)?;
    if let Some(patterns) = &patterns {
        ScanFilter::new(&[], &[], patterns).map_err(serde::de::Error::custom)?;
    }
    Ok(patterns)
}

impl Settings {
    /// The validated shape of the output tree this run writes.
    ///
    /// Every layer that can supply one of these validates it as it is read —
    /// the file layers in [`parse_layer`], the environment in [`env_layer`] —
    /// so in a real run this cannot fail. It returns a `Result` anyway because
    /// [`Settings`] is an ordinary struct that a caller may build by hand, and
    /// the alternative to an error here would be the organiser discovering the
    /// problem with half a library already moved.
    ///
    /// The two directory names go through `to_string_lossy`, so a path that is
    /// not valid UTF-8 — impossible from TOML, and unreachable through the
    /// environment parser, but constructible by hand — arrives as replacement
    /// characters and is sanitised like any other text rather than being
    /// refused for a reason nobody could act on.
    ///
    /// # Errors
    ///
    /// The first [`FormatError`] any of the four produces.
    pub fn layout(&self) -> Result<Layout, FormatError> {
        Ok(Layout::new(
            Scheme::new(
                &self.date_directory_format,
                &self.filename_format,
                self.include_location,
            )?,
            OutputSubdir::new("unsorted_dir", &self.unsorted_dir.to_string_lossy())?,
            OutputSubdir::new("duplicates_dir", &self.duplicates_dir.to_string_lossy())?,
        ))
    }

    /// Which wall clock this run reads undated timestamps against.
    ///
    /// Fallible for the same reason as [`Self::layout`]: every layer validated
    /// its own value as it was read, so this is the last line of defence rather
    /// than the first.
    ///
    /// # Errors
    ///
    /// [`TimezoneError`] naming the value, if a hand-built [`Settings`] carries
    /// one that is not a timezone.
    pub fn timezone_policy(&self) -> Result<TimezonePolicy, TimezoneError> {
        self.default_timezone
            .as_deref()
            .map(str::parse::<Timezone>)
            .transpose()
            .map(TimezonePolicy::new)
    }

    /// Which dates this run is willing to file a photograph under.
    #[must_use]
    pub fn date_policy(&self) -> DatePolicy {
        DatePolicy::from_require_exif(self.require_exif)
    }

    /// The share of filesystem dates above which the run says so.
    ///
    /// Infallible where [`Self::layout`] and [`Self::timezone_policy`] are not:
    /// every value a `u8` can hold is one this can act on, and the range check
    /// on the way in exists to catch a threshold nobody meant rather than one
    /// nothing could use.
    #[must_use]
    pub fn fallback_warning(&self) -> FallbackWarning {
        FallbackWarning(self.filesystem_date_warning_percent)
    }

    /// What the scan admits and what it passes over.
    ///
    /// Fallible for the same reason as [`Self::layout`]: every layer compiled
    /// its own patterns as it was read, so this is the last line of defence
    /// rather than the first.
    ///
    /// # Errors
    ///
    /// [`PatternError`] naming the first skip pattern that is not a glob.
    pub fn scan_filter(&self) -> Result<ScanFilter, PatternError> {
        ScanFilter::new(
            &self.extensions.image,
            &self.extensions.video,
            &self.skip_patterns,
        )
    }

    /// Fold `layers` lowest-priority-first, then fill what nobody set.
    ///
    /// The caller owns the order, and the order is the precedence rule: built-in
    /// defaults (implicit, applied here at the end), then user config, project
    /// config, environment, command line. Passing them in that sequence is all
    /// there is to it — there is no priority number on a layer, because a
    /// number would be a second place for the ordering to live and a second
    /// place for it to be wrong.
    ///
    /// Defaults are applied **last** rather than seeded as the first layer.
    /// Seeding them would work identically for the merge, and would destroy the
    /// thing `mmm config show` needs: after the fold, a field that is still
    /// `None` is one no layer claimed, which is exactly the question "where did
    /// this value come from?" reduces to. That answer is recoverable from the
    /// same layer list without changing this signature — walk it backwards and
    /// report the first layer whose field is `Some` — so the source annotations
    /// arriving in a later phase need no new merge algebra.
    #[must_use]
    pub fn resolve<I>(layers: I) -> Self
    where
        I: IntoIterator<Item = PartialSettings>,
    {
        let merged = layers
            .into_iter()
            .fold(PartialSettings::default(), PartialSettings::merge);
        Self::from_partial(merged)
    }

    /// Apply the built-in defaults to whatever the layers left unanswered.
    #[must_use]
    pub fn from_partial(partial: PartialSettings) -> Self {
        let defaults = Self::default();
        let default_extensions = defaults.extensions;

        let extensions = match partial.extensions {
            None => default_extensions,
            Some(ext) => Extensions {
                image: ext.image.unwrap_or(default_extensions.image),
                video: ext.video.unwrap_or(default_extensions.video),
            },
        };

        Self {
            output_dir: partial.output_dir,
            chunk_size: partial.chunk_size.unwrap_or(defaults.chunk_size),
            no_prompt: partial.no_prompt.unwrap_or(defaults.no_prompt),
            verbose: partial.verbose.unwrap_or(defaults.verbose),
            journal_dir: partial.journal_dir,
            date_directory_format: partial
                .date_directory_format
                .unwrap_or(defaults.date_directory_format),
            filename_format: partial.filename_format.unwrap_or(defaults.filename_format),
            include_location: partial
                .include_location
                .unwrap_or(defaults.include_location),
            duplicates_dir: partial.duplicates_dir.unwrap_or(defaults.duplicates_dir),
            unsorted_dir: partial.unsorted_dir.unwrap_or(defaults.unsorted_dir),
            extensions,
            skip_patterns: partial.skip_patterns.unwrap_or(defaults.skip_patterns),
            default_timezone: partial.default_timezone,
            require_exif: partial.require_exif.unwrap_or(defaults.require_exif),
            filesystem_date_warning_percent: partial
                .filesystem_date_warning_percent
                .unwrap_or(defaults.filesystem_date_warning_percent),
        }
    }
}

// =====================================================================
// Where a layer came from
// =====================================================================

/// The directory holding the per-user config, below the platform config dir.
pub const USER_CONFIG_DIR_NAME: &str = "mmm";

/// The per-user config file, inside [`USER_CONFIG_DIR_NAME`].
pub const USER_CONFIG_FILE_NAME: &str = "config.toml";

/// The project config filenames, in the order a directory is searched.
///
/// Both spellings because both conventions are real: a repository that keeps
/// its tool configuration visible wants `mmm.toml`, and one that keeps dotfiles
/// out of the way wants `.mmm.toml`. A directory holding both is answered by
/// the first — searching stops at a hit rather than merging them, because two
/// files in one directory disagreeing about a setting has no obvious winner and
/// inventing one would be a rule nobody could predict.
pub const PROJECT_CONFIG_NAMES: &[&str] = &["mmm.toml", ".mmm.toml"];

/// The prefix every environment override carries.
pub const ENV_PREFIX: &str = "MMM_";

/// The environment variable that relocates the config directory, when set.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// The keys that exist only on the command line, and the reason each does.
///
/// All three make a run harder to ask for by accident, which is a property they
/// only have while they must be typed. `deny_unknown_fields` already refuses
/// them in a file — this list exists so the refusal can say *why* instead of
/// listing eleven field names and leaving the reader to guess which one they
/// wanted.
pub const COMMAND_LINE_ONLY_KEYS: &[(&str, &str)] = &[
    (
        "commit",
        "moving files is opt-in at the command line so that no file — not one written months ago, \
         not one that arrived with a copied project directory — can make a run destructive",
    ),
    (
        "no_journal",
        "a run without a journal cannot be undone, so it has to be asked for deliberately",
    ),
    (
        "i_know_what_im_doing",
        "acknowledging an unsafe combination is the acknowledgement, and a file cannot give it on \
         your behalf",
    ),
];

/// The layer a value came from, in ascending priority.
///
/// Carried alongside each loaded layer so `mmm config show` can answer "why did
/// it do that?" by naming the file, rather than by having its reader reconstruct
/// the search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsSource {
    /// The built-in defaults in this module.
    Defaults,
    /// The per-user config file.
    UserConfig(PathBuf),
    /// A project config found by walking up from the working directory.
    ProjectConfig(PathBuf),
    /// The file named by `--config`, which replaces discovery entirely.
    ExplicitConfig(PathBuf),
    /// `MMM_`-prefixed environment variables.
    Environment,
    /// The command line.
    CommandLine,
}

impl SettingsSource {
    /// The file this layer was read from, when it was a file.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::UserConfig(path) | Self::ProjectConfig(path) | Self::ExplicitConfig(path) => {
                Some(path)
            }
            Self::Defaults | Self::Environment | Self::CommandLine => None,
        }
    }
}

impl fmt::Display for SettingsSource {
    /// The form the `# from:` annotations use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Defaults => f.write_str("built-in defaults"),
            Self::UserConfig(path) => write!(f, "user config ({})", path.display()),
            Self::ProjectConfig(path) => write!(f, "project config ({})", path.display()),
            Self::ExplicitConfig(path) => write!(f, "explicit config ({})", path.display()),
            Self::Environment => f.write_str("environment"),
            Self::CommandLine => f.write_str("command line"),
        }
    }
}

// =====================================================================
// What can go wrong
// =====================================================================

/// A config that could not be turned into a layer.
///
/// Every variant names the thing the reader has to go and edit — a file and a
/// position in it, or a variable. None of them is recoverable by falling back to
/// the defaults: a tool that quietly ignored a config file it could not parse
/// would do the wrong thing to somebody's photo library and report success.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `--config` named a file that is not there.
    #[error(
        "no config file at {} — --config names the file to read, and carrying on with the \
         defaults would silently do something other than what was asked",
        .path.display()
    )]
    Missing { path: PathBuf },

    /// The file exists and could not be read.
    #[error("could not read the config file {}: {source}", .path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The file was read and is not valid, or names a key that is not a setting.
    #[error("{}:{line}:{column}: {message}", .path.display())]
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },

    /// A `MMM_` variable is not a setting, or its value is not of the right type.
    #[error("{variable}: {message}")]
    Environment { variable: String, message: String },

    /// `--config` and `--no-config` together.
    #[error(
        "--config and --no-config contradict each other: one names a file to read, the other says \
         to read none"
    )]
    Contradiction,
}

/// Why a command-line-only key is refused wherever it is written down.
///
/// # Panics
///
/// Never for a key drawn from [`COMMAND_LINE_ONLY_KEYS`]; the fallback branch
/// covers any other key rather than panicking.
pub fn command_line_only_refusal(key: &str) -> String {
    let reason = COMMAND_LINE_ONLY_KEYS
        .iter()
        .find_map(|(name, reason)| (*name == key).then_some(*reason))
        .unwrap_or("it exists to make an unsafe run harder to ask for by accident");
    format!(
        "`{key}` cannot be set here — {reason}. Pass --{flag} on the command line instead.",
        flag = key.replace('_', "-")
    )
}

/// Turn a byte offset into the 1-based line and column a person can navigate to.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before, |newline| &before[newline + 1..])
        .chars()
        .count()
        + 1;
    (line, column)
}

/// Replace serde's field list with the reason, for the keys that have one.
///
/// `deny_unknown_fields` already does the refusing. What it cannot do is explain
/// itself: its message for `commit = true` is a list of the eleven fields that
/// *are* settings, which tells a reader everything except the one thing they
/// need to know.
fn explain(message: &str) -> String {
    for (key, _) in COMMAND_LINE_ONLY_KEYS {
        if message.starts_with(&format!("unknown field `{key}`")) {
            return command_line_only_refusal(key);
        }
    }
    message.to_string()
}

// =====================================================================
// Reading one file
// =====================================================================

/// Parse one config file's text, naming `path` in any error.
///
/// # Errors
///
/// [`ConfigError::Parse`] for malformed TOML, an unknown key, or a value of the
/// wrong type — with the line and column of the offending token.
pub fn parse_layer(text: &str, path: &Path) -> Result<PartialSettings, ConfigError> {
    toml::from_str::<PartialSettings>(text).map_err(|error| {
        let (line, column) = error
            .span()
            .map_or((1, 1), |span| line_and_column(text, span.start));
        ConfigError::Parse {
            path: path.to_path_buf(),
            line,
            column,
            message: explain(error.message()),
        }
    })
}

/// Read and parse a config file that is expected to exist.
///
/// # Errors
///
/// [`ConfigError::Missing`] if it is not there, [`ConfigError::Unreadable`] if
/// it cannot be opened, [`ConfigError::Parse`] if its contents are not a valid
/// layer.
pub fn load_file(path: &Path) -> Result<PartialSettings, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ConfigError::Missing {
                path: path.to_path_buf(),
            }
        } else {
            ConfigError::Unreadable {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    parse_layer(&text, path)
}

/// Read a discovered config file, where absence is an ordinary answer.
///
/// The distinction from [`load_file`] is the whole difference between a file the
/// user asked for and one the tool went looking for: `~/.config/mmm/config.toml`
/// not existing is the common case, and `--config missing.toml` never is.
///
/// # Errors
///
/// As [`load_file`], except that a missing file is `Ok(None)`.
pub fn load_optional_file(path: &Path) -> Result<Option<PartialSettings>, ConfigError> {
    match load_file(path) {
        Ok(settings) => Ok(Some(settings)),
        Err(ConfigError::Missing { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

// =====================================================================
// Finding the files
// =====================================================================

/// Where the per-user config lives on this machine, if there is a home to put it in.
///
/// `XDG_CONFIG_HOME` is honoured first and on every platform, not only where the
/// [`directories`] crate would consult it. Somebody who has set that variable
/// has said where their configuration lives, and a tool that obeyed it on Linux
/// and wrote to `~/Library/Application Support` on macOS would be answering a
/// question it was not asked.
pub fn user_config_path() -> Option<PathBuf> {
    user_config_path_from(
        std::env::var_os(XDG_CONFIG_HOME).map(PathBuf::from),
        directories::BaseDirs::new().map(|dirs| dirs.config_dir().to_path_buf()),
    )
}

/// The decision behind [`user_config_path`], with its two inputs passed in.
///
/// A relative `XDG_CONFIG_HOME` is ignored, as the specification requires: a
/// config directory that moved every time the process changed directory would
/// be worse than none.
fn user_config_path_from(xdg: Option<PathBuf>, platform: Option<PathBuf>) -> Option<PathBuf> {
    let base = match xdg {
        Some(dir) if dir.is_absolute() => dir,
        _ => platform?,
    };
    Some(base.join(USER_CONFIG_DIR_NAME).join(USER_CONFIG_FILE_NAME))
}

/// What walking up from a directory looking for a project config found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectSearch {
    /// Every path considered, in order, up to and including the one that hit.
    pub candidates: Vec<PathBuf>,
    /// The first file found, if any.
    pub found: Option<PathBuf>,
}

/// Walk up from `start` to the filesystem root, stopping at the first config.
///
/// Nearest wins, which is the only rule that makes nesting useful: a config in a
/// subdirectory of a project is a statement about that subdirectory, and one
/// several levels up that outranked it could never be overridden.
///
/// The walk is recorded rather than merely performed so `mmm config path` can
/// show what was searched. It stops at the first hit, so what it lists is what
/// actually happened and not a hypothetical full walk.
pub fn find_project_config(start: &Path) -> ProjectSearch {
    // Relative starts are resolved so that `.` searches its ancestors rather
    // than terminating immediately at the empty path.
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());

    let mut search = ProjectSearch::default();
    for directory in start.ancestors() {
        for name in PROJECT_CONFIG_NAMES {
            let candidate = directory.join(name);
            let hit = candidate.is_file();
            search.candidates.push(candidate.clone());
            if hit {
                search.found = Some(candidate);
                return search;
            }
        }
    }
    search
}

// =====================================================================
// The environment layer
// =====================================================================

/// Build a layer from `MMM_`-prefixed variables.
///
/// Keys are the setting names uppercased, with `.` written as `_`:
/// `MMM_CHUNK_SIZE`, `MMM_OUTPUT_DIR`, `MMM_NO_PROMPT`,
/// `MMM_EXTENSIONS_IMAGE`. Lists are comma-separated, and an empty value is an
/// empty list — the way to say "scan everything" from a shell.
///
/// An unrecognised `MMM_` variable is an error for the same reason
/// `deny_unknown_fields` is: `MMM_CHUNCK_SIZE` that does nothing looks exactly
/// like a setting that does not work.
///
/// # Errors
///
/// [`ConfigError::Environment`] naming the variable, for an unknown key, a
/// command-line-only key, or a value that is not of the setting's type.
pub fn env_layer<I>(vars: I) -> Result<PartialSettings, ConfigError>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut layer = PartialSettings::default();
    let mut extensions = PartialExtensions::default();
    let mut saw_extensions = false;

    for (variable, value) in vars {
        let Some(suffix) = variable.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let key = suffix.to_ascii_lowercase();

        if COMMAND_LINE_ONLY_KEYS.iter().any(|(name, _)| *name == key) {
            return Err(ConfigError::Environment {
                variable: variable.clone(),
                message: command_line_only_refusal(&key),
            });
        }

        match key.as_str() {
            "output_dir" => layer.output_dir = Some(PathBuf::from(value)),
            "journal_dir" => layer.journal_dir = Some(PathBuf::from(value)),
            // The two that must stay below the output tree are checked here for
            // the same reason the formats are: the file layers refuse
            // `duplicates_dir = "/"`, and an environment that did not would be a
            // hole straight through that refusal.
            "duplicates_dir" => {
                OutputSubdir::new("duplicates_dir", &value)
                    .map_err(|error| env_refusal(&variable, &error))?;
                layer.duplicates_dir = Some(PathBuf::from(value));
            }
            "unsorted_dir" => {
                OutputSubdir::new("unsorted_dir", &value)
                    .map_err(|error| env_refusal(&variable, &error))?;
                layer.unsorted_dir = Some(PathBuf::from(value));
            }
            // Validated here rather than after the fold for the same reason the
            // file layers are, minus the span: the variable's own name is the
            // thing the reader has to go and fix.
            "date_directory_format" => {
                DateDirectoryFormat::new(&value).map_err(|error| env_refusal(&variable, &error))?;
                layer.date_directory_format = Some(value);
            }
            "filename_format" => {
                FilenameFormat::new(&value).map_err(|error| env_refusal(&variable, &error))?;
                layer.filename_format = Some(value);
            }
            "chunk_size" => layer.chunk_size = Some(parse_number(&variable, &value)?),
            "verbose" => layer.verbose = Some(parse_number(&variable, &value)?),
            "no_prompt" => layer.no_prompt = Some(parse_bool(&variable, &value)?),
            "require_exif" => layer.require_exif = Some(parse_bool(&variable, &value)?),
            // Range-checked here for the same reason the file layer checks it:
            // a threshold above 100 is one no run can cross, and agreeing with
            // it silently would leave somebody waiting for a warning that is
            // never coming.
            //
            // Parsed wide and narrowed afterwards, rather than straight into the
            // `u8` it is stored as. `"500".parse::<u8>()` fails, and the error it
            // fails with is "expected a whole number" — which is both wrong and
            // useless, 500 being very much a whole number. The reader needs to be
            // told the range, not to be told their arithmetic is not arithmetic.
            "filesystem_date_warning_percent" => {
                let percent: u32 = parse_number(&variable, &value)?;
                let percent = u8::try_from(percent).ok().filter(|p| *p <= 100);
                layer.filesystem_date_warning_percent =
                    Some(percent.ok_or_else(|| ConfigError::Environment {
                        variable: variable.clone(),
                        message: format!(
                            "expected a percentage between 0 and 100, got `{value}` — use 100 to \
                             turn the warning off"
                        ),
                    })?);
            }
            "include_location" => layer.include_location = Some(parse_bool(&variable, &value)?),
            "skip_patterns" => {
                let patterns = parse_list(&value);
                ScanFilter::new(&[], &[], &patterns)
                    .map_err(|error| env_refusal(&variable, &error))?;
                layer.skip_patterns = Some(patterns);
            }
            // Validated where it is read, like the formats above, and for a
            // sharper reason: see [`de_default_timezone`].
            "default_timezone" => {
                value
                    .parse::<Timezone>()
                    .map_err(|error| env_refusal(&variable, &error))?;
                layer.default_timezone = Some(value);
            }
            "extensions_image" => {
                extensions.image = Some(parse_list(&value));
                saw_extensions = true;
            }
            "extensions_video" => {
                extensions.video = Some(parse_list(&value));
                saw_extensions = true;
            }
            _ => {
                return Err(ConfigError::Environment {
                    variable: variable.clone(),
                    message: format!(
                        "`{key}` is not a setting — see `mmm config show` for the \
                                      keys that are"
                    ),
                })
            }
        }
    }

    if saw_extensions {
        layer.extensions = Some(extensions);
    }
    Ok(layer)
}

/// Dress a validation failure as the environment's refusal of one variable.
///
/// Generic over the error so the formats, the two subdirectories and the skip
/// patterns all report the same way: the variable's own name, then whatever the
/// validator said. The variable name is the thing the reader has to go and fix,
/// and it is the one piece none of the validators knows.
fn env_refusal(variable: &str, error: &impl fmt::Display) -> ConfigError {
    ConfigError::Environment {
        variable: variable.to_string(),
        message: error.to_string(),
    }
}

/// Parse a whole-number setting, naming the variable if it will not.
fn parse_number<T: std::str::FromStr>(variable: &str, value: &str) -> Result<T, ConfigError> {
    value.trim().parse().map_err(|_| ConfigError::Environment {
        variable: variable.to_string(),
        message: format!("expected a whole number, got `{value}`"),
    })
}

/// Parse a boolean setting. Deliberately narrow: `true`/`false`/`1`/`0`.
///
/// A shell has no booleans, so the tool has to choose which spellings count.
/// Accepting `yes`, `on` and `y` as well would mean a `MMM_NO_PROMPT=maybe`
/// somewhere in between reads as false to one tool and true to another; a short
/// list and a named error is the version nobody has to guess about.
fn parse_bool(variable: &str, value: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(ConfigError::Environment {
            variable: variable.to_string(),
            message: format!("expected true or false, got `{other}`"),
        }),
    }
}

/// Split a comma-separated list, discarding empty entries.
///
/// So an empty value, and a value of `,`, both mean the empty list rather than a
/// list containing one empty string — which downstream would be an extension
/// matching every file without a dot in its name.
fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(String::from)
        .collect()
}

// =====================================================================
// Assembling the layers
// =====================================================================

/// What the loader is allowed to look at.
///
/// Every field is an input rather than something read from the process, which is
/// what lets the discovery rules be tested against a temporary directory instead
/// of against whatever happens to be in the developer's `$HOME`. A real run
/// fills it with [`LoadOptions::from_process`]; [`LoadOptions::default`] looks at
/// nothing at all.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// `--config PATH`: read this and skip discovery.
    pub explicit: Option<PathBuf>,
    /// `--no-config`: read no files. The environment still applies.
    pub no_config: bool,
    /// Where the project walk starts — the working directory, in a real run.
    pub start_dir: Option<PathBuf>,
    /// The per-user config path, from [`user_config_path`].
    pub user_config: Option<PathBuf>,
    /// The environment, already collected.
    pub env: Vec<(String, String)>,
}

impl LoadOptions {
    /// The options a real invocation runs with.
    pub fn from_process(explicit: Option<PathBuf>, no_config: bool) -> Self {
        Self {
            explicit,
            no_config,
            start_dir: std::env::current_dir().ok(),
            user_config: user_config_path(),
            env: std::env::vars()
                .filter(|(key, _)| key.starts_with(ENV_PREFIX))
                .collect(),
        }
    }
}

/// One layer that had something to say, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedLayer {
    pub source: SettingsSource,
    pub settings: PartialSettings,
}

/// A path the file search considered, and whether it was there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchedPath {
    pub source: SettingsSource,
    pub found: bool,
}

/// Everything below the command line: the file and environment layers, plus the
/// record of where the loader looked.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    /// Lowest priority first: user config, project config, environment.
    pub layers: Vec<LoadedLayer>,
    /// Every path considered, in the order it was considered. Empty when
    /// `--no-config` skipped the search.
    pub searched: Vec<SearchedPath>,
}

impl Loaded {
    /// Every layer this run has, lowest priority first, with the command line's
    /// own opinion on top.
    ///
    /// The one place the full stack is assembled, and the reason it returns
    /// [`LoadedLayer`] rather than bare opinions: `mmm config show` answers
    /// "where did this value come from?" by walking exactly the list that
    /// [`Settings::resolve`] folded. Two constructions of it would be two
    /// chances for the explanation to name a layer the run did not use.
    ///
    /// The command line goes on unconditionally, even when it said nothing — an
    /// empty layer claims no key, so it can never be blamed for a value it did
    /// not set.
    pub fn stack(&self, command_line: PartialSettings) -> Vec<LoadedLayer> {
        let mut stack = self.layers.clone();
        stack.push(LoadedLayer {
            source: SettingsSource::CommandLine,
            settings: command_line,
        });
        stack
    }
}

/// Fold a layer stack into the settings a run uses.
///
/// The counterpart of [`Loaded::stack`]: it takes the annotated layers so that
/// the values a run reads and the sources `mmm config show` reports are derived
/// from the same list.
#[must_use]
pub fn resolve_stack(stack: &[LoadedLayer]) -> Settings {
    Settings::resolve(stack.iter().map(|layer| layer.settings.clone()))
}

/// Discover, read and order every layer below the command line.
///
/// Ascending priority, which is the precedence rule and the only place it is
/// written down: user config, then project config, then the environment.
///
/// # Errors
///
/// Any [`ConfigError`]. A config that cannot be read or understood stops the
/// run — there is no silent fallback to the defaults, because the defaults are
/// not what was asked for and the difference between them moves files.
pub fn load(options: &LoadOptions) -> Result<Loaded, ConfigError> {
    let mut loaded = Loaded::default();

    match (&options.explicit, options.no_config) {
        (Some(_), true) => return Err(ConfigError::Contradiction),

        // `--config` replaces discovery rather than adding to it: a file named
        // on the command line is the answer to "what settings is this run
        // using?", and one that still inherited from `$HOME` would not be.
        (Some(path), false) => {
            let settings = load_file(path)?;
            loaded.searched.push(SearchedPath {
                source: SettingsSource::ExplicitConfig(path.clone()),
                found: true,
            });
            loaded.layers.push(LoadedLayer {
                source: SettingsSource::ExplicitConfig(path.clone()),
                settings,
            });
        }

        (None, true) => {}

        (None, false) => {
            if let Some(path) = &options.user_config {
                let settings = load_optional_file(path)?;
                loaded.searched.push(SearchedPath {
                    source: SettingsSource::UserConfig(path.clone()),
                    found: settings.is_some(),
                });
                if let Some(settings) = settings {
                    loaded.layers.push(LoadedLayer {
                        source: SettingsSource::UserConfig(path.clone()),
                        settings,
                    });
                }
            }

            if let Some(start) = &options.start_dir {
                let search = find_project_config(start);
                for candidate in &search.candidates {
                    loaded.searched.push(SearchedPath {
                        source: SettingsSource::ProjectConfig(candidate.clone()),
                        found: search.found.as_ref() == Some(candidate),
                    });
                }
                if let Some(path) = search.found {
                    let settings = load_file(&path)?;
                    loaded.layers.push(LoadedLayer {
                        source: SettingsSource::ProjectConfig(path),
                        settings,
                    });
                }
            }
        }
    }

    // The environment outranks every file and survives `--no-config`: it belongs
    // to this invocation, the way a flag does, and skipping files is a statement
    // about files.
    let environment = env_layer(options.env.iter().cloned())?;
    if !environment.is_empty() {
        loaded.layers.push(LoadedLayer {
            source: SettingsSource::Environment,
            settings: environment,
        });
    }

    Ok(loaded)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;

    /// Deserialise a layer from JSON.
    ///
    /// JSON rather than TOML because the file format is the *next* task's
    /// concern and `serde_json` is already a dependency: what is under test
    /// here is the derive — which keys are accepted, which are refused, and
    /// what a missing key becomes — and that is the same derive whichever
    /// self-describing format drives it.
    fn layer(json: &str) -> PartialSettings {
        serde_json::from_str(json).unwrap()
    }

    fn layer_err(json: &str) -> String {
        serde_json::from_str::<PartialSettings>(json)
            .expect_err("this layer was supposed to be refused")
            .to_string()
    }

    #[test]
    fn defaults_describe_the_behaviour_the_organiser_already_has() {
        let settings = Settings::default();
        assert_eq!(settings.chunk_size, 100);
        assert!(!settings.no_prompt);
        assert_eq!(settings.verbose, 0);
        assert!(settings.include_location);
        assert_eq!(settings.duplicates_dir, PathBuf::from("duplicates"));
        assert_eq!(settings.unsorted_dir, PathBuf::from("unsorted"));
        assert!(settings.skip_patterns.is_empty());
        assert_eq!(settings.output_dir, None);
        assert_eq!(settings.journal_dir, None);
    }

    /// One directory per day. The nested form is a supported choice, not the
    /// default — see [`DEFAULT_DATE_DIRECTORY_FORMAT`].
    #[test]
    fn the_default_date_layout_is_the_flat_one() {
        assert_eq!(Settings::default().date_directory_format, "%Y-%m-%d");
        assert!(
            !Settings::default().date_directory_format.contains('/'),
            "a default that nested the tree would split every existing library in two"
        );
    }

    #[test]
    fn the_default_filename_format_spells_what_the_organiser_produces() {
        assert_eq!(
            Settings::default().filename_format,
            "{date}-{time}{location}.{ext}"
        );
    }

    /// The extension lists are the scanner's, not a second copy of them.
    #[test]
    fn default_extensions_come_from_the_scanner() {
        let settings = Settings::default();
        assert_eq!(settings.extensions.image.len(), IMAGE_EXTENSIONS.len());
        assert_eq!(settings.extensions.video.len(), VIDEO_EXTENSIONS.len());
        assert!(settings.extensions.image.contains(&"jpg".to_string()));
        assert!(settings.extensions.video.contains(&"mov".to_string()));
    }

    #[test]
    fn no_layers_at_all_resolves_to_the_defaults() {
        assert_eq!(Settings::resolve(Vec::new()), Settings::default());
    }

    // -----------------------------------------------------------------
    // The merge algebra
    // -----------------------------------------------------------------

    #[test]
    fn the_higher_priority_layer_wins_where_it_has_an_opinion() {
        let merged = layer(r#"{"chunk_size": 10}"#).merge(layer(r#"{"chunk_size": 25}"#));
        assert_eq!(merged.chunk_size, Some(25));
    }

    /// The property the whole design exists for: silence is not agreement. A
    /// higher layer that says nothing must not overwrite a lower one with a
    /// value nobody wrote down.
    #[test]
    fn a_silent_layer_does_not_overwrite_the_one_below_it() {
        let merged = layer(r#"{"chunk_size": 10, "no_prompt": true}"#).merge(layer("{}"));
        assert_eq!(merged.chunk_size, Some(10));
        assert_eq!(merged.no_prompt, Some(true));
    }

    #[test]
    fn merging_is_field_wise_not_wholesale() {
        let merged = layer(r#"{"chunk_size": 10, "verbose": 3}"#)
            .merge(layer(r#"{"filename_format": "{date}.{ext}"}"#));
        assert_eq!(merged.chunk_size, Some(10), "untouched by the higher layer");
        assert_eq!(merged.verbose, Some(3));
        assert_eq!(merged.filename_format.as_deref(), Some("{date}.{ext}"));
    }

    /// A `false` in a higher layer is an opinion, not an absence. This is the
    /// bug an `unwrap_or_default()` in `merge` would introduce, and it would be
    /// invisible until someone tried to switch a boolean back off from a
    /// project file.
    #[test]
    fn a_false_in_a_higher_layer_beats_a_true_below_it() {
        let merged = layer(r#"{"no_prompt": true}"#).merge(layer(r#"{"no_prompt": false}"#));
        assert_eq!(merged.no_prompt, Some(false));
    }

    /// Adding one video extension must not discard the image list somebody
    /// spent an afternoon on.
    #[test]
    fn extensions_merge_field_wise_across_layers() {
        let merged = layer(r#"{"extensions": {"image": ["jpg", "heic"]}}"#)
            .merge(layer(r#"{"extensions": {"video": ["mov"]}}"#));
        let extensions = merged.extensions.unwrap();
        assert_eq!(
            extensions.image,
            Some(vec!["jpg".to_string(), "heic".to_string()])
        );
        assert_eq!(extensions.video, Some(vec!["mov".to_string()]));
    }

    #[test]
    fn a_higher_layer_replaces_an_extension_list_rather_than_extending_it() {
        let merged = layer(r#"{"extensions": {"image": ["jpg", "png"]}}"#)
            .merge(layer(r#"{"extensions": {"image": ["dng"]}}"#));
        assert_eq!(
            merged.extensions.unwrap().image,
            Some(vec!["dng".to_string()]),
            "a list that could only grow could never be narrowed"
        );
    }

    /// The same rule for the flat lists — and the case that proves it is worth
    /// having: emptying one.
    #[test]
    fn a_higher_layer_can_empty_a_list() {
        let merged =
            layer(r#"{"skip_patterns": ["*.tmp"]}"#).merge(layer(r#"{"skip_patterns": []}"#));
        assert_eq!(merged.skip_patterns, Some(Vec::new()));
    }

    // -----------------------------------------------------------------
    // Formats are refused by the layer that carries them
    // -----------------------------------------------------------------

    /// The rule the task names, applied where the value is read rather than
    /// after the fold — so a broken pattern in a layer a higher one would have
    /// overridden is still a broken config file.
    #[test]
    fn a_layer_refuses_a_date_format_that_could_leave_the_output_tree() {
        assert!(
            layer_err(r#"{"date_directory_format": "/%Y/%m"}"#).contains("absolute path"),
            "an absolute pattern must be named as one"
        );
        assert!(layer_err(r#"{"date_directory_format": "%Y/../%m"}"#).contains(".."));
        assert!(layer_err(r#"{"date_directory_format": "%Y/%Q"}"#).contains("strftime"));
    }

    #[test]
    fn a_layer_refuses_a_filename_format_that_is_not_one_filename() {
        assert!(
            layer_err(r#"{"filename_format": "{date}/{time}.{ext}"}"#).contains("path separator")
        );
        assert!(layer_err(r#"{"filename_format": "{date}-{time}"}"#).contains("{ext}"));
        assert!(layer_err(r#"{"filename_format": "{stem}.{ext}"}"#).contains("unknown token"));
    }

    /// And the patterns a person would actually write are accepted, so the
    /// refusals above are a rule rather than a wall.
    #[test]
    fn a_layer_accepts_the_formats_the_documentation_offers() {
        let settings = Settings::resolve([layer(
            r#"{"date_directory_format": "%Y/%Y-%m", "filename_format": "{original_stem}-{date}.{ext}"}"#,
        )]);
        assert_eq!(settings.date_directory_format, "%Y/%Y-%m");
        assert_eq!(settings.filename_format, "{original_stem}-{date}.{ext}");
        assert!(settings.layout().is_ok());
    }

    /// A config file gets the file, the line and the column, because that is
    /// what the reader has to go and edit.
    #[test]
    fn a_broken_format_in_a_file_is_reported_at_its_position() {
        let error = parse_layer(
            "chunk_size = 10\ndate_directory_format = \"/%Y\"\n",
            Path::new("mmm.toml"),
        )
        .expect_err("an absolute date format must be refused");

        let message = error.to_string();
        assert!(message.starts_with("mmm.toml:2:"), "got {message}");
        assert!(message.contains("absolute path"), "got {message}");
    }

    /// The environment applies the same rule and names the variable, which is
    /// its equivalent of a line number.
    #[test]
    fn a_broken_format_in_the_environment_names_the_variable() {
        let error = env_layer([(
            "MMM_FILENAME_FORMAT".to_string(),
            "{date}/{time}.{ext}".to_string(),
        )])
        .expect_err("a filename format with a separator must be refused");

        let message = error.to_string();
        assert!(message.starts_with("MMM_FILENAME_FORMAT:"), "got {message}");
        assert!(message.contains("path separator"), "got {message}");

        assert!(
            env_layer([(
                "MMM_DATE_DIRECTORY_FORMAT".to_string(),
                "%Y/%m/%d".to_string()
            )])
            .is_ok(),
            "a pattern that is fine in a file must be fine in the environment"
        );
    }

    /// The two directories are subject to the same containment rule as the
    /// dated path, and for a blunter reason: `unsorted_dir = "/etc"` is one line
    /// that would file photographs outside the tree the run was pointed at.
    /// Refused by the layer that carries it, so the position is the value's own.
    #[test]
    fn a_subdirectory_that_could_leave_the_output_tree_is_refused_at_its_position() {
        let error = parse_layer("unsorted_dir = \"/etc\"\n", Path::new("mmm.toml"))
            .expect_err("an absolute unsorted_dir must be refused");
        let message = error.to_string();
        assert!(message.starts_with("mmm.toml:1:"), "got {message}");
        assert!(message.contains("unsorted_dir"), "got {message}");
        assert!(message.contains("absolute path"), "got {message}");

        let error = parse_layer(
            "chunk_size = 10\nduplicates_dir = \"../dupes\"\n",
            Path::new("mmm.toml"),
        )
        .expect_err("a duplicates_dir walking out of the tree must be refused");
        let message = error.to_string();
        assert!(message.starts_with("mmm.toml:2:"), "got {message}");
        assert!(message.contains("duplicates_dir"), "got {message}");
    }

    /// The environment is not a hole through the rule above.
    #[test]
    fn a_subdirectory_from_the_environment_is_refused_and_names_the_variable() {
        let error = env_layer([("MMM_UNSORTED_DIR".to_string(), "/etc".to_string())])
            .expect_err("an absolute unsorted_dir must be refused");
        let message = error.to_string();
        assert!(message.starts_with("MMM_UNSORTED_DIR:"), "got {message}");
        assert!(message.contains("absolute path"), "got {message}");

        assert!(
            env_layer([("MMM_DUPLICATES_DIR".to_string(), "copies".to_string())]).is_ok(),
            "an ordinary name must still be accepted"
        );
    }

    /// A skip pattern that will not compile is a config error, not a pattern
    /// that quietly matches nothing.
    #[test]
    fn a_skip_pattern_that_is_not_a_glob_is_refused() {
        let error = parse_layer("skip_patterns = [\"[unclosed\"]\n", Path::new("mmm.toml"))
            .expect_err("a malformed glob must be refused");
        let message = error.to_string();
        assert!(message.starts_with("mmm.toml:1:"), "got {message}");
        assert!(message.contains("skip_patterns"), "got {message}");

        let error = env_layer([("MMM_SKIP_PATTERNS".to_string(), "*.tmp,[bad".to_string())])
            .expect_err("a malformed glob must be refused from the environment too");
        assert!(
            error.to_string().starts_with("MMM_SKIP_PATTERNS:"),
            "got {error}"
        );

        assert!(
            parse_layer(
                "skip_patterns = [\"*.tmp\", \"raw/**\"]\n",
                Path::new("mmm.toml")
            )
            .is_ok(),
            "the patterns the documentation offers must be accepted"
        );
    }

    /// The settings the organiser and the scanner actually read come off the
    /// resolved struct, so a key that resolves and never reaches them is the
    /// bug this asserts against.
    #[test]
    fn the_resolved_settings_build_the_layout_and_the_filter_they_name() {
        let settings = Settings::resolve([layer(
            r#"{"unsorted_dir": "no-date", "duplicates_dir": "copies",
                "skip_patterns": ["*.tmp"],
                "extensions": {"image": ["dng"], "video": ["insv"]}}"#,
        )]);

        let layout = settings.layout().expect("a valid layout");
        assert_eq!(layout.unsorted(), Path::new("no-date"));
        assert_eq!(layout.duplicates(), Path::new("copies"));
        settings.scan_filter().expect("a valid scan filter");
    }

    /// The built-in defaults are themselves a scheme, and a default that could
    /// not be built would break every run rather than only a configured one.
    #[test]
    fn the_default_settings_resolve_to_a_scheme() {
        let layout = Settings::default()
            .layout()
            .expect("the built-in defaults must be valid");
        assert!(layout.include_location());
        assert_eq!(layout.unsorted(), Path::new("unsorted"));
        assert_eq!(layout.duplicates(), Path::new("duplicates"));
    }

    // -----------------------------------------------------------------
    // Resolution
    // -----------------------------------------------------------------

    /// Layers arrive lowest-priority-first, and the last one wins.
    #[test]
    fn resolve_applies_the_layers_in_ascending_priority() {
        let settings = Settings::resolve([
            layer(r#"{"chunk_size": 10}"#),  // user config
            layer(r#"{"chunk_size": 25}"#),  // project config
            layer(r#"{"chunk_size": 50}"#),  // environment
            layer(r#"{"chunk_size": 500}"#), // command line
        ]);
        assert_eq!(settings.chunk_size, 500);
    }

    /// Each layer contributes what it alone said, and the defaults fill the
    /// rest — the worked precedence example, as a test.
    #[test]
    fn every_layer_contributes_what_it_alone_claimed() {
        let settings = Settings::resolve([
            layer(r#"{"chunk_size": 10, "include_location": false}"#),
            layer(r#"{"unsorted_dir": "no-date"}"#),
            layer(r#"{"no_prompt": true}"#),
            layer(r#"{"verbose": 2}"#),
        ]);

        assert_eq!(settings.chunk_size, 10, "from the lowest layer");
        assert!(!settings.include_location);
        assert_eq!(settings.unsorted_dir, PathBuf::from("no-date"));
        assert!(settings.no_prompt);
        assert_eq!(settings.verbose, 2);
        assert_eq!(
            settings.duplicates_dir,
            PathBuf::from("duplicates"),
            "nobody mentioned it, so the default applies"
        );
    }

    /// Defaults are applied last, to the fields still unclaimed — never as a
    /// layer that later ones have to argue with.
    #[test]
    fn defaults_fill_only_what_no_layer_claimed() {
        let settings = Settings::resolve([layer(r#"{"filename_format": "{date}.{ext}"}"#)]);
        assert_eq!(settings.filename_format, "{date}.{ext}");
        assert_eq!(settings.date_directory_format, "%Y-%m-%d");
        assert_eq!(settings.chunk_size, 100);
    }

    /// A partially-specified `[extensions]` table takes the default for the
    /// half it did not mention, rather than an empty list — which would mean a
    /// config adding a RAW format silently stopped the tool finding any video.
    #[test]
    fn a_half_specified_extensions_table_keeps_the_default_other_half() {
        let settings = Settings::resolve([layer(r#"{"extensions": {"image": ["dng"]}}"#)]);
        assert_eq!(settings.extensions.image, vec!["dng".to_string()]);
        assert_eq!(settings.extensions.video, Extensions::default().video);
    }

    // -----------------------------------------------------------------
    // What a layer is allowed to say
    // -----------------------------------------------------------------

    #[test]
    fn an_omitted_key_is_an_absence_not_an_error() {
        assert_eq!(layer("{}"), PartialSettings::default());
        assert!(layer("{}").is_empty());
        assert!(!layer(r#"{"chunk_size": 1}"#).is_empty());
    }

    /// A typo is refused and named. The alternative — ignoring it — is a user
    /// changing a value, seeing no difference, and concluding the setting does
    /// not work.
    #[test]
    fn an_unknown_key_is_refused_and_named() {
        let err = layer_err(r#"{"chunck_size": 10}"#);
        assert!(err.contains("chunck_size"), "{err}");
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn an_unknown_key_inside_the_extensions_table_is_refused_too() {
        let err = layer_err(r#"{"extensions": {"audio": ["mp3"]}}"#);
        assert!(err.contains("audio"), "{err}");
    }

    /// The safety property, asserted at the layer that would have to break it.
    /// `commit` is not a field, so `deny_unknown_fields` refuses the file — a
    /// config that could turn on moving files would undo the whole point of
    /// requiring `--commit` at the command line.
    #[test]
    fn commit_cannot_be_set_from_a_layer() {
        let err = layer_err(r#"{"commit": true}"#);
        assert!(err.contains("commit"), "{err}");
    }

    /// Same reasoning, same mechanism: the two flags that make a run
    /// unreversible are command-line-only as well.
    #[test]
    fn the_unsafe_flags_cannot_be_set_from_a_layer_either() {
        assert!(layer_err(r#"{"no_journal": true}"#).contains("no_journal"));
        assert!(layer_err(r#"{"i_know_what_im_doing": true}"#).contains("i_know_what_im_doing"));
    }

    // -----------------------------------------------------------------
    // Reading one file
    // -----------------------------------------------------------------

    use std::fs;
    use tempfile::TempDir;

    /// A temporary tree, canonicalised.
    ///
    /// Canonicalised because [`find_project_config`] canonicalises its start,
    /// and on macOS `/var` is a symlink to `/private/var` — so a test that
    /// compared against the raw `TempDir` path would fail on the platform
    /// rather than on the behaviour.
    fn temp_tree() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Parse a file's text and insist it was refused, returning the error.
    fn parse_err(text: &str) -> ConfigError {
        parse_layer(text, Path::new("/tmp/mmm.toml"))
            .expect_err("this config was supposed to be refused")
    }

    #[test]
    fn a_config_file_becomes_a_layer() {
        let layer = parse_layer(
            "chunk_size = 25\nno_prompt = true\nskip_patterns = [\"*.tmp\"]\n",
            Path::new("/tmp/mmm.toml"),
        )
        .unwrap();
        assert_eq!(layer.chunk_size, Some(25));
        assert_eq!(layer.no_prompt, Some(true));
        assert_eq!(layer.skip_patterns, Some(vec!["*.tmp".to_string()]));
        assert_eq!(layer.verbose, None, "a key nobody wrote stays unclaimed");
    }

    #[test]
    fn an_extensions_table_parses_from_toml() {
        let layer = parse_layer(
            "[extensions]\nimage = [\"jpg\", \"dng\"]\n",
            Path::new("/tmp/mmm.toml"),
        )
        .unwrap();
        let extensions = layer.extensions.unwrap();
        assert_eq!(
            extensions.image,
            Some(vec!["jpg".to_string(), "dng".to_string()])
        );
        assert_eq!(extensions.video, None);
    }

    /// The error has to say where to go and look, or the reader is left
    /// bisecting their own config file.
    #[test]
    fn malformed_toml_names_the_file_and_the_position() {
        let ConfigError::Parse {
            path,
            line,
            column,
            message,
        } = parse_err("chunk_size = 25\nno_prompt = \n")
        else {
            panic!("malformed TOML must be a parse error");
        };
        assert_eq!(path, PathBuf::from("/tmp/mmm.toml"));
        assert_eq!(line, 2, "the offending line, not the first one");
        assert!(column >= 1);
        assert!(!message.is_empty());
    }

    #[test]
    fn an_unknown_key_in_a_file_is_refused_and_named() {
        let ConfigError::Parse { line, message, .. } =
            parse_err("chunk_size = 25\nchunck_size = 30\n")
        else {
            panic!("an unknown key must be a parse error");
        };
        assert!(message.contains("chunck_size"), "{message}");
        assert_eq!(line, 2);
    }

    /// The safety property, at the layer that would have to break it — and with
    /// a message that explains itself rather than listing the eleven fields that
    /// are settings.
    #[test]
    fn commit_in_a_file_is_refused_with_the_reason() {
        let ConfigError::Parse { message, .. } = parse_err("commit = true\n") else {
            panic!("commit in a config file must be refused");
        };
        assert!(message.contains("commit"), "{message}");
        assert!(message.contains("command line"), "{message}");
        assert!(
            !message.contains("expected one of"),
            "the refusal must explain itself, not list the fields that are settings: {message}"
        );
    }

    #[test]
    fn the_other_command_line_only_keys_are_refused_with_a_reason_too() {
        for key in ["no_journal", "i_know_what_im_doing"] {
            let ConfigError::Parse { message, .. } = parse_err(&format!("{key} = true\n")) else {
                panic!("{key} in a config file must be refused");
            };
            assert!(message.contains(key), "{message}");
            assert!(message.contains("command line"), "{message}");
        }
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused() {
        let ConfigError::Parse { message, .. } = parse_err("chunk_size = \"lots\"\n") else {
            panic!("a string where a number belongs must be refused");
        };
        assert!(!message.is_empty(), "{message}");
    }

    #[test]
    fn a_missing_file_is_missing_when_it_was_asked_for_and_absent_when_it_was_discovered() {
        let (_dir, root) = temp_tree();
        let path = root.join("nowhere.toml");

        let err = load_file(&path).expect_err("--config must not invent a file");
        assert!(matches!(err, ConfigError::Missing { .. }));
        assert!(err.to_string().contains("nowhere.toml"), "{err}");

        assert_eq!(
            load_optional_file(&path).unwrap(),
            None,
            "a discovered file that is not there is the ordinary case"
        );
    }

    /// A file that exists and cannot be parsed is an error on *both* paths. The
    /// optional reading is about absence, never about giving up.
    #[test]
    fn a_broken_discovered_file_is_still_an_error() {
        let (_dir, root) = temp_tree();
        let path = root.join("mmm.toml");
        write(&path, "chunk_size = = 3\n");
        assert!(matches!(
            load_optional_file(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    // -----------------------------------------------------------------
    // Finding the files
    // -----------------------------------------------------------------

    #[test]
    fn xdg_config_home_wins_over_the_platform_directory() {
        assert_eq!(
            user_config_path_from(
                Some(PathBuf::from("/elsewhere")),
                Some(PathBuf::from("/platform"))
            ),
            Some(PathBuf::from("/elsewhere/mmm/config.toml"))
        );
    }

    /// A relative `XDG_CONFIG_HOME` is ignored, per the specification: a config
    /// directory that moved with the process's working directory would be worse
    /// than not having one.
    #[test]
    fn a_relative_xdg_config_home_is_ignored() {
        assert_eq!(
            user_config_path_from(
                Some(PathBuf::from("relative/config")),
                Some(PathBuf::from("/platform"))
            ),
            Some(PathBuf::from("/platform/mmm/config.toml"))
        );
    }

    #[test]
    fn with_no_home_at_all_there_is_no_user_config() {
        assert_eq!(user_config_path_from(None, None), None);
    }

    #[test]
    fn a_project_config_is_found_from_a_nested_subdirectory() {
        let (_dir, root) = temp_tree();
        write(&root.join("mmm.toml"), "chunk_size = 7\n");
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        let search = find_project_config(&nested);
        assert_eq!(search.found, Some(root.join("mmm.toml")));
        assert!(
            search.candidates.contains(&nested.join("mmm.toml")),
            "the walk starts where it was told to"
        );
    }

    /// Nearest wins, or a config in a subdirectory could never override the one
    /// above it — which is the only reason to allow nesting at all.
    #[test]
    fn the_nearest_project_config_wins() {
        let (_dir, root) = temp_tree();
        write(&root.join("mmm.toml"), "chunk_size = 7\n");
        let nested = root.join("a/b");
        write(&nested.join("mmm.toml"), "chunk_size = 9\n");

        assert_eq!(
            find_project_config(&nested).found,
            Some(nested.join("mmm.toml"))
        );
    }

    #[test]
    fn the_dotted_project_config_name_is_accepted() {
        let (_dir, root) = temp_tree();
        write(&root.join(".mmm.toml"), "chunk_size = 7\n");
        assert_eq!(
            find_project_config(&root).found,
            Some(root.join(".mmm.toml"))
        );
    }

    /// Both names in one directory is answered by the first, and the search
    /// stops — merging two files that disagree would need a winner nobody could
    /// predict.
    #[test]
    fn the_undotted_name_is_searched_first() {
        let (_dir, root) = temp_tree();
        write(&root.join("mmm.toml"), "chunk_size = 7\n");
        write(&root.join(".mmm.toml"), "chunk_size = 9\n");

        let search = find_project_config(&root);
        assert_eq!(search.found, Some(root.join("mmm.toml")));
        assert!(
            !search.candidates.contains(&root.join(".mmm.toml")),
            "the walk stops at the first hit, and says so"
        );
    }

    #[test]
    fn no_project_config_anywhere_is_an_answer_not_an_error() {
        let (_dir, root) = temp_tree();
        let search = find_project_config(&root);
        assert_eq!(search.found, None);
        assert!(
            !search.candidates.is_empty(),
            "it still records where it looked"
        );
    }

    // -----------------------------------------------------------------
    // The environment layer
    // -----------------------------------------------------------------

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn environment_variables_become_a_layer() {
        let layer = env_layer(env(&[
            ("MMM_CHUNK_SIZE", "25"),
            ("MMM_OUTPUT_DIR", "/sorted"),
            ("MMM_NO_PROMPT", "true"),
        ]))
        .unwrap();
        assert_eq!(layer.chunk_size, Some(25));
        assert_eq!(layer.output_dir, Some(PathBuf::from("/sorted")));
        assert_eq!(layer.no_prompt, Some(true));
    }

    #[test]
    fn variables_without_the_prefix_are_not_ours() {
        let layer = env_layer(env(&[("PATH", "/usr/bin"), ("HOME", "/home/x")])).unwrap();
        assert!(layer.is_empty());
    }

    #[test]
    fn environment_lists_are_comma_separated_and_can_be_emptied() {
        let layer = env_layer(env(&[
            ("MMM_SKIP_PATTERNS", "*.tmp, */cache/*"),
            ("MMM_EXTENSIONS_IMAGE", "jpg,dng"),
        ]))
        .unwrap();
        assert_eq!(
            layer.skip_patterns,
            Some(vec!["*.tmp".to_string(), "*/cache/*".to_string()])
        );
        assert_eq!(
            layer.extensions.unwrap().image,
            Some(vec!["jpg".to_string(), "dng".to_string()])
        );

        let cleared = env_layer(env(&[("MMM_SKIP_PATTERNS", "")])).unwrap();
        assert_eq!(cleared.skip_patterns, Some(Vec::new()));
    }

    /// Naming one half of the table must not blank the other, the same as in a
    /// file.
    #[test]
    fn a_lone_extensions_variable_leaves_the_other_half_unclaimed() {
        let layer = env_layer(env(&[("MMM_EXTENSIONS_VIDEO", "mov")])).unwrap();
        let extensions = layer.extensions.unwrap();
        assert_eq!(extensions.video, Some(vec!["mov".to_string()]));
        assert_eq!(extensions.image, None);
    }

    #[test]
    fn an_unknown_environment_variable_is_refused_and_named() {
        let err = env_layer(env(&[("MMM_CHUNCK_SIZE", "25")]))
            .expect_err("a typo that does nothing looks like a setting that does not work");
        assert!(err.to_string().contains("MMM_CHUNCK_SIZE"), "{err}");
    }

    /// The safety property again, at the other door into the settings.
    #[test]
    fn commit_cannot_be_set_from_the_environment() {
        for key in ["MMM_COMMIT", "MMM_NO_JOURNAL", "MMM_I_KNOW_WHAT_IM_DOING"] {
            let err = env_layer(env(&[(key, "true")]))
                .expect_err("an unsafe run must not be arrangeable from the environment");
            assert!(err.to_string().contains("command line"), "{err}");
        }
    }

    #[test]
    fn a_value_of_the_wrong_type_in_the_environment_names_the_variable() {
        let err = env_layer(env(&[("MMM_CHUNK_SIZE", "lots")])).unwrap_err();
        assert!(err.to_string().contains("MMM_CHUNK_SIZE"), "{err}");
        assert!(err.to_string().contains("whole number"), "{err}");

        let err = env_layer(env(&[("MMM_NO_PROMPT", "maybe")])).unwrap_err();
        assert!(err.to_string().contains("MMM_NO_PROMPT"), "{err}");
        assert!(err.to_string().contains("true or false"), "{err}");
    }

    #[test]
    fn the_accepted_boolean_spellings_are_the_documented_ones() {
        for (value, expected) in [("true", true), ("1", true), ("false", false), ("0", false)] {
            let layer = env_layer(env(&[("MMM_NO_PROMPT", value)])).unwrap();
            assert_eq!(layer.no_prompt, Some(expected), "{value}");
        }
    }

    // -----------------------------------------------------------------
    // Assembling the layers
    // -----------------------------------------------------------------

    /// What a load resolves to with nothing on the command line.
    ///
    /// Through [`Loaded::stack`] rather than around it, so these assertions
    /// cover the same assembly `main` and `mmm config show` use.
    fn resolved(loaded: &Loaded) -> Settings {
        resolve_stack(&loaded.stack(PartialSettings::default()))
    }

    /// A tree with a user config and a project config, and the options that
    /// find them — with nothing read from the real environment.
    fn tree_with_both_configs(user: &str, project: &str) -> (TempDir, PathBuf, LoadOptions) {
        let (dir, root) = temp_tree();
        let user_config = root.join("home/.config/mmm/config.toml");
        write(&user_config, user);
        let work = root.join("project/nested");
        fs::create_dir_all(&work).unwrap();
        write(&root.join("project/mmm.toml"), project);

        let options = LoadOptions {
            start_dir: Some(work),
            user_config: Some(user_config),
            ..LoadOptions::default()
        };
        (dir, root, options)
    }

    #[test]
    fn the_file_layers_arrive_in_ascending_priority() {
        let (_dir, root, options) =
            tree_with_both_configs("chunk_size = 10\nverbose = 3\n", "chunk_size = 25\n");
        let loaded = load(&options).unwrap();

        assert_eq!(
            loaded
                .layers
                .iter()
                .map(|layer| layer.source.clone())
                .collect::<Vec<_>>(),
            vec![
                SettingsSource::UserConfig(root.join("home/.config/mmm/config.toml")),
                SettingsSource::ProjectConfig(root.join("project/mmm.toml")),
            ]
        );

        let settings = resolved(&loaded);
        assert_eq!(
            settings.chunk_size, 25,
            "the project config outranks the user's"
        );
        assert_eq!(settings.verbose, 3, "and leaves what it did not mention");
    }

    #[test]
    fn the_environment_outranks_every_file() {
        let (_dir, _root, mut options) =
            tree_with_both_configs("chunk_size = 10\n", "chunk_size = 25\n");
        options.env = env(&[("MMM_CHUNK_SIZE", "50")]);

        let loaded = load(&options).unwrap();
        assert_eq!(
            loaded.layers.last().unwrap().source,
            SettingsSource::Environment
        );
        assert_eq!(resolved(&loaded).chunk_size, 50);
    }

    #[test]
    fn no_config_skips_both_discovered_files() {
        let (_dir, _root, mut options) =
            tree_with_both_configs("chunk_size = 10\n", "chunk_size = 25\n");
        options.no_config = true;

        let loaded = load(&options).unwrap();
        assert!(loaded.layers.is_empty());
        assert!(
            loaded.searched.is_empty(),
            "nothing was searched, so nothing is reported"
        );
        assert_eq!(resolved(&loaded), Settings::default());
    }

    /// `--no-config` is a statement about files. The environment belongs to the
    /// invocation, the way a flag does, and survives it.
    #[test]
    fn no_config_does_not_silence_the_environment() {
        let (_dir, _root, mut options) =
            tree_with_both_configs("chunk_size = 10\n", "chunk_size = 25\n");
        options.no_config = true;
        options.env = env(&[("MMM_CHUNK_SIZE", "50")]);

        assert_eq!(resolved(&load(&options).unwrap()).chunk_size, 50);
    }

    #[test]
    fn an_explicit_config_replaces_discovery_entirely() {
        let (_dir, root, mut options) =
            tree_with_both_configs("chunk_size = 10\nverbose = 3\n", "chunk_size = 25\n");
        let explicit = root.join("elsewhere.toml");
        write(&explicit, "chunk_size = 99\n");
        options.explicit = Some(explicit.clone());

        let loaded = load(&options).unwrap();
        assert_eq!(
            loaded
                .layers
                .iter()
                .map(|layer| layer.source.clone())
                .collect::<Vec<_>>(),
            vec![SettingsSource::ExplicitConfig(explicit)]
        );

        let settings = resolved(&loaded);
        assert_eq!(settings.chunk_size, 99);
        assert_eq!(
            settings.verbose, 0,
            "the user config was not consulted, so its verbosity is not inherited"
        );
    }

    #[test]
    fn an_explicit_config_that_is_not_there_stops_the_run() {
        let (_dir, root, mut options) = tree_with_both_configs("", "chunk_size = 25\n");
        options.explicit = Some(root.join("nowhere.toml"));

        let err = load(&options).expect_err("naming a file that is not there must not fall back");
        assert!(matches!(err, ConfigError::Missing { .. }));
        assert!(err.to_string().contains("nowhere.toml"), "{err}");
    }

    #[test]
    fn asking_for_a_config_and_for_none_is_a_contradiction() {
        let options = LoadOptions {
            explicit: Some(PathBuf::from("/tmp/mmm.toml")),
            no_config: true,
            ..LoadOptions::default()
        };
        assert!(matches!(load(&options), Err(ConfigError::Contradiction)));
    }

    /// A broken file stops the run wherever it was found. The alternative —
    /// carrying on with the defaults — would do something other than what the
    /// file asked for, to somebody's photo library, and report success.
    #[test]
    fn a_broken_project_config_stops_the_run() {
        let (_dir, _root, options) = tree_with_both_configs("", "chunk_size = = 3\n");
        assert!(matches!(load(&options), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn the_search_records_what_was_looked_at_and_what_was_there() {
        let (_dir, root, options) =
            tree_with_both_configs("chunk_size = 10\n", "chunk_size = 25\n");
        let loaded = load(&options).unwrap();

        let user = &loaded.searched[0];
        assert_eq!(
            user.source,
            SettingsSource::UserConfig(root.join("home/.config/mmm/config.toml"))
        );
        assert!(user.found);

        let hit = loaded
            .searched
            .iter()
            .filter(|entry| entry.found)
            .map(|entry| entry.source.clone())
            .collect::<Vec<_>>();
        assert!(hit.contains(&SettingsSource::ProjectConfig(
            root.join("project/mmm.toml")
        )));
        assert!(
            loaded.searched.len() > 2,
            "the directories walked past are part of the answer to `where did you look?`"
        );
    }

    #[test]
    fn a_user_config_that_is_not_there_is_reported_as_searched_and_absent() {
        let (_dir, root) = temp_tree();
        let options = LoadOptions {
            user_config: Some(root.join("home/.config/mmm/config.toml")),
            ..LoadOptions::default()
        };
        let loaded = load(&options).unwrap();
        assert!(loaded.layers.is_empty());
        assert_eq!(loaded.searched.len(), 1);
        assert!(!loaded.searched[0].found);
    }

    // -----------------------------------------------------------------
    // Naming the source
    // -----------------------------------------------------------------

    #[test]
    fn a_source_prints_the_way_the_annotations_need_it() {
        assert_eq!(
            SettingsSource::ProjectConfig(PathBuf::from("/work/mmm.toml")).to_string(),
            "project config (/work/mmm.toml)"
        );
        assert_eq!(SettingsSource::Environment.to_string(), "environment");
        assert_eq!(SettingsSource::CommandLine.to_string(), "command line");
        assert_eq!(SettingsSource::Defaults.to_string(), "built-in defaults");
    }

    #[test]
    fn only_the_file_sources_have_a_path() {
        assert_eq!(
            SettingsSource::UserConfig(PathBuf::from("/a/config.toml")).path(),
            Some(Path::new("/a/config.toml"))
        );
        assert_eq!(SettingsSource::Environment.path(), None);
    }
}
