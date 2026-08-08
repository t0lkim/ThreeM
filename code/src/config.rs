use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

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
/// naming no directories, and `args_conflicts_with_subcommands` so
/// `mmm ~/Photos undo` is an error rather than a guess.
///
/// The one cost is a directory literally named `undo` or `journal`:
/// `mmm undo` reads as the subcommand. `mmm organise undo` says the other
/// thing, which is why `organise` exists explicitly at all.
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
    args_conflicts_with_subcommands = true,
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
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
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
}

/// The three things `mmm` does.
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
    /// The directory to read journals from.
    pub fn resolve(&self) -> PathBuf {
        self.journal_dir
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

    /// Number of files to process per chunk before prompting to continue
    #[arg(short, long, default_value_t = 100)]
    pub chunk_size: usize,

    /// Skip user confirmation prompts between chunks
    #[arg(long, default_value_t = false)]
    pub no_prompt: bool,

    /// Write the run journal here instead of <OUTPUT>/.mmm/journal
    ///
    /// Useful when the output tree is read-only, on a filesystem that cannot be
    /// trusted to survive the run, or when journals for several libraries are
    /// collected in one place.
    #[arg(long, value_name = "PATH")]
    pub journal_dir: Option<PathBuf>,

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
    pub fn output_dir(&self) -> &PathBuf {
        self.output.as_ref().unwrap_or_else(|| &self.directories[0])
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
    pub fn resolve_journal_dir(&self) -> Option<PathBuf> {
        if self.no_journal {
            return None;
        }
        Some(
            self.journal_dir
                .clone()
                .unwrap_or_else(|| journal_dir_for(self.output_dir())),
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
        match Cli::try_parse_from(args).unwrap().resolve() {
            Command::Organise(config) => *config,
            other => panic!("expected an organise run, got {other:?}"),
        }
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
        assert_eq!(
            parse(&["mmm", "/photos", "/more"]).output_dir(),
            &PathBuf::from("/photos")
        );
        assert_eq!(
            parse(&["mmm", "/photos", "-o", "/sorted"]).output_dir(),
            &PathBuf::from("/sorted")
        );
    }

    #[test]
    fn the_journal_defaults_into_the_output_tree() {
        // Not a temp dir, not the home directory: the journal belongs to the
        // library it describes and must travel with it.
        assert_eq!(
            parse(&["mmm", "/photos"]).resolve_journal_dir(),
            Some(PathBuf::from("/photos/.mmm/journal"))
        );
        assert_eq!(
            parse(&["mmm", "/photos", "-o", "/sorted"]).resolve_journal_dir(),
            Some(PathBuf::from("/sorted/.mmm/journal")),
            "the journal follows the output tree, not the input one"
        );
    }

    #[test]
    fn journal_dir_overrides_the_default_location() {
        assert_eq!(
            parse(&["mmm", "/photos", "--journal-dir", "/var/log/mmm"]).resolve_journal_dir(),
            Some(PathBuf::from("/var/log/mmm"))
        );
    }

    #[test]
    fn no_journal_turns_journalling_off_entirely() {
        assert_eq!(
            parse(&["mmm", "/photos", "--no-journal"]).resolve_journal_dir(),
            None
        );
    }

    /// `--no-journal` wins over `--journal-dir`: asking for no journal and then
    /// naming where to put it is contradictory, and the safe reading of a
    /// contradiction is the one that writes nothing to a path the user may not
    /// have meant.
    #[test]
    fn no_journal_beats_an_explicit_journal_dir() {
        assert_eq!(
            parse(&[
                "mmm",
                "/photos",
                "--journal-dir",
                "/var/log/mmm",
                "--no-journal"
            ])
            .resolve_journal_dir(),
            None
        );
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
        let config = parse(&["mmm", "/photos", "--commit", "--i-know-what-im-doing"]);
        assert!(config.validate().is_ok());
        assert_eq!(
            config.resolve_journal_dir(),
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
        let config = parse(&["mmm", "organise", "/photos", "-o", "/sorted"]);
        assert_eq!(config.directories, vec![PathBuf::from("/photos")]);
        assert_eq!(config.output_dir(), &PathBuf::from("/sorted"));
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
        let Command::Undo(args) = Cli::try_parse_from(["mmm", "undo", "/photos"])
            .unwrap()
            .resolve()
        else {
            panic!("expected undo");
        };
        assert_eq!(
            args.location.resolve(),
            PathBuf::from("/photos/.mmm/journal")
        );
    }

    /// The read and write sides must agree on where journals live, or an undo
    /// looks somewhere the run never wrote.
    #[test]
    fn undo_looks_where_organise_writes() {
        let organised = parse(&["mmm", "/photos", "-o", "/sorted"]);
        let Command::Undo(args) = Cli::try_parse_from(["mmm", "undo", "/sorted"])
            .unwrap()
            .resolve()
        else {
            panic!("expected undo");
        };
        assert_eq!(
            organised.resolve_journal_dir(),
            Some(args.location.resolve())
        );
    }

    #[test]
    fn undo_journal_dir_overrides_the_library() {
        let Command::Undo(args) =
            Cli::try_parse_from(["mmm", "undo", "/photos", "--journal-dir", "/var/log/mmm"])
                .unwrap()
                .resolve()
        else {
            panic!("expected undo");
        };
        assert_eq!(args.location.resolve(), PathBuf::from("/var/log/mmm"));
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
        let Command::Journal {
            action: JournalAction::List(list),
        } = Cli::try_parse_from(["mmm", "journal", "list", "/photos"])
            .unwrap()
            .resolve()
        else {
            panic!("expected journal list");
        };
        let Command::Journal {
            action: JournalAction::Show(show),
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

        assert_eq!(list.resolve(), PathBuf::from("/photos/.mmm/journal"));
        assert_eq!(show.location.resolve(), list.resolve());
        assert_eq!(show.run_id, "20240315-103000-abc123");
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
