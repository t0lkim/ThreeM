use clap::Parser;
use std::path::PathBuf;

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
                  mmm ~/Photos -o ~/Sorted --commit # apply, writing into a separate tree",
    version
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

    /// Increase verbosity (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Emitted once on stderr when a caller passes the retired `--dry-run` flag.
pub const DRY_RUN_DEPRECATION_NOTICE: &str =
    "warning: --dry-run is deprecated and does nothing — previewing is now the default. \
     Pass --commit to move files.";

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

    fn parse(args: &[&str]) -> Config {
        Config::try_parse_from(args).unwrap()
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
    fn the_deprecated_flag_is_hidden_from_help() {
        let help = Config::command().render_long_help().to_string();
        assert!(
            !help.contains("--dry-run"),
            "the retired flag must not be advertised: {help}"
        );
        assert!(help.contains("--commit"));
    }

    #[test]
    fn help_states_the_safety_posture() {
        let help = Config::command().render_long_help().to_string();
        assert!(help.contains("SAFE BY DEFAULT"), "{help}");
        assert!(help.contains("--commit"), "{help}");
    }
}
