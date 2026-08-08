//! What `mmm config` prints and writes.
//!
//! Four questions, one catalogue. [`KEYS`] describes every setting once — its
//! name, the table it belongs to, what it does, how to tell whether a layer
//! claimed it, and how to render its resolved value as TOML — and all four
//! `config` actions are folded over that list:
//!
//! - `show` prints the resolved value of each key with the layer that decided it
//! - `path` prints where the loader looked and what it found
//! - `init` prints the same keys at their defaults, commented out
//! - `validate` reports what parsed
//!
//! One catalogue rather than four because the failure mode of four is a setting
//! that `show` reports, `init` never mentions, and the reader concludes does not
//! exist. A key added to [`crate::settings::Settings`] and not to [`KEYS`] fails
//! to compile a test in this module, which is the only enforcement that survives
//! somebody being in a hurry.
//!
//! Rendering lives here rather than in [`crate::reporter`] because every
//! function below returns a `String` the caller prints. That is what makes the
//! exact bytes — the source annotations, the comment convention in the starter
//! config — assertable without a subprocess.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use thiserror::Error;
use toml::Value;

use crate::settings::{
    Loaded, LoadedLayer, PartialSettings, Settings, SettingsSource, COMMAND_LINE_ONLY_KEYS,
    PROJECT_CONFIG_NAMES,
};

// =====================================================================
// The catalogue
// =====================================================================

/// One setting, described once for every command that has to talk about it.
pub struct SettingKey {
    /// The key as it is written in a config file.
    pub name: &'static str,

    /// The TOML table it sits in, or `None` for a top-level key.
    pub table: Option<&'static str>,

    /// What it does, in prose, for the starter config. May span lines.
    pub summary: &'static str,

    /// What happens when no layer sets it, for the two keys whose fallback is
    /// computed from the run rather than stored as a default.
    pub unset: Option<&'static str>,

    /// An illustrative value for those same two keys, so the starter config can
    /// still list every key as something the reader can uncomment.
    pub placeholder: Option<&'static str>,

    /// Whether a layer had an opinion about this key.
    claimed: fn(&PartialSettings) -> bool,

    /// This key's value in a resolved settings, or `None` when there is nothing
    /// to show because nothing was set and there is no default.
    value: fn(&Settings) -> Option<Value>,
}

impl SettingKey {
    /// Whether `layer` claimed this key.
    pub fn claimed_by(&self, layer: &PartialSettings) -> bool {
        (self.claimed)(layer)
    }

    /// This key's resolved value, as it would be written in a config file.
    pub fn value_in(&self, settings: &Settings) -> Option<Value> {
        (self.value)(settings)
    }

    /// The key as `mmm config show` labels it — table-qualified, so a reader
    /// grepping the output for `image` is told which one it is.
    pub fn qualified_name(&self) -> String {
        match self.table {
            Some(table) => format!("{table}.{}", self.name),
            None => self.name.to_string(),
        }
    }
}

/// A path as a TOML string.
///
/// Lossy because a path is bytes and TOML is text. A filename that is not UTF-8
/// therefore prints with replacement characters — which is wrong to copy back
/// into a config file, and still better than refusing to describe the run.
fn path_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

fn list_value(items: &[String]) -> Value {
    Value::Array(items.iter().cloned().map(Value::String).collect())
}

/// Every setting, in the order `mmm config show` and `mmm config init` print
/// them.
///
/// Top-level keys first and the `[extensions]` table last, because a TOML key
/// written after a table header belongs to that table. `no_settings_key_follows_a_table`
/// pins it.
pub const KEYS: &[SettingKey] = &[
    SettingKey {
        name: "output_dir",
        table: None,
        summary: "Where organised files are written.",
        unset: Some("the run writes into its first input directory"),
        placeholder: Some("/path/to/organised/library"),
        claimed: |layer| layer.output_dir.is_some(),
        value: |settings| settings.output_dir.as_deref().map(path_value),
    },
    SettingKey {
        name: "chunk_size",
        table: None,
        summary: "Files moved between prompts.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.chunk_size.is_some(),
        value: |settings| {
            Some(Value::Integer(
                i64::try_from(settings.chunk_size).unwrap_or(i64::MAX),
            ))
        },
    },
    SettingKey {
        name: "no_prompt",
        table: None,
        summary: "Do not stop to ask at chunk boundaries.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.no_prompt.is_some(),
        value: |settings| Some(Value::Boolean(settings.no_prompt)),
    },
    SettingKey {
        name: "verbose",
        table: None,
        summary: "Log verbosity: 0 warnings, 1 info, 2 debug, 3 trace.\n\
                  A verbosity set here cannot be turned back down with -v, only with --no-config.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.verbose.is_some(),
        value: |settings| Some(Value::Integer(i64::from(settings.verbose))),
    },
    SettingKey {
        name: "journal_dir",
        table: None,
        summary: "Where run journals are written, and where `mmm undo` reads them from.\n\
                  Both sides read this one key, so moving it moves them together.",
        unset: Some("journals go under <output>/.mmm/journal"),
        placeholder: Some("/var/log/mmm"),
        claimed: |layer| layer.journal_dir.is_some(),
        value: |settings| settings.journal_dir.as_deref().map(path_value),
    },
    SettingKey {
        name: "date_directory_format",
        table: None,
        summary: "The dated directory each file is filed under, strftime-style.\n\
                  The default is one directory per day; \"%Y/%m/%d\" nests it by year and month.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.date_directory_format.is_some(),
        value: |settings| Some(Value::String(settings.date_directory_format.clone())),
    },
    SettingKey {
        name: "filename_format",
        table: None,
        summary: "The name each file is given.\n\
                  Tokens: {date}, {time}, {location}, {ext}, {original_stem}. {location} carries \
                  its own separator and expands to nothing when a file has no coordinates.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.filename_format.is_some(),
        value: |settings| Some(Value::String(settings.filename_format.clone())),
    },
    SettingKey {
        name: "include_location",
        table: None,
        summary: "Append the geocoded place name to filenames.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.include_location.is_some(),
        value: |settings| Some(Value::Boolean(settings.include_location)),
    },
    SettingKey {
        name: "duplicates_dir",
        table: None,
        summary: "Where relocated duplicates are grouped, relative to the output tree.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.duplicates_dir.is_some(),
        value: |settings| Some(path_value(&settings.duplicates_dir)),
    },
    SettingKey {
        name: "unsorted_dir",
        table: None,
        summary: "Where files with no usable date go, relative to the output tree.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.unsorted_dir.is_some(),
        value: |settings| Some(path_value(&settings.unsorted_dir)),
    },
    SettingKey {
        name: "skip_patterns",
        table: None,
        summary: "Paths the scan passes over. Empty by default: skipping a photograph somebody \
                  expected to be organised is a surprise, so every skip has to be asked for.",
        unset: None,
        placeholder: None,
        claimed: |layer| layer.skip_patterns.is_some(),
        value: |settings| Some(list_value(&settings.skip_patterns)),
    },
    SettingKey {
        name: "default_timezone",
        table: None,
        summary: "Which wall clock a photo with no recorded offset is read against.\n\
                  A fixed offset (\"+08:00\") or an IANA name (\"Asia/Singapore\"). A file that \
                  carries its own offset tag is unaffected — the file always wins.",
        unset: Some("the machine's own timezone is used, and the run says so"),
        placeholder: Some("Asia/Singapore"),
        claimed: |layer| layer.default_timezone.is_some(),
        value: |settings| settings.default_timezone.clone().map(Value::String),
    },
    SettingKey {
        name: "image",
        table: Some("extensions"),
        summary: "Which extensions count as photographs — lowercase, no leading dot.\n\
                  A list replaces the default rather than adding to it, so name every extension \
                  you want scanned.",
        unset: None,
        placeholder: None,
        claimed: |layer| {
            layer
                .extensions
                .as_ref()
                .is_some_and(|table| table.image.is_some())
        },
        value: |settings| Some(list_value(&settings.extensions.image)),
    },
    SettingKey {
        name: "video",
        table: Some("extensions"),
        summary: "Which extensions count as video. Replaces rather than extends, as above.",
        unset: None,
        placeholder: None,
        claimed: |layer| {
            layer
                .extensions
                .as_ref()
                .is_some_and(|table| table.video.is_some())
        },
        value: |settings| Some(list_value(&settings.extensions.video)),
    },
];

/// The layer that decided `key`, or [`SettingsSource::Defaults`] if none did.
///
/// Walks the stack backwards and stops at the first claim, which is the same
/// rule [`crate::settings::PartialSettings::merge`] applies going forwards. Any
/// other traversal would produce an explanation that disagreed with the value it
/// was explaining.
pub fn source_of(key: &SettingKey, stack: &[LoadedLayer]) -> SettingsSource {
    stack
        .iter()
        .rev()
        .find(|layer| key.claimed_by(&layer.settings))
        .map_or(SettingsSource::Defaults, |layer| layer.source.clone())
}

// =====================================================================
// `mmm config show`
// =====================================================================

/// Render the resolved settings as TOML, each value naming its layer.
///
/// The output parses back as a config layer, which is the property that makes it
/// useful rather than merely informative: `mmm config show > mmm.toml` produces
/// a file that pins the run it came from. The two keys with no value are
/// commented out, so a reader is told they exist without the file claiming a
/// path nobody chose.
pub fn render_show(settings: &Settings, stack: &[LoadedLayer]) -> String {
    let mut lines = vec![
        "# The settings this run resolved to, and the layer each value came from.".to_string(),
        "# `mmm config path` shows where the files were looked for.".to_string(),
        String::new(),
    ];

    let mut open_table: Option<&str> = None;
    for key in KEYS {
        if key.table != open_table {
            open_table = key.table;
            if let Some(table) = open_table {
                lines.push(String::new());
                lines.push(format!("[{table}]"));
            }
        }

        let source = source_of(key, stack);
        lines.push(match (key.value_in(settings), key.unset) {
            (Some(value), _) => format!("{} = {value}  # from: {source}", key.name),
            // Unset and no default to fall back on. Commented so the output
            // stays a config file that means what this run meant.
            (None, note) => format!(
                "# {} is unset  # from: {source}{}",
                key.name,
                note.map_or_else(String::new, |note| format!(" — {note}")),
            ),
        });
    }

    lines.join("\n") + "\n"
}

// =====================================================================
// `mmm config path`
// =====================================================================

/// What `mmm config path` says when the files were skipped outright.
pub const NO_CONFIG_NOTICE: &str =
    "No config files were searched: --no-config was passed. MMM_ environment variables still \
     apply.";

/// Render every location the loader considered, and whether it was there.
///
/// The whole walk, not just the hit — a user asking where the settings come from
/// is usually asking why the file they wrote is being ignored, and the answer is
/// almost always a directory that was never searched or a name that was not the
/// one they used.
pub fn render_paths(loaded: &Loaded, no_config: bool) -> String {
    if no_config {
        return format!("{NO_CONFIG_NOTICE}\n");
    }

    if loaded.searched.is_empty() {
        return "No config files could be looked for: this run has neither a home directory nor a \
                working directory to search.\n"
            .to_string();
    }

    let mut lines = vec!["Searched, in order:".to_string(), String::new()];
    for entry in &loaded.searched {
        lines.push(format!(
            "  {:<9}  {}",
            if entry.found { "found" } else { "not found" },
            entry.source
        ));
    }

    lines.push(String::new());
    let found = loaded.searched.iter().filter(|entry| entry.found).count();
    lines.push(match found {
        0 => format!(
            "Nothing found, so this run uses the built-in defaults. `mmm config init` writes a \
             starter file; the project names searched are {}.",
            PROJECT_CONFIG_NAMES.join(" and ")
        ),
        1 => "1 file was read.".to_string(),
        n => format!("{n} files were read, lowest priority first."),
    });

    lines.join("\n") + "\n"
}

// =====================================================================
// `mmm config validate`
// =====================================================================

/// Report on the files this run read, having already read them.
///
/// Reachable only when every discovered file parsed, because loading happens
/// before any subcommand runs and a broken file stops it there. So this states
/// what was checked rather than pretending to do the checking — the failing case
/// is the loader's error, which names the file and the line.
pub fn render_validate(loaded: &Loaded) -> String {
    let files: Vec<&SettingsSource> = loaded
        .layers
        .iter()
        .map(|layer| &layer.source)
        .filter(|source| source.path().is_some())
        .collect();

    if files.is_empty() {
        return "No config files were read, so there is nothing to check. `mmm config validate \
                <PATH>` checks a particular file.\n"
            .to_string();
    }

    let mut lines: Vec<String> = files
        .iter()
        .map(|source| format!("  ok  {source}"))
        .collect();
    lines.push(String::new());
    lines.push(format!(
        "{} config file{} parsed.",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    ));
    lines.join("\n") + "\n"
}

// =====================================================================
// `mmm config init`
// =====================================================================

/// Which config file `mmm config init` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitTarget {
    /// The per-user file, below the platform config directory.
    User,
    /// `mmm.toml` in the working directory.
    Project,
}

/// A starter config that could not be placed or written.
#[derive(Debug, Error)]
pub enum InitError {
    /// A file is already there and `--force` was not passed.
    #[error(
        "{} already exists — pass --force to overwrite it, or edit it in place",
        .path.display()
    )]
    Exists { path: PathBuf },

    /// There is nowhere to put a per-user config on this machine.
    #[error(
        "there is no per-user config directory on this machine, so there is nowhere to write a \
         user config — use --project to write {} in the working directory instead",
        PROJECT_CONFIG_NAMES[0]
    )]
    NoUserConfigDir,

    /// The write itself failed.
    #[error("could not write {}: {source}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Where a starter config goes.
///
/// Both inputs are arguments rather than process reads, for the same reason
/// [`crate::settings::LoadOptions`] is: the placement rule is then testable
/// against a temporary directory instead of against whatever the developer's
/// `$HOME` happens to be.
///
/// # Errors
///
/// [`InitError::NoUserConfigDir`] when a user config was asked for and the
/// platform has no config directory to put one in.
pub fn init_path(
    target: InitTarget,
    user_config: Option<PathBuf>,
    working_dir: &Path,
) -> Result<PathBuf, InitError> {
    match target {
        InitTarget::User => user_config.ok_or(InitError::NoUserConfigDir),
        InitTarget::Project => Ok(working_dir.join(PROJECT_CONFIG_NAMES[0])),
    }
}

/// Write the starter config to `path`, refusing to clobber unless `force`.
///
/// The refusal is `create_new` rather than an `exists()` check followed by a
/// write, so there is no window between the two in which somebody else's file
/// could appear and be destroyed by a command whose entire job is to be safe
/// about it.
///
/// # Errors
///
/// [`InitError::Exists`] when a file is already there and `force` is false;
/// [`InitError::Write`] if the parent directory or the file cannot be created.
pub fn write_starter_config(path: &Path, force: bool) -> Result<(), InitError> {
    let fail = |source: io::Error| InitError::Write {
        path: path.to_path_buf(),
        source,
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(fail)?;
        }
    }

    let mut file = OpenOptions::new()
        .write(true)
        .truncate(force)
        .create(force)
        .create_new(!force)
        .open(path)
        .map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                InitError::Exists {
                    path: path.to_path_buf(),
                }
            } else {
                fail(source)
            }
        })?;

    file.write_all(starter_config().as_bytes()).map_err(fail)
}

/// The prefix that marks a line of the starter config as a setting rather than
/// prose.
///
/// The convention is mechanical on purpose: `#` followed immediately by
/// something is a line to uncomment, `# ` followed by a space is commentary.
/// That is what lets `uncommenting_the_starter_config_yields_the_defaults`
/// assert the file describes the defaults rather than merely mentioning them.
pub const SETTING_COMMENT: char = '#';

/// The column the starter config's prose wraps at.
const COMMENT_WIDTH: usize = 92;

/// Wrap `text` into comment lines, each carrying `prefix`.
///
/// Word-wrapped rather than emitted verbatim because the reasons in
/// [`COMMAND_LINE_ONLY_KEYS`] are one long string each: a file explaining itself
/// in 200-column lines is one whose explanation nobody reads.
fn comment_lines(prefix: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && prefix.len() + line.len() + 1 + word.len() > COMMENT_WIDTH {
                out.push(format!("{prefix}{line}"));
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push(format!("{prefix}{line}"));
        }
    }
    out
}

/// The contents of the file `mmm config init` writes.
///
/// Every key at its built-in default, commented out, with the precedence rule
/// and the command-line-only refusals stated at the top — because the reader of
/// this file is the person who will next ask "why did it do that?", and the
/// answer should be in the file they are already looking at.
pub fn starter_config() -> String {
    let defaults = Settings::default();

    let mut lines = vec![
        "# mmm configuration".to_string(),
        "#".to_string(),
        "# Precedence, lowest first: this file, then a project mmm.toml found by walking up from"
            .to_string(),
        "# the working directory, then MMM_ environment variables, then the command line. A layer"
            .to_string(),
        "# that says nothing leaves the one below it standing, so a file only has to name what it"
            .to_string(),
        "# changes.".to_string(),
        "#".to_string(),
    ];

    lines.push(
        "# These cannot be set here, and writing one is an error rather than a silent no-op:"
            .to_string(),
    );
    for (key, reason) in COMMAND_LINE_ONLY_KEYS {
        lines.extend(comment_lines("#   ", &format!("{key} — {reason}")));
    }

    lines.extend([
        "#".to_string(),
        "# Every key below is shown at its built-in default and commented out. A line beginning"
            .to_string(),
        format!(
            "# `{SETTING_COMMENT}` and then a key is a setting to uncomment; a line beginning \
             `{SETTING_COMMENT} ` is prose."
        ),
    ]);

    let mut open_table: Option<&str> = None;
    for key in KEYS {
        lines.push(String::new());

        if key.table != open_table {
            open_table = key.table;
            if let Some(table) = open_table {
                lines.push(format!("{SETTING_COMMENT}[{table}]"));
                lines.push(String::new());
            }
        }

        lines.extend(comment_lines("# ", key.summary));
        if let Some(note) = key.unset {
            lines.extend(comment_lines(
                "# ",
                &format!("No default: {note}. The value below is an example."),
            ));
        }

        let value = match (key.value_in(&defaults), key.placeholder) {
            (Some(value), _) => value,
            (None, Some(placeholder)) => Value::String(placeholder.to_string()),
            // Unreachable for the catalogue above, and asserted by
            // `every_key_has_something_to_show`. Rendered rather than panicked
            // so a future key that forgets a placeholder produces a visibly
            // wrong line instead of killing the command.
            (None, None) => Value::String(String::new()),
        };
        lines.push(format!("{SETTING_COMMENT}{} = {value}", key.name));
    }

    lines.join("\n") + "\n"
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use crate::settings::{
        parse_layer, Extensions, PartialExtensions, SearchedPath, DEFAULT_CHUNK_SIZE,
    };
    use tempfile::TempDir;

    /// A stack of layers from `(source, TOML)` pairs, lowest priority first.
    fn stack(layers: &[(SettingsSource, &str)]) -> Vec<LoadedLayer> {
        layers
            .iter()
            .map(|(source, text)| LoadedLayer {
                source: source.clone(),
                settings: parse_layer(text, Path::new("/tmp/mmm.toml"))
                    .expect("the fixture layer must parse"),
            })
            .collect()
    }

    fn user(path: &str) -> SettingsSource {
        SettingsSource::UserConfig(PathBuf::from(path))
    }

    fn project(path: &str) -> SettingsSource {
        SettingsSource::ProjectConfig(PathBuf::from(path))
    }

    /// The key with this name, table-qualified.
    fn key(name: &str) -> &'static SettingKey {
        KEYS.iter()
            .find(|key| key.qualified_name() == name)
            .expect("no such setting")
    }

    /// The `# from:` a rendered line carries, for the key named at its start.
    fn annotation(rendered: &str, key: &str) -> String {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(&format!("{key} = ")) || line.starts_with(&format!("# {key} "))
            })
            .unwrap_or_else(|| panic!("no line for {key} in:\n{rendered}"))
            .split_once("# from: ")
            .expect("every line names its source")
            .1
            .to_string()
    }

    // -----------------------------------------------------------------
    // The catalogue
    // -----------------------------------------------------------------

    /// The enforcement the whole module depends on. Destructured exhaustively
    /// on purpose: a field added to `Settings` stops this compiling until the
    /// catalogue is extended too, which is the only way a new setting cannot
    /// quietly go missing from `config show` and `config init`.
    #[test]
    fn every_setting_has_a_key_in_the_catalogue() {
        let Settings {
            output_dir: _,
            chunk_size: _,
            no_prompt: _,
            verbose: _,
            journal_dir: _,
            date_directory_format: _,
            filename_format: _,
            include_location: _,
            duplicates_dir: _,
            unsorted_dir: _,
            extensions: Extensions { image: _, video: _ },
            skip_patterns: _,
            default_timezone: _,
        } = Settings::default();

        assert_eq!(
            KEYS.len(),
            14,
            "the thirteen settings, with [extensions] counted as its two keys"
        );
    }

    /// Every key is one a config file may actually contain — the names in the
    /// catalogue and the names `PartialSettings` accepts are the same names.
    #[test]
    fn every_key_is_accepted_by_the_deserialiser() {
        let defaults = Settings::default();
        for key in KEYS {
            let value = key
                .value_in(&defaults)
                .or_else(|| key.placeholder.map(|p| Value::String(p.to_string())))
                .expect("every key renders something");
            let text = match key.table {
                Some(table) => format!("[{table}]\n{} = {value}\n", key.name),
                None => format!("{} = {value}\n", key.name),
            };
            parse_layer(&text, Path::new("/tmp/mmm.toml"))
                .unwrap_or_else(|e| panic!("`{}` is not a settable key: {e}", key.name));
        }
    }

    /// A layer that set everything claims every key, and an empty one claims
    /// none — so no key's `claimed` is wired to the wrong field.
    #[test]
    fn every_key_reads_its_own_field() {
        let all = PartialSettings {
            output_dir: Some(PathBuf::from("/o")),
            chunk_size: Some(1),
            no_prompt: Some(true),
            verbose: Some(1),
            journal_dir: Some(PathBuf::from("/j")),
            date_directory_format: Some("%Y".to_string()),
            filename_format: Some("{date}.{ext}".to_string()),
            include_location: Some(false),
            duplicates_dir: Some(PathBuf::from("d")),
            unsorted_dir: Some(PathBuf::from("u")),
            extensions: Some(PartialExtensions {
                image: Some(vec!["jpg".to_string()]),
                video: Some(vec!["mov".to_string()]),
            }),
            skip_patterns: Some(Vec::new()),
            default_timezone: Some("Asia/Singapore".to_string()),
        };
        let none = PartialSettings::default();

        for key in KEYS {
            assert!(key.claimed_by(&all), "{} claimed nothing", key.name);
            assert!(!key.claimed_by(&none), "{} claimed silence", key.name);
        }
    }

    /// TOML puts a bare key after a table header *inside* that table. A
    /// top-level key ordered after `[extensions]` would therefore be rendered
    /// as `extensions.skip_patterns` and refused on the way back in.
    #[test]
    fn no_settings_key_follows_a_table() {
        let first_table = KEYS.iter().position(|key| key.table.is_some());
        let last_bare = KEYS.iter().rposition(|key| key.table.is_none());
        if let (Some(first_table), Some(last_bare)) = (first_table, last_bare) {
            assert!(
                last_bare < first_table,
                "a top-level key written after a table header belongs to that table"
            );
        }
    }

    #[test]
    fn every_key_has_something_to_show() {
        let defaults = Settings::default();
        for key in KEYS {
            assert!(
                key.value_in(&defaults).is_some() || key.placeholder.is_some(),
                "{} has neither a default nor a placeholder to print",
                key.name
            );
            assert_eq!(
                key.value_in(&defaults).is_none(),
                key.unset.is_some(),
                "{} must explain itself exactly when it has no default",
                key.name
            );
        }
    }

    // -----------------------------------------------------------------
    // Where a value came from
    // -----------------------------------------------------------------

    #[test]
    fn a_value_nobody_set_comes_from_the_defaults() {
        assert_eq!(
            source_of(key("chunk_size"), &stack(&[])),
            SettingsSource::Defaults
        );
    }

    #[test]
    fn the_highest_layer_that_claimed_a_key_is_the_one_named() {
        let layers = stack(&[
            (user("/home/mmm.toml"), "chunk_size = 10\nverbose = 3\n"),
            (project("/work/mmm.toml"), "chunk_size = 25\n"),
        ]);

        assert_eq!(
            source_of(key("chunk_size"), &layers),
            project("/work/mmm.toml")
        );
        assert_eq!(
            source_of(key("verbose"), &layers),
            user("/home/mmm.toml"),
            "the project config said nothing about it, so it is not to blame for it"
        );
    }

    /// Half a table claimed is half a table attributed — the same rule the
    /// merge applies one level down.
    #[test]
    fn the_two_extension_lists_are_attributed_separately() {
        let layers = stack(&[
            (
                user("/home/mmm.toml"),
                "[extensions]\nimage = [\"jpg\"]\nvideo = [\"mov\"]\n",
            ),
            (
                project("/work/mmm.toml"),
                "[extensions]\nvideo = [\"mkv\"]\n",
            ),
        ]);

        assert_eq!(
            source_of(key("extensions.image"), &layers),
            user("/home/mmm.toml")
        );
        assert_eq!(
            source_of(key("extensions.video"), &layers),
            project("/work/mmm.toml")
        );
    }

    // -----------------------------------------------------------------
    // `config show`
    // -----------------------------------------------------------------

    #[test]
    fn show_prints_every_key_with_the_layer_that_decided_it() {
        let layers = stack(&[
            (user("/home/mmm.toml"), "chunk_size = 10\n"),
            (project("/work/mmm.toml"), "no_prompt = true\n"),
        ]);
        let rendered = render_show(&crate::settings::resolve_stack(&layers), &layers);

        assert!(rendered.contains("chunk_size = 10  # from: user config (/home/mmm.toml)"));
        assert!(rendered.contains("no_prompt = true  # from: project config (/work/mmm.toml)"));
        assert_eq!(annotation(&rendered, "verbose"), "built-in defaults");

        for key in KEYS {
            assert!(
                rendered.contains(key.name),
                "{} is missing from the output",
                key.name
            );
        }
    }

    /// The command line is a layer like any other, and has to be nameable.
    #[test]
    fn a_flag_is_reported_as_the_command_line() {
        let layers = stack(&[
            (user("/home/mmm.toml"), "chunk_size = 10\n"),
            (SettingsSource::CommandLine, "chunk_size = 7\n"),
        ]);
        let rendered = render_show(&crate::settings::resolve_stack(&layers), &layers);
        assert!(
            rendered.contains("chunk_size = 7  # from: command line"),
            "{rendered}"
        );
    }

    /// The output is a config file, so it can be redirected into one.
    #[test]
    fn what_show_prints_parses_back_as_a_layer() {
        let layers = stack(&[(
            project("/work/mmm.toml"),
            "output_dir = \"/sorted\"\nskip_patterns = [\"*.tmp\"]\n",
        )]);
        let settings = crate::settings::resolve_stack(&layers);
        let rendered = render_show(&settings, &layers);

        let reparsed = parse_layer(&rendered, Path::new("/tmp/mmm.toml"))
            .unwrap_or_else(|e| panic!("`config show` must print a valid config: {e}\n{rendered}"));
        assert_eq!(
            Settings::from_partial(reparsed),
            settings,
            "and one that means the same thing"
        );
    }

    /// A key with no value is stated rather than omitted: a reader who cannot
    /// find `output_dir` in the output concludes it does not exist.
    #[test]
    fn a_key_with_no_value_is_commented_out_and_explained() {
        let rendered = render_show(&Settings::default(), &[]);
        let line = rendered
            .lines()
            .find(|line| line.contains("output_dir"))
            .expect("output_dir must appear");

        assert!(line.starts_with("# "), "{line}");
        assert!(line.contains("unset"), "{line}");
        assert!(line.contains("first input directory"), "{line}");
        assert!(line.contains("# from: built-in defaults"), "{line}");
    }

    /// `[extensions]` is a table, and its two keys belong under the header.
    #[test]
    fn the_extensions_table_is_rendered_as_a_table() {
        let rendered = render_show(&Settings::default(), &[]);
        let header = rendered
            .find("[extensions]")
            .expect("the table header must be printed");
        let image = rendered.find("image = ").expect("image must be printed");
        assert!(header < image, "{rendered}");
    }

    // -----------------------------------------------------------------
    // `config path`
    // -----------------------------------------------------------------

    fn searched(entries: &[(SettingsSource, bool)]) -> Loaded {
        Loaded {
            layers: Vec::new(),
            searched: entries
                .iter()
                .map(|(source, found)| SearchedPath {
                    source: source.clone(),
                    found: *found,
                })
                .collect(),
        }
    }

    #[test]
    fn path_lists_every_location_and_whether_it_was_there() {
        let rendered = render_paths(
            &searched(&[
                (user("/home/.config/mmm/config.toml"), false),
                (project("/work/nested/mmm.toml"), false),
                (project("/work/mmm.toml"), true),
            ]),
            false,
        );

        assert!(rendered.contains("not found  user config (/home/.config/mmm/config.toml)"));
        assert!(rendered.contains("not found  project config (/work/nested/mmm.toml)"));
        assert!(rendered.contains("found      project config (/work/mmm.toml)"));
        assert!(rendered.contains("1 file was read."), "{rendered}");
    }

    /// The directories walked past are the answer to "why is my file being
    /// ignored?", so they are reported even when nothing was found.
    #[test]
    fn path_reports_the_walk_when_nothing_was_found() {
        let rendered = render_paths(&searched(&[(project("/work/mmm.toml"), false)]), false);
        assert!(rendered.contains("/work/mmm.toml"), "{rendered}");
        assert!(rendered.contains("built-in defaults"), "{rendered}");
        assert!(rendered.contains("mmm.toml and .mmm.toml"), "{rendered}");
    }

    #[test]
    fn path_says_so_when_the_files_were_skipped() {
        let rendered = render_paths(&searched(&[(project("/work/mmm.toml"), true)]), true);
        assert!(rendered.contains("--no-config"), "{rendered}");
        assert!(
            !rendered.contains("/work/mmm.toml"),
            "a skipped search has no results to report: {rendered}"
        );
    }

    // -----------------------------------------------------------------
    // `config validate`
    // -----------------------------------------------------------------

    #[test]
    fn validate_names_the_files_that_parsed() {
        let loaded = Loaded {
            layers: stack(&[
                (user("/home/mmm.toml"), "chunk_size = 10\n"),
                (project("/work/mmm.toml"), "verbose = 1\n"),
            ]),
            searched: Vec::new(),
        };
        let rendered = render_validate(&loaded);
        assert!(
            rendered.contains("ok  user config (/home/mmm.toml)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("ok  project config (/work/mmm.toml)"),
            "{rendered}"
        );
        assert!(rendered.contains("2 config files parsed."), "{rendered}");
    }

    /// The environment is a layer, not a file, and `validate` is about files.
    #[test]
    fn validate_ignores_the_environment_layer() {
        let loaded = Loaded {
            layers: stack(&[(SettingsSource::Environment, "chunk_size = 10\n")]),
            searched: Vec::new(),
        };
        let rendered = render_validate(&loaded);
        assert!(rendered.contains("nothing to check"), "{rendered}");
    }

    // -----------------------------------------------------------------
    // `config init`
    // -----------------------------------------------------------------

    #[test]
    fn the_starter_config_lists_every_key() {
        let template = starter_config();
        for key in KEYS {
            assert!(
                template.contains(&format!("{SETTING_COMMENT}{} = ", key.name)),
                "{} is missing from the starter config",
                key.name
            );
        }
        assert!(template.contains("#[extensions]"), "{template}");
    }

    /// The starter config has to be a file, not a leaflet: uncommenting every
    /// setting line must produce a config that parses and means the defaults.
    #[test]
    fn uncommenting_the_starter_config_yields_the_defaults() {
        let uncommented: String = starter_config()
            .lines()
            .filter_map(|line| match line.strip_prefix(SETTING_COMMENT) {
                // `# ` is prose; `#` followed by anything else is a setting.
                Some(rest) if !rest.starts_with(' ') => Some(rest.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let layer = parse_layer(&uncommented, Path::new("/tmp/mmm.toml")).unwrap_or_else(|e| {
            panic!("the starter config must be uncommentable: {e}\n{uncommented}")
        });
        let settings = Settings::from_partial(layer);

        assert_eq!(
            Settings {
                // The only three keys shown as examples rather than defaults,
                // because each has a fallback computed from the run rather than
                // a value this module could write down: the first input
                // directory, a path inside the output tree, and the timezone of
                // whichever machine the run happens on.
                output_dir: None,
                journal_dir: None,
                default_timezone: None,
                ..settings.clone()
            },
            Settings::default(),
            "every default in the template must be the default it claims to be"
        );
        assert_eq!(settings.chunk_size, DEFAULT_CHUNK_SIZE);
        assert!(settings.output_dir.is_some(), "shown as an example value");
        assert!(settings.journal_dir.is_some());
        assert!(settings.default_timezone.is_some());
    }

    /// The reader of this file is the person who will next ask why a run did
    /// something, so the refusals are stated where they are looking.
    #[test]
    fn the_starter_config_states_the_precedence_and_the_refusals() {
        let template = starter_config();
        assert!(template.contains("Precedence"), "{template}");
        assert!(template.contains("MMM_"), "{template}");
        for (key, _) in COMMAND_LINE_ONLY_KEYS {
            assert!(template.contains(key), "{key} is not mentioned");
        }
    }

    #[test]
    fn a_user_init_goes_to_the_user_config_and_a_project_init_to_the_working_directory() {
        let user = PathBuf::from("/home/.config/mmm/config.toml");
        assert_eq!(
            init_path(InitTarget::User, Some(user.clone()), Path::new("/work")).unwrap(),
            user
        );
        assert_eq!(
            init_path(InitTarget::Project, Some(user), Path::new("/work")).unwrap(),
            PathBuf::from("/work/mmm.toml")
        );
    }

    #[test]
    fn a_user_init_with_nowhere_to_put_it_says_so() {
        let err = init_path(InitTarget::User, None, Path::new("/work"))
            .expect_err("there is nowhere to write");
        assert!(matches!(err, InitError::NoUserConfigDir));
        assert!(err.to_string().contains("--project"), "{err}");
    }

    #[test]
    fn init_writes_a_config_that_parses() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/mmm.toml");

        write_starter_config(&path, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        parse_layer(&text, &path).expect("what init writes must be readable");
    }

    /// Overwriting somebody's config is the one thing this command must not do
    /// by accident.
    #[test]
    fn init_refuses_to_overwrite_without_force() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mmm.toml");
        fs::write(&path, "chunk_size = 7\n").unwrap();

        let err = write_starter_config(&path, false).expect_err("an existing file must survive");
        assert!(matches!(err, InitError::Exists { .. }));
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "chunk_size = 7\n",
            "and must be untouched"
        );

        write_starter_config(&path, true).expect("--force is the way past it");
        assert!(fs::read_to_string(&path).unwrap().contains("chunk_size"));
        assert!(fs::read_to_string(&path).unwrap().len() > 100);
    }
}
