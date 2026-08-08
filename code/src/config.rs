use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::settings::{LoadOptions, PartialSettings, Settings};
use crate::settings_report::InitTarget;
use crate::METADATA_DIR_NAME;

/// The journal directory, below [`METADATA_DIR_NAME`] in the output tree.
const JOURNAL_SUBDIR: &str = "journal";

/// Where the journals of runs that organised into `output_dir` live.
///
/// One definition of the layout, shared by the side that writes journals
/// ([`Config::resolve_journal_dir`]) and the side that reads them back
/// ([`JournalLocation::resolve`]). Two copies of this join would be two
/// opportunities for `undo` to look somewhere `organise` never wrote.
pub fn journal_dir_for(output_dir: &Path) -> PathBuf {
    output_dir.join(METADATA_DIR_NAME).join(JOURNAL_SUBDIR)
}

/// The whole command line: an optional subcommand, or the organise arguments
/// given bare.
///
/// `mmm ~/Photos` has meant "organise ~/Photos" since before there were
/// subcommands, and it has to keep meaning that — a tool that breaks every
/// invocation in every script the day it grows an `undo` has not made anyone
/// safer. So [`Config`] is flattened in as well as being the payload of
/// `organise`, with `subcommand_negates_reqs` so `mmm undo` is not refused for
/// naming no directories.
///
/// The one cost is a directory literally named `undo`, `journal` or `config`:
/// `mmm undo` reads as the subcommand. `mmm organise undo` says the other
/// thing, which is why `organise` exists explicitly at all.
///
/// # Why there is no `args_conflicts_with_subcommands`
///
/// It was here, and it made every *global* flag stop the subcommand being seen:
/// `mmm --config x.toml undo` parsed as an organise run over a directory called
/// `undo`, and `mmm -v undo ~/Photos --commit` as an organise run over
/// `~/Photos` — which would have moved the library of somebody who asked to put
/// it back. clap conflicts the top-level args with the subcommands as one group,
/// and a global belongs to that group like any other.
///
/// What it bought was a refusal for organise flags typed before a subcommand.
/// [`Cli::validate_placement`] does that instead, and does it by naming the flag
/// rather than by silently reinterpreting the command.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "mmm",
    about = "Organise images and videos: deduplicate, rename by date/location, sort into directories",
    long_about = "Organise images and videos: deduplicate, rename by date/location, sort into \
                  directories.\n\n\
                  SAFE BY DEFAULT: mmm shows you the plan and changes nothing. Pass --commit \
                  when you have read the plan and want the moves applied.",
    after_help = "SAFETY:\n  \
                  Without --commit, mmm is read-only: it scans, plans, prints, and exits without \
                  touching a single file.\n  \
                  With --commit, files are MOVED — review a plain run first.\n\n\
                  EXAMPLES:\n  \
                  mmm ~/Photos                      # preview the plan, change nothing\n  \
                  mmm ~/Photos --commit             # apply the plan, moving files\n  \
                  mmm ~/Photos -o ~/Sorted --commit # apply, writing into a separate tree\n  \
                  mmm undo ~/Photos                 # preview putting the last run back\n  \
                  mmm undo ~/Photos --commit        # put the last run back\n  \
                  mmm journal list ~/Photos         # what has been run against this library\n\n\
                  JOURNAL:\n  \
                  Every committing run records what it is about to do in a journal under \
                  <output>/.mmm/journal/ before it does it, so the run can be reversed with \
                  `mmm undo`. The path is printed in the run summary.",
    version,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// The organise arguments, when given without naming a subcommand.
    #[command(flatten)]
    pub organise: Config,

    /// Increase verbosity (can be repeated: -v, -vv, -vvv)
    ///
    /// Global so it means the same thing before or after a subcommand: the
    /// operator reaching for `-v` is debugging, and having to remember where
    /// the flag goes is not what they need at that moment.
    ///
    /// A count of zero is the same thing as not passing the flag, so it enters
    /// the command line's layer as "said nothing" and leaves a configured
    /// `verbose` standing. The cost is that a config file's verbosity cannot be
    /// turned back *down* from the command line; the way past it is `--no-config`.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Read this config file instead of searching for one
    ///
    /// Replaces discovery rather than adding to it: a file named here is the
    /// answer to "what settings is this run using?", and one that still
    /// inherited from $HOME would not be. A path that does not exist is an
    /// error, not a fall back to the defaults.
    #[arg(long, value_name = "PATH", global = true, conflicts_with = "no_config")]
    pub config: Option<PathBuf>,

    /// Ignore every config file (MMM_ environment variables still apply)
    ///
    /// The environment belongs to this invocation the way a flag does; skipping
    /// files is a statement about files.
    #[arg(long, global = true)]
    pub no_config: bool,
}

impl Cli {
    /// What this invocation actually asked for.
    ///
    /// Not called `command` because [`clap::CommandFactory::command`] already
    /// is, and a method that shadows it would turn every `Cli::command()` in
    /// the help tests into something else entirely.
    pub fn resolve(self) -> Command {
        self.command
            .unwrap_or(Command::Organise(Box::new(self.organise)))
    }

    /// What the settings loader is allowed to look at for this invocation.
    ///
    /// Taken before [`Cli::resolve`] consumes the parse, because `--config` and
    /// `--no-config` are global: they describe the run, not the subcommand.
    pub fn load_options(&self) -> LoadOptions {
        LoadOptions::from_process(self.config.clone(), self.no_config)
    }

    /// Refuse organise flags typed before a subcommand that cannot use them.
    ///
    /// `mmm --commit undo ~/Photos` reads naturally and means nothing: the
    /// `--commit` lands on the flattened organise arguments, which a resolved
    /// `undo` never looks at, so the undo would preview and the operator would
    /// be told their library was restored by a run that moved nothing.
    ///
    /// Refused rather than quietly honoured, because "honour it" would mean
    /// deciding that a flag typed in one command's position belongs to another,
    /// and that guess is exactly what the subcommand layout exists to avoid.
    /// The same reasoning as `deny_unknown_fields` one layer down: a flag that
    /// silently does nothing is indistinguishable from a flag that is broken.
    ///
    /// `--dry-run` is exempt. It is a no-op wherever it appears, so ignoring it
    /// is not a lie, and refusing it would break the old scripts it exists for.
    ///
    /// The one subcommand that *can* use an organise flag is `config`:
    /// `mmm --chunk-size 7 config show` is the question "what would a run with
    /// this flag resolve to?", and answering it by naming the command line as
    /// the deciding layer is the entire point of `config show`. The flags that
    /// are not settings — `--commit` and the two that go with it — are refused
    /// there as well, because `config` cannot act on them either.
    ///
    /// # Errors
    ///
    /// A message naming every misplaced flag and where it belongs.
    pub fn validate_placement(&self) -> Result<(), String> {
        let Some(command) = &self.command else {
            return Ok(());
        };

        let organise = &self.organise;
        // Flags that are settings. `config` reports them; nothing else can act
        // on them, so for every other subcommand they are misplaced.
        let settings_flags = [
            (organise.output.is_some(), "--output"),
            (organise.chunk_size.is_some(), "--chunk-size"),
            (organise.no_prompt.is_some(), "--no-prompt"),
            (organise.journal_dir.is_some(), "--journal-dir"),
        ];
        // Flags that are not settings, and that no subcommand can act on.
        let switches = [
            (organise.commit, "--commit"),
            (organise.no_journal, "--no-journal"),
            (organise.i_know_what_im_doing, "--i-know-what-im-doing"),
        ];

        let reports_settings = matches!(command, Command::Config { .. });
        let misplaced: Vec<&str> = settings_flags
            .into_iter()
            .filter(|_| !reports_settings)
            .chain(switches)
            .filter_map(|(given, flag)| given.then_some(flag))
            .collect();

        if misplaced.is_empty() {
            return Ok(());
        }

        Err(format!(
            "{} belong{} to `mmm organise`, which `{name}` is not. Drop {} to run `mmm {name}`, or \
             drop `{name}` to organise. A flag a subcommand has of its own goes after it, as in \
             `mmm undo --journal-dir PATH`.",
            misplaced.join(" and "),
            if misplaced.len() == 1 { "s" } else { "" },
            if misplaced.len() == 1 { "it" } else { "them" },
            name = command.name(),
        ))
    }

    /// The file `mmm config validate <PATH>` was asked about, if that is what
    /// this invocation is.
    ///
    /// It exists because every other command loads the ambient configuration
    /// first, and a broken user or project config stops the run there — which
    /// is right for a command that is about to move files, and useless for the
    /// one command whose entire job is to tell you whether a file is broken.
    /// So a named path is answered from that path alone, reading nothing else.
    ///
    /// `mmm config validate` with no path is the opposite question — "are the
    /// files this run reads all right?" — and goes through the ordinary load.
    pub fn standalone_validate(&self) -> Option<&Path> {
        match &self.command {
            Some(Command::Config {
                action: ConfigAction::Validate(args),
            }) => args.path.as_deref(),
            _ => None,
        }
    }

    /// The command line's own layer — the highest-priority one there is.
    ///
    /// Every value the command line supplies that a config file could also
    /// supply goes through here, including the `--journal-dir` of a subcommand
    /// that only reads journals. That is what keeps the precedence rule in one
    /// place: the pipeline below resolves [`Settings`] and never asks a second
    /// time whether a flag was passed.
    pub fn settings_layer(&self) -> PartialSettings {
        let from_command = match &self.command {
            Some(Command::Organise(config)) => config.settings_layer(),
            Some(Command::Undo(args)) => args.location.settings_layer(),
            Some(Command::Journal { action }) => action.location().settings_layer(),
            // Both read the flattened organise arguments, for different
            // reasons. `None` because the organise arguments were given bare,
            // and `config` because `mmm --chunk-size 7 config show` asks what a
            // run with that flag would resolve to — so the flag has to reach the
            // layer, or `config show` would describe every layer except the one
            // that wins most often, and "why did it do that?" would go
            // unanswered for exactly the runs people ask it about.
            Some(Command::Config { .. }) | None => self.organise.settings_layer(),
        };
        PartialSettings {
            verbose: (self.verbose > 0).then_some(self.verbose),
            ..from_command
        }
    }
}

/// The four things `mmm` does.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Organise media into the output tree (the default)
    ///
    /// Boxed because it is by far the largest variant, and an enum sized to
    /// its biggest member would make every `undo` carry the organiser's
    /// command line around with it.
    Organise(Box<Config>),

    /// Put a recorded run's moves back
    Undo(UndoArgs),

    /// Inspect the journals of past runs
    Journal {
        #[command(subcommand)]
        action: JournalAction,
    },

    /// Inspect, start and check the configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

impl Command {
    /// The word that names this command on the command line.
    ///
    /// Used by [`Cli::validate_placement`] to say where a misplaced flag would
    /// have to go, which is the only part of a refusal a reader can act on.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Organise(_) => "organise",
            Self::Undo(_) => "undo",
            Self::Journal { .. } => "journal",
            Self::Config { .. } => "config",
        }
    }
}

/// `mmm config …` — read-only, except for `init`, which writes one new file.
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Print the resolved settings, each value naming the layer it came from
    Show,

    /// List every config file location that was searched, and what was there
    Path,

    /// Write a starter config with every key present and commented out
    Init(ConfigInitArgs),

    /// Parse a config file and report what is wrong with it, running nothing
    Validate(ConfigValidateArgs),
}

/// `mmm config init` — where to put the starter file, and whether to clobber.
#[derive(Args, Debug, Clone)]
pub struct ConfigInitArgs {
    /// Write the per-user config (the default)
    #[arg(long, conflicts_with = "project")]
    pub user: bool,

    /// Write mmm.toml in the working directory instead
    #[arg(long)]
    pub project: bool,

    /// Overwrite a config that is already there
    #[arg(long)]
    pub force: bool,
}

impl ConfigInitArgs {
    /// Which file this invocation writes.
    ///
    /// The user config is the default because it is the one that stops the
    /// retyping this whole phase exists to end; a project file is a statement
    /// about one tree and has to be asked for.
    pub fn target(&self) -> InitTarget {
        if self.project {
            InitTarget::Project
        } else {
            InitTarget::User
        }
    }
}

/// `mmm config validate` — one named file, or the ones this run would read.
#[derive(Args, Debug, Clone)]
pub struct ConfigValidateArgs {
    /// The file to check [default: the files this run reads]
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

/// Which library's journals a reading subcommand is about.
///
/// A positional defaulting to the current directory, because the shape this
/// has to make easy is `cd ~/Photos && mmm undo` — the operator who has just
/// looked at what a run did to a tree is standing in it.
#[derive(Args, Debug, Clone)]
pub struct JournalLocation {
    /// The organised library whose journals to read
    #[arg(value_name = "LIBRARY", default_value = ".")]
    pub library: PathBuf,

    /// Read journals from here instead of <LIBRARY>/.mmm/journal
    ///
    /// The counterpart of `organise --journal-dir`: a run whose journal was
    /// written elsewhere has to be undoable from there too.
    #[arg(long, value_name = "PATH")]
    pub journal_dir: Option<PathBuf>,
}

impl JournalLocation {
    /// What this location said about the settings — its `--journal-dir`, if any.
    pub fn settings_layer(&self) -> PartialSettings {
        PartialSettings {
            journal_dir: self.journal_dir.clone(),
            ..PartialSettings::default()
        }
    }

    /// The directory to read journals from.
    ///
    /// Reads `settings` rather than its own `--journal-dir` field, which reaches
    /// here through [`Cli::settings_layer`] as the top layer. One precedence
    /// rule, applied once: a `journal_dir` in a config file relocates the
    /// journals a run writes *and* the ones `undo` reads, and a flag outranks
    /// both. Two separate resolutions would be two chances for `undo` to look
    /// somewhere `organise` never wrote.
    pub fn resolve(&self, settings: &Settings) -> PathBuf {
        settings
            .journal_dir
            .clone()
            .unwrap_or_else(|| journal_dir_for(&self.library))
    }
}

/// `mmm undo` — replay a run's journal backwards.
#[derive(Args, Debug, Clone)]
pub struct UndoArgs {
    #[command(flatten)]
    pub location: JournalLocation,

    /// Undo this run rather than the most recent one
    #[arg(long, value_name = "RUN_ID", conflicts_with = "last")]
    pub run: Option<String>,

    /// Undo the most recent recorded run (the default)
    ///
    /// Inert on its own — it is what happens anyway. It exists so a script can
    /// say which run it means instead of relying on a default that a later
    /// version might change.
    #[arg(long, default_value_t = false)]
    pub last: bool,

    /// Actually move the files back (without this, undo only prints the plan)
    #[arg(long, default_value_t = false)]
    pub commit: bool,
}

impl UndoArgs {
    /// Whether this undo is a preview. Same posture as everything else: moving
    /// files is the opt-in.
    pub fn is_dry_run(&self) -> bool {
        !self.commit
    }
}

/// `mmm journal …` — read-only, always.
#[derive(Subcommand, Debug, Clone)]
pub enum JournalAction {
    /// List the runs recorded against a library, newest first
    List(JournalLocation),

    /// Show one run's journal in full
    Show(JournalShowArgs),
}

impl JournalAction {
    /// Which library this action reads, whichever action it is.
    pub fn location(&self) -> &JournalLocation {
        match self {
            Self::List(location) => location,
            Self::Show(args) => &args.location,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct JournalShowArgs {
    /// The run to show, as printed by `mmm journal list`
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,

    #[command(flatten)]
    pub location: JournalLocation,
}

/// `mmm organise` — the arguments of a run that moves media into place.
#[derive(Args, Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "this struct is the command line, and a command line is a bag of independent \
              switches. The lint's suggested cure — folding them into two-variant enums or a \
              state machine — would hide the derive that generates the flags and put a layer \
              between `--no-journal` and the field that means it."
)]
pub struct Config {
    /// One or more directories to scan for media files
    #[arg(required = true)]
    pub directories: Vec<PathBuf>,

    /// Output directory for organised files (default: first input directory)
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Actually move files (without this, mmm only prints the plan and exits)
    #[arg(long, default_value_t = false)]
    pub commit: bool,

    /// Deprecated no-op: previewing is the default, so this flag does nothing
    ///
    /// Hidden from `--help` on purpose. It stays accepted so scripts written
    /// against the old destructive-by-default CLI keep running instead of
    /// failing on an unknown argument.
    #[arg(short = 'd', long = "dry-run", hide = true, default_value_t = false)]
    pub dry_run_deprecated: bool,

    /// Number of files to process per chunk before prompting to continue [default: 100]
    ///
    /// Optional rather than defaulted, so that "not passed" stays
    /// distinguishable from "passed 100". A default baked in here would arrive
    /// as an *opinion of the command line* — the highest-priority layer there
    /// is — and would silently outrank every `chunk_size` a config file could
    /// set, which looks exactly like the setting not working.
    #[arg(short, long, value_name = "N")]
    pub chunk_size: Option<usize>,

    /// Skip user confirmation prompts between chunks [--no-prompt=false to keep them]
    ///
    /// `--no-prompt` on its own means yes. The `=false` form exists because
    /// otherwise a `no_prompt = true` somebody wrote into `~/.config/mmm/config.toml`
    /// would be unanswerable from the command line, and a flag that can only be
    /// switched on is not a precedence rule.
    ///
    /// The value must be attached with `=`, so `mmm --no-prompt ~/Photos` still
    /// reads the path as a directory rather than trying it as a boolean.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    pub no_prompt: Option<bool>,

    /// Write the run journal here instead of <OUTPUT>/.mmm/journal
    ///
    /// Useful when the output tree is read-only, on a filesystem that cannot be
    /// trusted to survive the run, or when journals for several libraries are
    /// collected in one place.
    #[arg(long, value_name = "PATH")]
    pub journal_dir: Option<PathBuf>,

    /// Which timezone to assume for photos whose EXIF records no offset
    ///
    /// A fixed offset (`+08:00`, `-05:30`) or an IANA zone name
    /// (`Asia/Singapore`). Without this the run falls back to the machine's own
    /// timezone. Files that *do* carry an offset tag are unaffected — the file's
    /// own record always wins.
    ///
    /// This does not change which day a photo is filed under: an EXIF wall clock
    /// is filed under exactly the digits the camera wrote, whatever the zone. It
    /// decides the instant the run records, and it does move filesystem-dated
    /// and UTC-stamped video files.
    ///
    /// `allow_hyphen_values` because half the world's offsets start with one.
    /// Without it `--timezone -05:30` is refused as an unknown flag `-0`, and
    /// the only spelling that works is `--timezone=-05:30` — a trap laid for
    /// exactly the users the flag exists to serve. The cost is that
    /// `--timezone --commit` takes `--commit` as the value, which then fails as
    /// "`--commit` is not a timezone" rather than as a missing value; a wrong
    /// value named in the error is a better failure than a correct one refused.
    #[arg(long, value_name = "TZ", allow_hyphen_values = true)]
    pub timezone: Option<String>,

    /// UNSAFE: do not journal this run — it cannot be undone
    ///
    /// Without a journal there is no record of where a file came from, so `mmm
    /// undo` has nothing to replay and the moves are permanent. Refused
    /// together with --commit unless --i-know-what-im-doing is also passed.
    #[arg(long, default_value_t = false)]
    pub no_journal: bool,

    /// Acknowledge an unsafe flag combination (currently only --no-journal --commit)
    #[arg(long, default_value_t = false)]
    pub i_know_what_im_doing: bool,
}

/// Emitted once on stderr when a caller passes the retired `--dry-run` flag.
pub const DRY_RUN_DEPRECATION_NOTICE: &str =
    "warning: --dry-run is deprecated and does nothing — previewing is now the default. \
     Pass --commit to move files.";

/// Why `--no-journal --commit` is refused.
///
/// The two flags together mean "move this person's photo library and keep no
/// record of where anything came from". That is a legitimate thing to want on a
/// scratch tree and a catastrophic thing to type by accident, and the only
/// difference between the two is whether the operator meant it — so the refusal
/// names the flag that says so rather than guessing.
pub const NO_JOURNAL_WITH_COMMIT_REFUSAL: &str =
    "refusing to move files without a journal: --no-journal disables the record that `mmm undo` \
     replays, so a committing run with it cannot be reversed. Drop --no-journal, or pass \
     --i-know-what-im-doing as well if the moves really are throwaway.";

impl Config {
    /// What this command line said about the settings every layer has a voice in.
    ///
    /// Only the fields a config file could also supply appear here. `commit`,
    /// `no_journal` and `i_know_what_im_doing` are absent by design — a layer is
    /// precisely the thing they must not be expressible as, and the reasoning is
    /// on [`crate::settings::Settings`].
    pub fn settings_layer(&self) -> PartialSettings {
        PartialSettings {
            output_dir: self.output.clone(),
            chunk_size: self.chunk_size,
            no_prompt: self.no_prompt,
            journal_dir: self.journal_dir.clone(),
            default_timezone: self.timezone.clone(),
            ..PartialSettings::default()
        }
    }

    /// Where this run writes.
    ///
    /// The one place the two types have to meet: `output_dir` stays `Option`
    /// even in the resolved [`Settings`] because its fallback — "the first input
    /// directory" — is not knowable until the command line has been parsed, and
    /// the input directories are the command line's alone.
    pub fn output_dir<'a>(&'a self, settings: &'a Settings) -> &'a Path {
        settings
            .output_dir
            .as_deref()
            .unwrap_or_else(|| &self.directories[0])
    }

    /// Whether this run is a preview.
    ///
    /// The posture is inverted from the original CLI: moving files is the
    /// opt-in, so any run that did not ask for `--commit` is a dry run.
    pub fn is_dry_run(&self) -> bool {
        !self.commit
    }

    /// Where this run's journal belongs, or `None` if journalling is off.
    ///
    /// The default sits *inside the output tree* rather than in a home
    /// directory or a temp dir on purpose: the journal describes one particular
    /// library, and a library that gets copied to another disk, or handed to
    /// someone else, arrives with the record of how it was built still attached.
    /// A journal parked in `~/.local/share` is one machine reinstall away from
    /// being an undo that cannot find its run.
    ///
    /// Whether there is a journal at all is the command line's decision
    /// (`--no-journal`, which no file may make); *where* it goes is a setting,
    /// so the location comes from `settings` — where this run's own
    /// `--journal-dir` has already arrived as the top layer.
    pub fn resolve_journal_dir(&self, settings: &Settings) -> Option<PathBuf> {
        if self.no_journal {
            return None;
        }
        Some(
            settings
                .journal_dir
                .clone()
                .unwrap_or_else(|| journal_dir_for(self.output_dir(settings))),
        )
    }

    /// Reject flag combinations that are individually sensible and jointly
    /// destructive, before any work starts.
    ///
    /// Returned rather than printed for the same reason as
    /// [`Config::deprecation_notice`]: the decision is the testable part, and
    /// `main` owns what a refusal looks like on a terminal.
    ///
    /// # Errors
    ///
    /// Returns [`NO_JOURNAL_WITH_COMMIT_REFUSAL`] when `--no-journal` and
    /// `--commit` are given together without `--i-know-what-im-doing`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.no_journal && self.commit && !self.i_know_what_im_doing {
            return Err(NO_JOURNAL_WITH_COMMIT_REFUSAL);
        }
        Ok(())
    }

    /// The deprecation notice for this invocation, if any legacy flag was used.
    ///
    /// Returned rather than printed so the decision is unit-testable; `main`
    /// owns the actual write to stderr.
    pub fn deprecation_notice(&self) -> Option<&'static str> {
        self.dry_run_deprecated
            .then_some(DRY_RUN_DEPRECATION_NOTICE)
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
    use clap::CommandFactory;

    /// Parse a full command line and insist it resolved to an organise run.
    ///
    /// Every assertion below is about the organise arguments, and routing them
    /// through [`Cli`] rather than through `Config` directly is the point: the
    /// property under test is what the *command line* means, and since the
    /// subcommands arrived that is no longer a question `Config` alone can
    /// answer.
    fn parse(args: &[&str]) -> Config {
        parse_with(args).0
    }

    /// Parse a command line and resolve the settings it alone implies.
    ///
    /// "It alone" is the point: no config files, no environment, so what comes
    /// back is what the flags said plus the built-in defaults — which is the
    /// baseline every precedence assertion below is measured against.
    fn parse_with(args: &[&str]) -> (Config, Settings) {
        let cli = Cli::try_parse_from(args).unwrap();
        let settings = Settings::resolve([cli.settings_layer()]);
        match cli.resolve() {
            Command::Organise(config) => (*config, settings),
            other => panic!("expected an organise run, got {other:?}"),
        }
    }

    /// Resolve a command line sitting on top of one config file.
    ///
    /// The file goes through the real TOML loader rather than a hand-built
    /// [`PartialSettings`], so these assertions cover the path a user's file
    /// actually takes.
    fn settings_over(file: &str, args: &[&str]) -> Settings {
        let lower = crate::settings::parse_layer(file, Path::new("/tmp/mmm.toml"))
            .expect("the fixture config must parse");
        let cli = Cli::try_parse_from(args).unwrap();
        Settings::resolve([lower, cli.settings_layer()])
    }

    #[test]
    fn a_plain_run_is_a_dry_run() {
        let config = parse(&["mmm", "/photos"]);
        assert!(!config.commit);
        assert!(config.is_dry_run());
    }

    #[test]
    fn commit_opts_into_moving_files() {
        let config = parse(&["mmm", "/photos", "--commit"]);
        assert!(config.commit);
        assert!(!config.is_dry_run());
    }

    #[test]
    fn deprecated_dry_run_flag_is_still_accepted() {
        // The whole point of keeping it: an old script must not die on an
        // unknown argument.
        let config = parse(&["mmm", "/photos", "--dry-run"]);
        assert!(config.dry_run_deprecated);
        assert!(config.is_dry_run());
    }

    #[test]
    fn deprecated_short_dry_run_flag_is_still_accepted() {
        let config = parse(&["mmm", "/photos", "-d"]);
        assert!(config.dry_run_deprecated);
    }

    #[test]
    fn deprecated_dry_run_flag_is_a_no_op_not_a_veto() {
        // `--dry-run --commit` is contradictory but reachable from a script
        // that bolted `--commit` onto an old invocation. The explicit,
        // current flag wins; the retired one does nothing.
        let config = parse(&["mmm", "/photos", "--dry-run", "--commit"]);
        assert!(!config.is_dry_run());
    }

    #[test]
    fn the_deprecation_notice_fires_only_for_the_legacy_flag() {
        assert_eq!(parse(&["mmm", "/photos"]).deprecation_notice(), None);
        assert_eq!(
            parse(&["mmm", "/photos", "--commit"]).deprecation_notice(),
            None
        );
        assert_eq!(
            parse(&["mmm", "/photos", "--dry-run"]).deprecation_notice(),
            Some(DRY_RUN_DEPRECATION_NOTICE)
        );
    }

    #[test]
    fn output_dir_falls_back_to_the_first_input_directory() {
        let (config, settings) = parse_with(&["mmm", "/photos", "/more"]);
        assert_eq!(config.output_dir(&settings), Path::new("/photos"));

        let (config, settings) = parse_with(&["mmm", "/photos", "-o", "/sorted"]);
        assert_eq!(config.output_dir(&settings), Path::new("/sorted"));
    }

    #[test]
    fn the_journal_defaults_into_the_output_tree() {
        // Not a temp dir, not the home directory: the journal belongs to the
        // library it describes and must travel with it.
        let (config, settings) = parse_with(&["mmm", "/photos"]);
        assert_eq!(
            config.resolve_journal_dir(&settings),
            Some(PathBuf::from("/photos/.mmm/journal"))
        );

        let (config, settings) = parse_with(&["mmm", "/photos", "-o", "/sorted"]);
        assert_eq!(
            config.resolve_journal_dir(&settings),
            Some(PathBuf::from("/sorted/.mmm/journal")),
            "the journal follows the output tree, not the input one"
        );
    }

    #[test]
    fn journal_dir_overrides_the_default_location() {
        let (config, settings) = parse_with(&["mmm", "/photos", "--journal-dir", "/var/log/mmm"]);
        assert_eq!(
            config.resolve_journal_dir(&settings),
            Some(PathBuf::from("/var/log/mmm"))
        );
    }

    #[test]
    fn no_journal_turns_journalling_off_entirely() {
        let (config, settings) = parse_with(&["mmm", "/photos", "--no-journal"]);
        assert_eq!(config.resolve_journal_dir(&settings), None);
    }

    /// `--no-journal` wins over `--journal-dir`: asking for no journal and then
    /// naming where to put it is contradictory, and the safe reading of a
    /// contradiction is the one that writes nothing to a path the user may not
    /// have meant.
    #[test]
    fn no_journal_beats_an_explicit_journal_dir() {
        let (config, settings) = parse_with(&[
            "mmm",
            "/photos",
            "--journal-dir",
            "/var/log/mmm",
            "--no-journal",
        ]);
        assert_eq!(config.resolve_journal_dir(&settings), None);
    }

    #[test]
    fn ordinary_invocations_validate() {
        assert!(parse(&["mmm", "/photos"]).validate().is_ok());
        assert!(parse(&["mmm", "/photos", "--commit"]).validate().is_ok());
    }

    /// Moving files with no record of where they came from is unreversible, so
    /// it has to be asked for twice.
    #[test]
    fn no_journal_with_commit_is_refused() {
        let err = parse(&["mmm", "/photos", "--no-journal", "--commit"])
            .validate()
            .expect_err("an unjournalled commit must not start");
        assert_eq!(err, NO_JOURNAL_WITH_COMMIT_REFUSAL);
        assert!(err.contains("--i-know-what-im-doing"), "{err}");
    }

    #[test]
    fn no_journal_with_commit_is_allowed_once_acknowledged() {
        assert!(parse(&[
            "mmm",
            "/photos",
            "--no-journal",
            "--commit",
            "--i-know-what-im-doing"
        ])
        .validate()
        .is_ok());
    }

    /// A preview moves nothing, so there is nothing to undo and nothing to
    /// refuse. `--no-journal` alone must not block a dry run.
    #[test]
    fn no_journal_without_commit_is_fine() {
        assert!(parse(&["mmm", "/photos", "--no-journal"])
            .validate()
            .is_ok());
    }

    /// The acknowledgement flag on its own is inert — it must not smuggle any
    /// behaviour of its own into an otherwise ordinary run.
    #[test]
    fn the_acknowledgement_flag_alone_changes_nothing() {
        let (config, settings) =
            parse_with(&["mmm", "/photos", "--commit", "--i-know-what-im-doing"]);
        assert!(config.validate().is_ok());
        assert_eq!(
            config.resolve_journal_dir(&settings),
            Some(PathBuf::from("/photos/.mmm/journal"))
        );
    }

    // -----------------------------------------------------------------
    // Subcommand layout
    // -----------------------------------------------------------------

    /// The compatibility promise: every invocation written before subcommands
    /// existed still means what it meant.
    #[test]
    fn a_bare_path_is_still_an_organise_run() {
        let config = parse(&["mmm", "/photos", "--commit"]);
        assert_eq!(config.directories, vec![PathBuf::from("/photos")]);
        assert!(config.commit);
    }

    #[test]
    fn organise_can_also_be_named_explicitly() {
        let (config, settings) = parse_with(&["mmm", "organise", "/photos", "-o", "/sorted"]);
        assert_eq!(config.directories, vec![PathBuf::from("/photos")]);
        assert_eq!(
            config.output_dir(&settings),
            Path::new("/sorted"),
            "the flags of an explicit `organise` reach the layer too"
        );
    }

    /// Where a journal-reading command line looks, with nothing under it.
    fn journal_dir_of(args: &[&str]) -> PathBuf {
        journal_dir_of_over("", args)
    }

    /// The same, with a config file underneath the command line.
    fn journal_dir_of_over(file: &str, args: &[&str]) -> PathBuf {
        let settings = settings_over(file, args);
        match Cli::try_parse_from(args).unwrap().resolve() {
            Command::Undo(undo) => undo.location.resolve(&settings),
            Command::Journal { action } => action.location().resolve(&settings),
            other @ (Command::Organise(_) | Command::Config { .. }) => {
                panic!("expected a journal-reading command, got {other:?}")
            }
        }
    }

    /// `mmm undo` names no directories, and must not be refused for it.
    #[test]
    fn undo_does_not_inherit_the_organiser_required_directories() {
        let Command::Undo(args) = Cli::try_parse_from(["mmm", "undo"]).unwrap().resolve() else {
            panic!("`mmm undo` must resolve to the undo subcommand");
        };
        assert_eq!(args.location.library, PathBuf::from("."));
        assert!(args.is_dry_run(), "undo previews unless told otherwise");
        assert_eq!(args.run, None);
    }

    #[test]
    fn undo_takes_the_library_positionally_and_commits_on_request() {
        let Command::Undo(args) = Cli::try_parse_from(["mmm", "undo", "/photos", "--commit"])
            .unwrap()
            .resolve()
        else {
            panic!("expected undo");
        };
        assert_eq!(args.location.library, PathBuf::from("/photos"));
        assert!(!args.is_dry_run());
    }

    #[test]
    fn undo_reads_journals_from_the_library_metadata_dir() {
        assert_eq!(
            journal_dir_of(&["mmm", "undo", "/photos"]),
            PathBuf::from("/photos/.mmm/journal")
        );
    }

    /// The read and write sides must agree on where journals live, or an undo
    /// looks somewhere the run never wrote.
    #[test]
    fn undo_looks_where_organise_writes() {
        let (organised, settings) = parse_with(&["mmm", "/photos", "-o", "/sorted"]);
        assert_eq!(
            organised.resolve_journal_dir(&settings),
            Some(journal_dir_of(&["mmm", "undo", "/sorted"]))
        );
    }

    /// And they must still agree when a config file, rather than a flag, is what
    /// moved the journals — otherwise a `journal_dir` in a project config
    /// organises into one place and undoes from another.
    #[test]
    fn undo_looks_where_organise_writes_when_a_config_file_moved_them() {
        const FILE: &str = "journal_dir = \"/var/log/mmm\"\n";

        let organise_settings = settings_over(FILE, &["mmm", "/photos"]);
        let Command::Organise(organised) =
            Cli::try_parse_from(["mmm", "/photos"]).unwrap().resolve()
        else {
            panic!("expected an organise run");
        };

        assert_eq!(
            organised.resolve_journal_dir(&organise_settings),
            Some(PathBuf::from("/var/log/mmm"))
        );
        assert_eq!(
            journal_dir_of_over(FILE, &["mmm", "undo", "/photos"]),
            PathBuf::from("/var/log/mmm")
        );
    }

    #[test]
    fn undo_journal_dir_overrides_the_library() {
        assert_eq!(
            journal_dir_of(&["mmm", "undo", "/photos", "--journal-dir", "/var/log/mmm"]),
            PathBuf::from("/var/log/mmm")
        );
    }

    /// The flag is the top layer for a reading subcommand too.
    #[test]
    fn undo_journal_dir_outranks_a_configured_one() {
        assert_eq!(
            journal_dir_of_over(
                "journal_dir = \"/var/log/mmm\"\n",
                &["mmm", "undo", "/photos", "--journal-dir", "/elsewhere"]
            ),
            PathBuf::from("/elsewhere")
        );
    }

    /// Naming a run and asking for the newest one are contradictory requests,
    /// and clap is the right place to say so — before anything reads a journal.
    #[test]
    fn undo_refuses_both_a_named_run_and_last() {
        assert!(
            Cli::try_parse_from(["mmm", "undo", "--run", "20240315-103000-abc123", "--last"])
                .is_err()
        );
    }

    #[test]
    fn undo_takes_a_named_run() {
        let Command::Undo(args) =
            Cli::try_parse_from(["mmm", "undo", "--run", "20240315-103000-abc123"])
                .unwrap()
                .resolve()
        else {
            panic!("expected undo");
        };
        assert_eq!(args.run.as_deref(), Some("20240315-103000-abc123"));
    }

    #[test]
    fn journal_list_and_show_reach_the_same_directory() {
        let list = journal_dir_of(&["mmm", "journal", "list", "/photos"]);
        let show = journal_dir_of(&[
            "mmm",
            "journal",
            "show",
            "20240315-103000-abc123",
            "/photos",
        ]);

        assert_eq!(list, PathBuf::from("/photos/.mmm/journal"));
        assert_eq!(show, list, "one location, whichever action asks for it");

        let Command::Journal {
            action: JournalAction::Show(args),
        } = Cli::try_parse_from([
            "mmm",
            "journal",
            "show",
            "20240315-103000-abc123",
            "/photos",
        ])
        .unwrap()
        .resolve()
        else {
            panic!("expected journal show");
        };
        assert_eq!(args.run_id, "20240315-103000-abc123");
    }

    /// `journal show` needs to know which run; there is no sensible default.
    #[test]
    fn journal_show_requires_a_run_id() {
        assert!(Cli::try_parse_from(["mmm", "journal", "show"]).is_err());
    }

    /// Only the *first* argument can name a subcommand. After a path has been
    /// taken as a directory, everything following it is another directory —
    /// including one called `undo`.
    ///
    /// This is the behaviour worth pinning rather than a refusal: a run that
    /// silently dropped `~/Photos/undo` from its input, or that refused an
    /// otherwise valid two-directory invocation, would both be worse than
    /// reading the word where it sits.
    #[test]
    fn a_directory_named_undo_is_a_directory_when_it_is_not_the_first_argument() {
        let config = parse(&["mmm", "/photos", "undo"]);
        assert_eq!(
            config.directories,
            vec![PathBuf::from("/photos"), PathBuf::from("undo")]
        );
    }

    /// The other half of the same rule, and the one that costs something:
    /// a *first* argument called `undo` is the subcommand. `mmm organise undo`
    /// is how to say the other thing.
    #[test]
    fn a_leading_undo_is_the_subcommand_and_organise_disambiguates_it() {
        assert!(matches!(
            Cli::try_parse_from(["mmm", "undo"]).unwrap().resolve(),
            Command::Undo(_)
        ));
        assert_eq!(
            parse(&["mmm", "organise", "undo"]).directories,
            vec![PathBuf::from("undo")]
        );
    }

    /// `-v` is global, so it means the same thing wherever the operator
    /// reaches for it.
    #[test]
    fn verbosity_is_accepted_before_or_after_a_subcommand() {
        assert_eq!(
            Cli::try_parse_from(["mmm", "-vv", "/photos"])
                .unwrap()
                .verbose,
            2
        );
        assert_eq!(
            Cli::try_parse_from(["mmm", "undo", "-vv"]).unwrap().verbose,
            2
        );
    }

    // -----------------------------------------------------------------
    // The command line as a layer
    // -----------------------------------------------------------------

    /// The layer of a command line that passed nothing.
    fn layer_of(args: &[&str]) -> PartialSettings {
        Cli::try_parse_from(args).unwrap().settings_layer()
    }

    /// The property the whole task turns on: a flag that was not passed must
    /// say *nothing*, so that a config file's value survives. A `chunk_size`
    /// defaulted in clap would arrive here as an opinion of the highest-priority
    /// layer and silently outrank every file on the machine.
    #[test]
    fn an_unpassed_flag_contributes_nothing_to_the_layer() {
        let layer = layer_of(&["mmm", "/photos"]);
        assert_eq!(layer.chunk_size, None);
        assert_eq!(layer.no_prompt, None);
        assert_eq!(layer.output_dir, None);
        assert_eq!(layer.journal_dir, None);
        assert_eq!(layer.verbose, None);
        assert!(
            layer.is_empty(),
            "a bare `mmm ~/Photos` states no settings at all: {layer:?}"
        );
    }

    #[test]
    fn a_passed_flag_reaches_the_layer() {
        let layer = layer_of(&[
            "mmm",
            "/photos",
            "-o",
            "/sorted",
            "--chunk-size",
            "25",
            "--no-prompt",
            "--journal-dir",
            "/var/log/mmm",
            "-vv",
        ]);
        assert_eq!(layer.output_dir, Some(PathBuf::from("/sorted")));
        assert_eq!(layer.chunk_size, Some(25));
        assert_eq!(layer.no_prompt, Some(true));
        assert_eq!(layer.journal_dir, Some(PathBuf::from("/var/log/mmm")));
        assert_eq!(layer.verbose, Some(2));
    }

    /// The switches that exist to make a destructive run deliberate are not
    /// settings, and must not appear in a layer even at the top of the stack —
    /// a layer is exactly the shape a file could take.
    #[test]
    fn the_command_line_only_switches_are_not_in_the_layer() {
        let layer = layer_of(&[
            "mmm",
            "/photos",
            "--commit",
            "--no-journal",
            "--i-know-what-im-doing",
        ]);
        assert!(
            layer.is_empty(),
            "commit and its relatives are not settings: {layer:?}"
        );
    }

    /// With nothing anywhere, the resolved settings are the built-in defaults —
    /// which is the behaviour every earlier phase's tests assert on.
    #[test]
    fn a_bare_command_line_resolves_to_the_defaults() {
        let (_, settings) = parse_with(&["mmm", "/photos"]);
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.chunk_size, 100);
        assert!(!settings.no_prompt);
    }

    // -----------------------------------------------------------------
    // Precedence between a config file and the flags
    // -----------------------------------------------------------------

    #[test]
    fn a_configured_value_stands_when_the_flag_is_absent() {
        let settings = settings_over("chunk_size = 25\n", &["mmm", "/photos"]);
        assert_eq!(settings.chunk_size, 25);
    }

    #[test]
    fn the_flag_outranks_the_configured_value() {
        let settings = settings_over(
            "chunk_size = 25\n",
            &["mmm", "/photos", "--chunk-size", "7"],
        );
        assert_eq!(settings.chunk_size, 7);
    }

    /// The case a bare switch cannot express: a config file that turned the
    /// prompts off, and a run that wants them back.
    #[test]
    fn no_prompt_can_be_switched_back_on_from_the_command_line() {
        assert!(settings_over("no_prompt = true\n", &["mmm", "/photos"]).no_prompt);
        assert!(
            !settings_over(
                "no_prompt = true\n",
                &["mmm", "/photos", "--no-prompt=false"]
            )
            .no_prompt,
            "a flag that can only be switched on is not a precedence rule"
        );
    }

    /// The value has to be attached with `=`, so the ordinary invocation still
    /// reads the path that follows as a directory rather than as a boolean.
    #[test]
    fn a_bare_no_prompt_still_means_yes_and_leaves_the_next_argument_alone() {
        let (config, settings) = parse_with(&["mmm", "--no-prompt", "/photos"]);
        assert_eq!(config.directories, vec![PathBuf::from("/photos")]);
        assert!(settings.no_prompt);
    }

    #[test]
    fn a_configured_output_directory_is_used_and_the_flag_outranks_it() {
        let settings = settings_over("output_dir = \"/sorted\"\n", &["mmm", "/photos"]);
        let (config, _) = parse_with(&["mmm", "/photos"]);
        assert_eq!(config.output_dir(&settings), Path::new("/sorted"));
        assert_eq!(
            config.resolve_journal_dir(&settings),
            Some(PathBuf::from("/sorted/.mmm/journal")),
            "and the journal follows it"
        );

        let settings = settings_over(
            "output_dir = \"/sorted\"\n",
            &["mmm", "/photos", "-o", "/e"],
        );
        assert_eq!(config.output_dir(&settings), Path::new("/e"));
    }

    /// A configured verbosity reaches the run; `-v` outranks it. Zero is
    /// "nothing said", so an absent flag does not overwrite the file.
    #[test]
    fn verbosity_resolves_through_the_layers() {
        assert_eq!(
            settings_over("verbose = 3\n", &["mmm", "/photos"]).verbose,
            3
        );
        assert_eq!(
            settings_over("verbose = 3\n", &["mmm", "/photos", "-v"]).verbose,
            1
        );
    }

    /// `--commit` is not a setting, so no combination of layers can produce it.
    /// The assertion is on the parse: it is a `Config` field and nothing else.
    #[test]
    fn commit_comes_only_from_the_command_line() {
        let (config, _) = parse_with(&["mmm", "/photos"]);
        assert!(!config.commit);
        let (config, _) = parse_with(&["mmm", "/photos", "--commit"]);
        assert!(config.commit);
    }

    // -----------------------------------------------------------------
    // Which config files a run reads
    // -----------------------------------------------------------------

    #[test]
    fn config_names_an_explicit_file() {
        let cli = Cli::try_parse_from(["mmm", "/photos", "--config", "/etc/mmm.toml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/etc/mmm.toml")));
        assert!(!cli.no_config);
    }

    #[test]
    fn no_config_is_a_plain_switch() {
        let cli = Cli::try_parse_from(["mmm", "/photos", "--no-config"]).unwrap();
        assert!(cli.no_config);
        assert_eq!(cli.config, None);
    }

    /// Naming a file to read and asking for none are contradictory, and clap is
    /// the right place to say so — before anything opens a file.
    #[test]
    fn config_and_no_config_together_are_refused() {
        assert!(Cli::try_parse_from([
            "mmm",
            "/photos",
            "--config",
            "/etc/mmm.toml",
            "--no-config"
        ])
        .is_err());
    }

    /// Both are global, so they mean the same thing wherever they are typed —
    /// including on a subcommand that reads journals rather than settings.
    #[test]
    fn the_config_flags_are_accepted_before_or_after_a_subcommand() {
        assert!(
            Cli::try_parse_from(["mmm", "undo", "--no-config"])
                .unwrap()
                .no_config
        );
        assert_eq!(
            Cli::try_parse_from(["mmm", "--config", "/etc/mmm.toml", "undo"])
                .unwrap()
                .config,
            Some(PathBuf::from("/etc/mmm.toml"))
        );
    }

    #[test]
    fn the_load_options_carry_the_flags_through() {
        let cli = Cli::try_parse_from(["mmm", "/photos", "--config", "/etc/mmm.toml"]).unwrap();
        let options = cli.load_options();
        assert_eq!(options.explicit, Some(PathBuf::from("/etc/mmm.toml")));
        assert!(!options.no_config);

        let cli = Cli::try_parse_from(["mmm", "/photos", "--no-config"]).unwrap();
        assert!(cli.load_options().no_config);
    }

    // -----------------------------------------------------------------
    // The config subcommand family
    // -----------------------------------------------------------------

    /// The action `args` names, or a panic saying what it got instead.
    fn config_action(args: &[&str]) -> ConfigAction {
        match Cli::try_parse_from(args).unwrap().resolve() {
            Command::Config { action } => action,
            other => panic!("expected a config command, got {other:?}"),
        }
    }

    /// Like `undo`, `config` names no directories and must not be refused for
    /// it — `subcommand_negates_reqs` is what makes that true.
    #[test]
    fn the_config_actions_parse_without_naming_a_directory() {
        assert!(matches!(
            config_action(&["mmm", "config", "show"]),
            ConfigAction::Show
        ));
        assert!(matches!(
            config_action(&["mmm", "config", "path"]),
            ConfigAction::Path
        ));
        assert!(matches!(
            config_action(&["mmm", "config", "init"]),
            ConfigAction::Init(_)
        ));
        assert!(matches!(
            config_action(&["mmm", "config", "validate"]),
            ConfigAction::Validate(_)
        ));
    }

    /// `mmm config` alone has no sensible default action — showing the settings
    /// would be a guess, and writing a file would be a dangerous one.
    #[test]
    fn config_requires_an_action() {
        assert!(Cli::try_parse_from(["mmm", "config"]).is_err());
    }

    /// The subcommand named `config` and the global flag `--config` are
    /// different things, and both have to keep working next to each other.
    #[test]
    fn the_config_subcommand_and_the_config_flag_coexist() {
        let cli =
            Cli::try_parse_from(["mmm", "--config", "/etc/mmm.toml", "config", "show"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/etc/mmm.toml")));
        assert!(matches!(
            cli.resolve(),
            Command::Config {
                action: ConfigAction::Show
            }
        ));
    }

    /// `mmm config show` on its own reports the files and nothing else.
    #[test]
    fn a_bare_config_show_states_no_settings_of_its_own() {
        assert!(layer_of(&["mmm", "config", "show"]).is_empty());
        assert_eq!(
            layer_of(&["mmm", "config", "show", "-vv"]).verbose,
            Some(2),
            "except the global verbosity, which is a setting wherever it is typed"
        );
    }

    /// And an organise flag typed before it reaches the layer, so `config show`
    /// can name the command line as the layer that decided a value. Without
    /// this, the one layer that wins most often would be the one `config show`
    /// could not describe.
    #[test]
    fn an_organise_flag_before_config_show_reaches_the_layer() {
        let layer = layer_of(&[
            "mmm",
            "--chunk-size",
            "7",
            "-o",
            "/sorted",
            "config",
            "show",
        ]);
        assert_eq!(layer.chunk_size, Some(7));
        assert_eq!(layer.output_dir, Some(PathBuf::from("/sorted")));
    }

    #[test]
    fn init_writes_the_user_config_unless_told_otherwise() {
        let ConfigAction::Init(args) = config_action(&["mmm", "config", "init"]) else {
            panic!("expected init");
        };
        assert_eq!(args.target(), InitTarget::User);
        assert!(!args.force);

        let ConfigAction::Init(args) = config_action(&["mmm", "config", "init", "--project"])
        else {
            panic!("expected init");
        };
        assert_eq!(args.target(), InitTarget::Project);
    }

    #[test]
    fn init_takes_force_and_refuses_both_targets_at_once() {
        let ConfigAction::Init(args) = config_action(&["mmm", "config", "init", "--force"]) else {
            panic!("expected init");
        };
        assert!(args.force);
        assert!(Cli::try_parse_from(["mmm", "config", "init", "--user", "--project"]).is_err());
    }

    /// The escape hatch: a named file is answered from that file alone, so a
    /// broken config elsewhere cannot stop the command that diagnoses broken
    /// configs.
    #[test]
    fn validate_with_a_path_bypasses_the_ambient_load() {
        let cli = Cli::try_parse_from(["mmm", "config", "validate", "/etc/mmm.toml"]).unwrap();
        assert_eq!(
            cli.standalone_validate(),
            Some(Path::new("/etc/mmm.toml")),
            "the named file is the whole question"
        );
    }

    /// Every other invocation goes through the ordinary load, including
    /// `config validate` with nothing named — which is the other question, "are
    /// the files this run reads all right?".
    #[test]
    fn nothing_else_bypasses_the_ambient_load() {
        for args in [
            vec!["mmm", "config", "validate"],
            vec!["mmm", "config", "show"],
            vec!["mmm", "config", "init"],
            vec!["mmm", "/photos"],
            vec!["mmm", "undo"],
        ] {
            assert_eq!(
                Cli::try_parse_from(&args).unwrap().standalone_validate(),
                None,
                "{args:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Where a flag may be typed
    // -----------------------------------------------------------------

    /// The regression that removing `args_conflicts_with_subcommands` fixed.
    /// With it, a global flag before a subcommand made clap stop looking for
    /// the subcommand, and `mmm -v undo ~/Photos --commit` became an organise
    /// run that moved the library of somebody who had asked to put it back.
    #[test]
    fn a_global_flag_before_a_subcommand_leaves_it_a_subcommand() {
        for args in [
            vec!["mmm", "-vv", "undo", "/photos"],
            vec!["mmm", "--no-config", "undo", "/photos"],
            vec!["mmm", "--config", "/etc/mmm.toml", "undo", "/photos"],
        ] {
            let resolved = Cli::try_parse_from(&args).unwrap().resolve();
            assert!(
                matches!(resolved, Command::Undo(_)),
                "{args:?} resolved to {resolved:?}"
            );
        }

        assert!(matches!(
            Cli::try_parse_from(["mmm", "-v", "journal", "list", "/photos"])
                .unwrap()
                .resolve(),
            Command::Journal { .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["mmm", "--no-config", "config", "path"])
                .unwrap()
                .resolve(),
            Command::Config { .. }
        ));
    }

    /// And the global still arrives, so it was read rather than merely tolerated.
    #[test]
    fn a_global_flag_before_a_subcommand_still_reaches_the_run() {
        let cli = Cli::try_parse_from(["mmm", "-vv", "--no-config", "undo"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(cli.no_config);
        assert_eq!(cli.settings_layer().verbose, Some(2));
    }

    /// An organise flag before a subcommand is refused rather than swallowed:
    /// `mmm --commit undo` would otherwise preview and report success.
    #[test]
    fn an_organise_flag_before_a_subcommand_is_refused_and_named() {
        let refusal = Cli::try_parse_from(["mmm", "--commit", "undo", "/photos"])
            .unwrap()
            .validate_placement()
            .expect_err("a swallowed --commit must not be silent");

        assert!(refusal.contains("--commit"), "{refusal}");
        assert!(refusal.contains("undo"), "{refusal}");
        assert!(refusal.contains("organise"), "{refusal}");
    }

    #[test]
    fn every_misplaced_flag_is_listed() {
        let refusal = Cli::try_parse_from([
            "mmm",
            "-o",
            "/sorted",
            "--no-journal",
            "journal",
            "list",
            "/photos",
        ])
        .unwrap()
        .validate_placement()
        .expect_err("both flags are misplaced");
        assert!(refusal.contains("--output"), "{refusal}");
        assert!(refusal.contains("--no-journal"), "{refusal}");
        assert!(refusal.contains("journal"), "{refusal}");
    }

    /// `config` is the exception, and only for the flags that are settings:
    /// reporting what `--chunk-size 7` resolves to is its job, and `--commit`
    /// is still something it cannot act on.
    #[test]
    fn config_takes_the_settings_flags_and_still_refuses_the_switches() {
        assert_eq!(
            Cli::try_parse_from(["mmm", "--chunk-size", "7", "config", "show"])
                .unwrap()
                .validate_placement(),
            Ok(())
        );

        let refusal = Cli::try_parse_from(["mmm", "--commit", "config", "show"])
            .unwrap()
            .validate_placement()
            .expect_err("`config` cannot commit anything");
        assert!(refusal.contains("--commit"), "{refusal}");
    }

    /// The ordinary invocations, which must not be caught by the guard: an
    /// organise run may say anything it likes, and a subcommand's own flags
    /// belong to the subcommand.
    #[test]
    fn a_flag_in_its_own_place_is_not_misplaced() {
        for args in [
            vec!["mmm", "/photos", "--commit", "-o", "/sorted"],
            vec!["mmm", "organise", "/photos", "--commit"],
            vec!["mmm", "undo", "/photos", "--commit"],
            vec!["mmm", "undo", "--journal-dir", "/var/log/mmm"],
            vec!["mmm", "journal", "list", "/photos"],
            vec!["mmm", "config", "show"],
            vec!["mmm", "--chunk-size", "7", "config", "show"],
            vec!["mmm", "-vv", "--no-config", "undo"],
        ] {
            assert_eq!(
                Cli::try_parse_from(&args).unwrap().validate_placement(),
                Ok(()),
                "{args:?}"
            );
        }
    }

    /// The retired flag is a no-op wherever it lands, so ignoring it is honest
    /// and refusing it would break the scripts it was kept for.
    #[test]
    fn the_deprecated_flag_is_exempt_from_the_placement_rule() {
        assert_eq!(
            Cli::try_parse_from(["mmm", "--dry-run", "undo", "/photos"])
                .unwrap()
                .validate_placement(),
            Ok(())
        );
    }

    #[test]
    fn help_lists_the_config_subcommand() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("config"), "{help}");
    }

    #[test]
    fn help_marks_no_journal_as_unsafe_and_names_the_default_location() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--journal-dir"), "{help}");
        assert!(
            help.contains("UNSAFE"),
            "the flag that makes a run unreversible must say so: {help}"
        );
        assert!(help.contains(".mmm/journal"), "{help}");
    }

    #[test]
    fn the_deprecated_flag_is_hidden_from_help() {
        let help = Cli::command().render_long_help().to_string();
        assert!(
            !help.contains("--dry-run"),
            "the retired flag must not be advertised: {help}"
        );
        assert!(help.contains("--commit"));
    }

    #[test]
    fn help_states_the_safety_posture() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("SAFE BY DEFAULT"), "{help}");
        assert!(help.contains("--commit"), "{help}");
    }

    /// A user who has just been told a run is journalled needs to be able to
    /// find the command that replays it without leaving `--help`.
    #[test]
    fn help_advertises_undo_as_the_way_back() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("undo"), "{help}");
        assert!(help.contains("journal"), "{help}");
    }
}
