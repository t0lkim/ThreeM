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
//! This module deliberately knows nothing about *where* layers come from. File
//! discovery, TOML parsing, and the `MMM_` environment variables arrive next and
//! all produce a `PartialSettings`; keeping the merge algebra ignorant of their
//! origin is what makes it testable without a filesystem.

use std::path::PathBuf;

use serde::Deserialize;

use crate::scanner::{IMAGE_EXTENSIONS, VIDEO_EXTENSIONS};

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
    pub date_directory_format: Option<String>,
    pub filename_format: Option<String>,
    pub include_location: Option<bool>,
    pub duplicates_dir: Option<PathBuf>,
    pub unsorted_dir: Option<PathBuf>,
    pub extensions: Option<PartialExtensions>,
    pub skip_patterns: Option<Vec<String>>,
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

impl Settings {
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
        }
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
}
