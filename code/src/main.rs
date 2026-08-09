use std::path::Path;

use anyhow::{Context as _, Result};
use chrono::Utc;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::{debug, error, info};

use mmm::{
    hasher, journal, organiser, reporter, scanner, settings, settings_report, sidecar, undo,
};

use mmm::config::{Cli, Command, Config, ConfigAction, JournalAction, UndoArgs};
use mmm::geocoder::GeoLookup;
use mmm::journal::{Journal, JournalEntry, RunHeader};
use mmm::organiser::{ChunkController, MoveRecorder};
use mmm::reporter::JournalStatus;
use mmm::settings::{Loaded, LoadedLayer, Settings};
use mmm::undo::RestorePlan;

/// Drives the chunked move phase from the terminal: the progress bar, and the
/// operator's answer at each chunk boundary.
///
/// Everything interactive lives here rather than in the library. The library's
/// job is to hand back what happened; deciding what that looks like on a
/// terminal — and whether to ask at all — is `main`'s.
struct CliController<'a> {
    bar: &'a ProgressBar,
    /// Whether to ask at chunk boundaries. `--no-prompt` answers yes silently.
    prompt: bool,
}

impl ChunkController for CliController<'_> {
    fn chunk_started(&mut self, chunk_number: usize, chunks: usize) {
        self.bar
            .set_message(format!("chunk {chunk_number}/{chunks}"));
    }

    fn file_finished(&mut self) {
        self.bar.inc(1);
    }

    fn should_continue(&mut self, chunk_number: usize, remaining: usize) -> bool {
        if !self.prompt {
            return true;
        }
        // `suspend` clears the bar for the duration of the prompt; the answer
        // travels back out as a value, which is the whole point — the old code
        // exited the process from inside this closure.
        self.bar
            .suspend(|| reporter::prompt_continue(chunk_number, remaining))
    }
}

/// Open this run's journal, or report that there will not be one.
///
/// Called only on the committing path, and deliberately not before: a preview
/// must leave no trace at all, and the surest way to guarantee that is for the
/// journal never to be created on that path rather than for something later to
/// remember not to write to it.
///
/// # Errors
///
/// Returns an error if the journal cannot be created. Nothing has moved at that
/// point, and nothing will: a run that cannot record what it is about to do
/// must not do it.
fn open_journal(config: &Config, settings: &Settings) -> Result<Option<Journal>> {
    let Some(dir) = config.resolve_journal_dir(settings) else {
        println!();
        reporter::print_journal_location(JournalStatus::Disabled);
        return Ok(None);
    };

    let header = RunHeader::new(
        journal::generate_run_id(),
        config.output_dir(settings),
        RunHeader::current_argv(),
    );
    let journal = Journal::create(&dir, &header)
        .context("the run journal could not be created, so no files have been moved")?;

    // Printed here as well as in the summary: an interrupted run never reaches
    // its summary, and its operator is the one who most needs this path.
    println!();
    reporter::print_journal_location(JournalStatus::At(journal.path()));

    Ok(Some(journal))
}

/// Close the journal with the `RunCompleted` line, on every path out of a
/// committing run.
///
/// Best effort by design. The run is over and its counts are already on their
/// way to the operator; a journal that cannot take this last line is one whose
/// *missing* `RunCompleted` tells `undo` the run did not finish cleanly, which
/// is exactly what undo needs to know.
fn finish_journal(journal: Option<&mut Journal>, moved: usize, failed: usize, skipped: usize) {
    let Some(journal) = journal else { return };

    if let Err(e) = journal.append(&JournalEntry::RunCompleted {
        moved,
        failed,
        skipped,
        ended_at: Utc::now(),
    }) {
        error!(
            error = %format!("{e:#}"),
            "could not close the run journal; undo will treat this run as interrupted"
        );
    }
}

/// Open the journal that records an *undo* run.
///
/// Written into the same directory the run being reversed was read from, so
/// `mmm journal list` shows an undo alongside the run it undid and
/// `mmm undo --last` can reverse the reversal. Unlike an organise run this has
/// no `--no-journal`: the point of the subcommand is that a move is recoverable,
/// and an unrecorded undo would be the one operation in the tool that is not.
///
/// # Errors
///
/// Returns an error if the journal cannot be created. Nothing has moved at that
/// point, and nothing will.
fn open_undo_journal(dir: &Path, plan: &RestorePlan) -> Result<Journal> {
    let header = RunHeader::new(
        journal::generate_run_id(),
        &plan.header.output_dir,
        RunHeader::current_argv(),
    );
    let journal = Journal::create(dir, &header)
        .context("the undo journal could not be created, so no files have been moved")?;

    println!();
    reporter::print_journal_location(JournalStatus::At(journal.path()));

    Ok(journal)
}

/// `mmm undo` — replay one run's journal in reverse.
fn run_undo(args: &UndoArgs, settings: &Settings) -> Result<()> {
    let dir = args.location.resolve(settings);

    let journal_path = match &args.run {
        Some(run_id) => {
            let path = journal::journal_path(&dir, run_id);
            if !path.is_file() {
                anyhow::bail!(
                    "no run {run_id} was recorded in {} — `mmm journal list` shows the runs that \
                     were",
                    dir.display()
                );
            }
            path
        }
        // `--last` and giving nothing mean the same thing; the flag exists so a
        // script can say which it meant.
        None => journal::journals_newest_first(&dir)?
            .into_iter()
            .next()
            .with_context(|| {
                format!(
                    "no runs are recorded in {} — there is nothing to undo",
                    dir.display()
                )
            })?,
    };

    let plan = undo::plan_restore(&journal_path)?;

    reporter::print_mode_banner(args.is_dry_run());

    // Verified only for the preview. The restore verifies each file again at
    // the moment it moves it — which it must, since the restores run in reverse
    // and can change what a later step finds — so doing it here as well on the
    // committing path would be work whose answer is discarded.
    let checks = args.is_dry_run().then(|| undo::verify_plan(&plan));
    reporter::print_restore_plan(&plan, checks.as_deref());

    if args.is_dry_run() {
        println!("\n{}", reporter::DRY_RUN_BANNER);
        return Ok(());
    }

    if plan.steps.is_empty() {
        // Nothing to record and nothing to reverse. Creating a journal for a
        // run that moves no files would leave a growing trail of empty undos.
        return Ok(());
    }

    let mut journal = open_undo_journal(&dir, &plan)?;
    let journal_path = journal.path().to_path_buf();

    let mut recorder = MoveRecorder::new(Some(&mut journal));
    let run = undo::execute_restore(&plan, &mut recorder);

    finish_journal(Some(&mut journal), run.moved(), run.failed, run.skipped());
    reporter::print_restore_summary(&plan, &run, JournalStatus::At(&journal_path));

    // A partial undo has to be detectable by a script, which cannot read the
    // table above.
    if run.journal_failed {
        anyhow::bail!(
            "the undo journal could not be written, so the undo stopped after {} file{} — see \
             the errors above. Nothing further was moved.",
            run.moved(),
            if run.moved() == 1 { "" } else { "s" }
        );
    }
    // Anything short of "every file is back where it came from" is a partial
    // undo, including files restored under a collision suffix: the library is
    // not as it was, and a script that treats a zero exit as "the undo worked"
    // must not be told that it did.
    if let Some(shortfall) = run.shortfall() {
        anyhow::bail!(
            "this run was not fully put back — {shortfall} — see the results above. The other {} \
             file{} restored.",
            run.restored,
            if run.restored == 1 { " was" } else { "s were" }
        );
    }
    // Reached only when every step succeeded, so this is the one remaining
    // reason the library may not be as it was: the interrupted run left moves
    // nothing recorded the outcome of, and undo cannot reverse what it cannot
    // establish happened. A script must not read that as a clean undo.
    if !plan.unresolved.is_empty() {
        anyhow::bail!(
            "the run was interrupted before it could record what happened to {} move{} — see \
             \"{}\" above and check each by hand. Everything it did record has been put back.",
            plan.unresolved.len(),
            if plan.unresolved.len() == 1 { "" } else { "s" },
            reporter::POSSIBLY_MOVED_HEADING,
        );
    }

    Ok(())
}

/// `mmm journal list` / `mmm journal show` — read-only, always.
fn run_journal(action: &JournalAction, settings: &Settings) -> Result<()> {
    match action {
        JournalAction::List(location) => {
            let rows = undo::summarise_runs(&location.resolve(settings))?;
            reporter::print_run_list(&rows);
        }
        JournalAction::Show(args) => {
            let dir = args.location.resolve(settings);
            let path = journal::journal_path(&dir, &args.run_id);
            if !path.is_file() {
                anyhow::bail!(
                    "no run {} was recorded in {} — `mmm journal list` shows the runs that were",
                    args.run_id,
                    dir.display()
                );
            }
            let (header, entries) = undo::read_run(&path)?;
            reporter::print_run_detail(&path, &header, &entries);
        }
    }
    Ok(())
}

/// Install the tracing subscriber for a resolved verbosity.
fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// `mmm config …` — report the configuration, or start one.
///
/// Takes the whole stack rather than the resolved settings alone, because the
/// question `show` answers is not "what is the value?" but "which layer decided
/// it?", and only the stack holds that.
fn run_config(
    action: &ConfigAction,
    settings: &Settings,
    stack: &[LoadedLayer],
    loaded: &Loaded,
    no_config: bool,
) -> Result<()> {
    match action {
        ConfigAction::Show => print!("{}", settings_report::render_show(settings, stack)),
        ConfigAction::Path => print!("{}", settings_report::render_paths(loaded, no_config)),
        // The no-path form. A named file never reaches here — it is answered
        // before the ambient load, so that a different broken config cannot
        // stop the command that diagnoses broken configs.
        ConfigAction::Validate(_) => print!("{}", settings_report::render_validate(loaded)),
        ConfigAction::Init(args) => {
            let path = settings_report::init_path(
                args.target(),
                settings::user_config_path(),
                &std::env::current_dir().context(
                    "the working directory could not be read, so there is nowhere to \
                              write a project config",
                )?,
            )?;
            settings_report::write_starter_config(&path, args.force)?;
            println!("Wrote {}", path.display());
            println!(
                "Every key is commented out at its default — uncomment what you want to change."
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Before anything is read or done: an organise flag typed before a
    // subcommand lands on arguments that subcommand never looks at, so
    // `mmm --commit undo ~/Photos` would preview and report success while
    // putting nothing back.
    if let Err(refusal) = cli.validate_placement() {
        anyhow::bail!(refusal);
    }

    // Answered before anything else is read: `mmm config validate <PATH>` is a
    // question about that one file, and loading the ambient configuration first
    // would let an unrelated broken config stop the command you reach for to
    // find out what is wrong with a config.
    if let Some(path) = cli.standalone_validate() {
        settings::load_file(path)?;
        println!("ok  {}", path.display());
        return Ok(());
    }

    // Read before any work starts, and for every subcommand: a config that
    // cannot be understood has to stop the run here rather than be discovered
    // halfway through moving somebody's library. `--no-config` is the way past a
    // file that is in the way.
    //
    // Before tracing is initialised, too, because the verbosity is itself a
    // setting: a `verbose = 2` in a config file has to reach the subscriber, and
    // a subscriber already installed from the flag alone could not be told about
    // it afterwards. The cost is that a load error is reported by `main`'s
    // `Result` rather than through the log — which is where an error naming a
    // file and a line belongs anyway.
    let loaded = settings::load(&cli.load_options())?;

    // The command line goes on last, so it wins: the layers arrive
    // lowest-priority first and `resolve` folds them in that order. Kept as a
    // stack rather than folded away because `mmm config show` has to name the
    // layer each value came from, and it must be *this* list it names.
    let stack = loaded.stack(cli.settings_layer());
    let settings = settings::resolve_stack(&stack);

    init_tracing(settings.verbose);
    for layer in &loaded.layers {
        debug!(source = %layer.source, "read config layer");
    }

    let no_config = cli.no_config;
    match cli.resolve() {
        Command::Organise(config) => run_organise(&config, &settings),
        Command::Undo(args) => run_undo(&args, &settings),
        Command::Journal { action } => run_journal(&action, &settings),
        Command::Config { action } => run_config(&action, &settings, &stack, &loaded, no_config),
    }
}

/// `mmm organise` — the scan, plan, and move pipeline.
///
/// Takes both types deliberately. `config` answers the questions only the
/// command line may answer — which directories, and whether this run moves
/// anything; `settings` answers everything the config layers had a voice in.
fn run_organise(config: &Config, settings: &Settings) -> Result<()> {
    if let Some(notice) = config.deprecation_notice() {
        eprintln!("{notice}");
    }

    // Before the banner, before the scan: a run that is going to be refused
    // must not first tell the operator it is about to move their files.
    if let Err(refusal) = config.validate() {
        anyhow::bail!(refusal);
    }

    // Built once, here, and for the same reason as the refusal above: every
    // layer already validated its own patterns as it was read, so this is the
    // last line of defence rather than the first, and it belongs before the
    // banner so that a run which cannot name its files never announces that it
    // is about to move them.
    let layout = settings.layout()?;
    let filter = settings.scan_filter()?;
    let timezone = settings.timezone_policy()?;
    let date_policy = settings.date_policy();
    let fallback_warning = settings.fallback_warning();

    // Say which posture we are in before doing any work, not after — a user
    // who expected a preview must not learn otherwise from the aftermath.
    reporter::print_mode_banner(config.is_dry_run());

    info!("mmm v{}", env!("CARGO_PKG_VERSION"));
    info!(
        "scanning {} director{}",
        config.directories.len(),
        if config.directories.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );

    // === PHASE A: SCAN ===
    println!("Scanning directories...");

    let scan_spinner = ProgressBar::new_spinner();
    scan_spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    scan_spinner.set_message("discovering media files...");
    scan_spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let scanner::ScanResult {
        files,
        sidecars,
        skipped: scan_skipped,
        excluded,
    } = scanner::scan_directories(&config.directories, &filter);
    scan_spinner.finish_with_message(format!("found {} media files", files.len()));

    // Paired before the early return below, so a tree holding nothing but
    // orphaned sidecars still says so rather than reporting "no media files"
    // and leaving the operator to guess whether their `.xmp` files were seen.
    let sidecars = sidecar::SidecarIndex::build(&files, &sidecars);
    if sidecars.paired() > 0 || !sidecars.orphans().is_empty() {
        println!(
            "  {} sidecar file{} paired, {} left in place",
            sidecars.paired(),
            if sidecars.paired() == 1 { "" } else { "s" },
            sidecars.orphans().len()
        );
    }

    // Printed rather than logged: a `skip_patterns` entry that is quietly
    // excluding half a library should be visible to the operator, not inferred
    // from a file count that came out lower than they expected.
    if excluded > 0 {
        println!(
            "  {excluded} entr{} excluded by skip_patterns",
            if excluded == 1 { "y" } else { "ies" }
        );
    }

    if files.is_empty() {
        println!("No media files found in the specified directories.");
        reporter::print_sidecar_orphans(sidecars.orphans());
        if scan_skipped > 0 {
            // "Nothing here" and "we could not look" must never read the same.
            println!(
                "{scan_skipped} entr{} could not be read and {} skipped — see the warnings above.",
                if scan_skipped == 1 { "y" } else { "ies" },
                if scan_skipped == 1 { "was" } else { "were" }
            );
        }
        return Ok(());
    }

    // Dedup
    //
    // The pool is built here rather than inside `find_duplicates` so that a
    // thread count nothing can be spawned for stops the run at the point it was
    // asked for, before a single file has been read — and so the count is
    // printed alongside the phase it bounds, which is the one place a user
    // reaching for `--threads` will look to see whether it took.
    let hash_pool = hasher::HashPool::with_threads(settings.hash_thread_count())?;
    let threads = hash_pool.threads().get();
    println!(
        "\nAnalysing for duplicates ({threads} thread{})...",
        if threads == 1 { "" } else { "s" }
    );
    let dedup_pb = hasher::hashing_progress_bar(files.len() as u64);
    let dedup_result = hasher::find_duplicates(&files, &dedup_pb, &hash_pool);
    dedup_pb.finish_with_message("deduplication complete");

    // Report duplicates
    reporter::print_duplicates(&dedup_result.duplicate_groups);

    let total_duplicate_files: usize = dedup_result
        .duplicate_groups
        .iter()
        .map(|g| g.files.len() - 1)
        .sum();

    // Initialise reverse geocoder
    println!("\nLoading geocoding data...");
    let geo = GeoLookup::new();

    // Plan all moves
    println!("Planning file organisation...");
    let plan_pb = ProgressBar::new(dedup_result.unique.len() as u64);
    plan_pb.set_style(hasher::styled_bar(
        "[{elapsed_precise}] {bar:40.green/white} {pos}/{len} planning",
    ));

    let output_dir = config.output_dir(settings);
    let mut planned_moves = Vec::new();
    let mut plan_errors = 0;

    // One ledger across both passes, and duplicates claim first because that is
    // the order the committing run moves in. Planning them here rather than
    // inside `move_duplicates` is what lets a *preview* say where a duplicate
    // will land — it used to report them as counts and nothing else.
    let mut ledger = organiser::DestinationLedger::new();
    let duplicate_plans = organiser::plan_duplicate_moves(
        &dedup_result.duplicate_groups,
        output_dir,
        layout.duplicates(),
        &sidecars,
        &mut ledger,
    );

    for unique in &dedup_result.unique {
        match organiser::plan_move(
            &unique.file,
            output_dir,
            &geo,
            &layout,
            &timezone,
            date_policy,
            &sidecars,
            unique.known_hash.clone(),
        ) {
            Ok(mut planned) => {
                // The name this run will actually use, suffix and all, so the
                // preview and the commit describe the same tree.
                planned.destination = ledger.claim(&planned.destination);
                planned_moves.push(planned);
            }
            Err(e) => {
                error!(path = %unique.file.path.display(), error = %e, "failed to plan move");
                plan_errors += 1;
            }
        }
        plan_pb.inc(1);
    }
    plan_pb.finish_with_message("planning complete");

    // Every figure but `organised` is already known, so the two summaries
    // differ in exactly one field rather than in five positional arguments.
    let summary = reporter::RunSummary {
        scanned: files.len(),
        organised: 0,
        duplicate_groups: dedup_result.duplicate_groups.len(),
        duplicate_files: total_duplicate_files,
        scan_skipped,
        hash_skipped: dedup_result.skipped,
        unprocessed: 0,
        errors: plan_errors,
        dates: reporter::DateSourceTally::of(&planned_moves),
        // Counted over the whole index rather than over `planned_moves`: a
        // duplicate's sidecar travels too, and duplicates are not in that list.
        sidecars_found: sidecars.paired(),
        sidecars_moved: 0,
        sidecar_orphans: sidecars.orphans().len(),
    };

    // === DRY RUN (the default): stop here, before anything is moved ===
    if config.is_dry_run() {
        reporter::print_dry_run(&planned_moves, &duplicate_plans);
        reporter::print_sidecar_orphans(sidecars.orphans());
        // No journal, and none reported: a preview moves nothing, so there is
        // nothing to undo.
        reporter::print_summary(&summary, JournalStatus::NotNeeded, fallback_warning);
        println!("{}", reporter::DRY_RUN_BANNER);
        return Ok(());
    }

    // === JOURNAL: opened before the first move, closed on every way out ===
    let mut journal = open_journal(config, settings)?;
    let journal_path = journal.as_ref().map(|j| j.path().to_path_buf());
    let journal_status = || match journal_path.as_deref() {
        Some(path) => JournalStatus::At(path),
        None => JournalStatus::Disabled,
    };

    // === Move duplicates to duplicates/ directory ===
    let duplicates = if dedup_result.duplicate_groups.is_empty() {
        organiser::DuplicateRun::default()
    } else {
        println!(
            "\nMoving duplicates to {}/ directory...",
            layout.duplicates().display()
        );
        let mut recorder = MoveRecorder::new(journal.as_mut());
        match organiser::move_duplicates(
            &dedup_result.duplicate_groups,
            output_dir,
            layout.duplicates(),
            &duplicate_plans,
            &mut recorder,
        ) {
            Ok(run) => {
                println!(
                    "  Moved {} duplicate files ({} errors)",
                    run.moved, run.errors
                );
                run
            }
            // Nothing in the organise pass has run yet, so every planned move
            // is untouched — but the journal still owes a closing line.
            Err(e) => {
                finish_journal(journal.as_mut(), 0, 0, planned_moves.len());
                return Err(e);
            }
        }
    };

    // === PHASE B: PROCESS (chunked) ===
    println!("\nOrganising files...");

    let move_pb = ProgressBar::new(planned_moves.len() as u64);
    move_pb.set_style(hasher::styled_bar(
        "[{elapsed_precise}] {bar:40.yellow/white} {pos}/{len} {msg}",
    ));

    let mut controller = CliController {
        bar: &move_pb,
        prompt: !settings.no_prompt,
    };
    let mut recorder = MoveRecorder::new(journal.as_mut());
    let run = organiser::process_moves(
        &planned_moves,
        settings.chunk_size,
        &mut controller,
        &mut recorder,
        &duplicates.original_manifests,
    );

    // A run that stopped did not complete, and the bar must not claim it did.
    if run.journal_failed {
        move_pb.abandon_with_message("journal write failed");
    } else if run.stopped_early {
        move_pb.abandon_with_message("stopped by user");
        println!(
            "\nStopped by user. {} file{} organised, {} left untouched.",
            run.moved,
            if run.moved == 1 { "" } else { "s" },
            run.unprocessed
        );
    } else {
        move_pb.finish_with_message("organisation complete");
    }

    // Sidecars are folded in here and nowhere else. The journal's closing line
    // counts *entries*, and a sidecar was journalled as one — a `RunCompleted`
    // claiming forty moves for a journal holding eighty would be the one line of
    // the record that disagrees with the rest of it.
    let sidecars_moved = run.sidecars.moved + duplicates.sidecars.moved;
    let sidecar_errors = run.sidecars.errors + duplicates.sidecars.errors;

    // Every exit path from the move phase closes the journal, including the
    // early stop and the journal's own failure. `failed` counts moves that were
    // attempted and did not happen; `skipped` counts files never attempted —
    // those the run stopped before, plus those whose destination could not be
    // planned.
    finish_journal(
        journal.as_mut(),
        run.moved + duplicates.moved + sidecars_moved,
        run.errors + duplicates.errors + sidecar_errors,
        run.unprocessed + plan_errors,
    );

    // Printed on every path out of the move phase, including the early stop —
    // the operator who just interrupted a run is precisely the one who needs
    // to be told what it managed first.
    reporter::print_summary(
        &reporter::RunSummary {
            organised: run.moved,
            unprocessed: run.unprocessed,
            errors: plan_errors + run.errors + duplicates.errors + sidecar_errors,
            sidecars_moved,
            ..summary
        },
        journal_status(),
        fallback_warning,
    );

    // After the summary rather than before it: these files did not move, so the
    // question they raise is one for afterwards, and burying the table an
    // operator is looking for underneath a list of paths would be the wrong way
    // round.
    reporter::print_sidecar_orphans(sidecars.orphans());

    // A run that stopped because it could not record itself is a failed run,
    // and a script driving `mmm` has to be able to tell.
    if run.journal_failed {
        anyhow::bail!(
            "the run journal could not be written, so the run stopped after {} file{} — see the \
             errors above. Nothing further was moved.",
            run.moved,
            if run.moved == 1 { "" } else { "s" }
        );
    }

    Ok(())
}
