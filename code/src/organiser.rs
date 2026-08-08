use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use tracing::{debug, error, info};

use crate::geocoder::GeoLookup;
use crate::hasher::DuplicateGroup;
use crate::journal::{IntentKind, Journal, JournalEntry};
use crate::metadata::{self, DateSource, FileMetadata};
use crate::naming::{sanitise_for_filename, year_is_representable, FilenameParts, Layout};
use crate::scanner::ScannedFile;
use crate::timezone::{TimezonePolicy, TimezoneSource};

/// A planned file operation (computed during scan, executed during process)
#[derive(Debug, Clone)]
pub struct PlannedMove {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub date_source: DateSource,
    /// How the offset behind the destination's dated directory was decided, or
    /// `None` for a file with no usable date at all.
    ///
    /// Carried alongside `date_source` rather than folded into it because they
    /// answer different questions — *what* said when the file was made, and
    /// *which wall clock* that saying was read against — and a run can be
    /// confident about one while guessing at the other.
    pub timezone_source: Option<TimezoneSource>,
    pub has_location: bool,
    /// The source's full BLAKE3 digest, when the dedup cascade already
    /// established one (see [`crate::hasher::UniqueFile`]).
    ///
    /// Journalled so `undo` can refuse a file whose contents changed since the
    /// run — a check size alone cannot make, because a same-length edit passes
    /// it. `None` where no digest was paid for; undo then falls back to size.
    pub known_hash: Option<String>,
}

/// Which dates a run is willing to file a photograph under.
///
/// The conservative posture is not the default, and cannot be: a tool that
/// refused every scan and every screenshot out of the box would be one nobody
/// could point at their own library. It is the posture of somebody who knows
/// their files have been copied between disks — where a modification time is the
/// date of the copy, not of the photograph — and would rather sort those by hand
/// than have the tool guess confidently on their behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DatePolicy {
    /// File under the best date available, filesystem timestamps included.
    #[default]
    AnyDate,
    /// `--require-exif`: file only under a date the file itself recorded.
    /// Everything else goes to `unsorted/`.
    EmbeddedOnly,
}

impl DatePolicy {
    /// The policy `--require-exif` asks for, or the default when it was not
    /// passed.
    #[must_use]
    pub fn from_require_exif(require_exif: bool) -> Self {
        if require_exif {
            Self::EmbeddedOnly
        } else {
            Self::AnyDate
        }
    }

    /// Whether a date established this way may name a dated directory.
    #[must_use]
    pub fn admits(self, source: DateSource) -> bool {
        match self {
            Self::AnyDate => true,
            Self::EmbeddedOnly => source.is_embedded(),
        }
    }
}

/// Build the target path for a file based on its metadata
///
/// # Errors
///
/// Returns an error if the file's metadata cannot be extracted.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is one independent thing a plan depends on, and folding \
              them into a context struct would put a layer between a caller and the \
              decision it is making"
)]
pub fn plan_move(
    file: &ScannedFile,
    output_dir: &Path,
    geo: &GeoLookup,
    layout: &Layout,
    tz: &TimezonePolicy,
    policy: DatePolicy,
    known_hash: Option<String>,
) -> Result<PlannedMove> {
    let meta = metadata::extract_metadata(&file.path, file.is_video, tz)?;

    // The stem is only read by the `{original_stem}` token, but it is derived
    // here for every file rather than inside the format: a file whose name is
    // not valid UTF-8 has no stem this scheme can spell, and the empty string it
    // becomes is answered by the same guard as an empty `{ext}`.
    let original_stem = file
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();

    let (date_dir, filename) =
        build_target_path(&meta, &file.extension, original_stem, geo, layout, policy);
    let destination = output_dir.join(date_dir).join(filename);

    Ok(PlannedMove {
        source: file.path.clone(),
        destination,
        date_source: meta.date_source,
        timezone_source: meta.timezone_source,
        has_location: meta.latitude.is_some() && meta.longitude.is_some(),
        known_hash,
    })
}

/// Build the dated directory and the filename `layout` asks for.
///
/// Total by construction: the directory it returns is either the rendering of
/// [`Scheme::date_directory`] or the layout's `unsorted` directory — both
/// relative paths of ordinary components, whatever the config said — and the
/// filename is always a single ordinary path component. `tests/path_properties.rs`
/// asserts both over generated input *and over generated formats*, which is what
/// closed the four holes described below.
///
/// **The extension is sanitised.** It arrives here as arbitrary text — this
/// function is `pub`, and nothing in its signature stops a caller passing
/// `"../../etc/passwd"`, which used to be pasted in verbatim and produced a
/// destination outside the output tree entirely. Today's only caller is the
/// scanner, which admits none but its own known-media extensions, so this was
/// not reachable from the CLI; it is fixed because the invariant belongs to the
/// function rather than to the discipline of one caller.
///
/// **The date is read as a wall clock, not as an instant.** `meta.date` carries
/// the local offset resolved by [`crate::timezone`], so `dt.year()`,
/// `dt.day()` and `dt.hour()` here are the reading a person would have seen on
/// the camera. This is the fix for evening photographs landing on the following
/// day: nothing in this function converts to UTC, and nothing may start.
///
/// **A year outside four digits goes to the unsorted directory.** See
/// [`crate::naming::year_is_representable`] — printing it produced directories
/// like `44-03-15` and, for the negative years `chrono` will parse out of an
/// EXIF string, `-44-03-15` plus a filename beginning with `-`.
///
/// **A format that renders to nothing goes there too.** A pattern is
/// trial-rendered when it is validated, so this is not reachable from a config
/// file; it is here because `Scheme` renders against a date and a date is not
/// something validation can enumerate. The unsorted directory is the bucket that
/// already means "no filing we can trust", which is better than a directory
/// named after whatever the default happened to be.
///
/// **A date the policy refuses goes there too, keeping its own name.** See
/// [`unsorted_filename`] — this is the one route into `unsorted/` where the file
/// had a perfectly good name and a perfectly good date, and only the *provenance*
/// of the date was rejected.
// exposed for integration tests
pub fn build_target_path(
    meta: &FileMetadata,
    extension: &str,
    original_stem: &str,
    geo: &GeoLookup,
    layout: &Layout,
    policy: DatePolicy,
) -> (PathBuf, String) {
    let extension = sanitise_for_filename(extension);

    // A file that has a date the run will not file under, as distinct from one
    // that has no date at all. The two go to the same directory and must not
    // arrive under the same name.
    let refused = meta.date.is_some() && !policy.admits(meta.date_source);

    let dated = match meta.date {
        Some(dt) if !refused && year_is_representable(dt.year()) => {
            layout.date_directory(&dt).map(|dir| {
                (
                    dir,
                    date_filename(&dt, meta, &extension, original_stem, geo, layout),
                )
            })
        }
        _ => None,
    };

    dated.unwrap_or_else(|| {
        (
            layout.unsorted().to_path_buf(),
            unsorted_filename(refused.then_some(original_stem), &extension),
        )
    })
}

/// The name a file gets in `unsorted/`.
///
/// `keep_stem` is `Some` only for a file [`DatePolicy::EmbeddedOnly`] refused,
/// and it is the difference between a conservative flag and a destructive one.
/// Everything else in `unsorted/` is `unknown.jpg` because there is genuinely
/// nothing to say about it — no date, and by then no reason to trust the name
/// either. A file `--require-exif` sent here is the opposite case: it is
/// `IMG_4471.CR2`, it has a filesystem date the run declined to trust, and its
/// *name* is now the only handle its owner has on it. Throwing that away would
/// make the safe posture the lossy one.
///
/// It is also what makes the flag usable at all. A library of forty thousand
/// RAW files under `--require-exif` would otherwise be forty thousand files
/// competing for `unknown.cr2`, and [`MAX_COLLISION_ATTEMPTS`] stops at ten
/// thousand — so the run would not merely be unhelpful, it would start failing
/// moves a third of the way in.
///
/// Total, like everything else here: [`sanitise_for_filename`] admits only
/// alphanumerics, `-` and `_`, so the result is one ordinary path component, and
/// a stem that sanitises to nothing (an empty or non-UTF-8 filename) falls back
/// to the same [`crate::naming::UNNAMED`] as the undated case.
fn unsorted_filename(keep_stem: Option<&str>, extension: &str) -> String {
    let stem = keep_stem
        .map(sanitise_for_filename)
        .filter(|stem| !stem.is_empty());
    format!(
        "{}.{extension}",
        stem.as_deref().unwrap_or(crate::naming::UNNAMED)
    )
}

/// The name `scheme` gives a file whose date is usable.
///
/// The geocoder is consulted only when the scheme says locations are spelled at
/// all — `include_location = false` is a run that does not pay for the lookups
/// it would then discard — and only when the file has coordinates.
fn date_filename(
    dt: &DateTime<FixedOffset>,
    meta: &FileMetadata,
    extension: &str,
    original_stem: &str,
    geo: &GeoLookup,
    layout: &Layout,
) -> String {
    let date = format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day());
    let time = format!("{:02}{:02}{:02}", dt.hour(), dt.minute(), dt.second());

    let location = match (layout.include_location(), meta.latitude, meta.longitude) {
        (true, Some(lat), Some(lon)) => geo
            .lookup(lat, lon)
            // The separator belongs to the token, not to the pattern: a file
            // with no coordinates must not leave a dangling `-` behind.
            .map_or_else(String::new, |info| format!("-{}", info.filename_part)),
        _ => String::new(),
    };

    layout.filename(&FilenameParts {
        date: &date,
        time: &time,
        location: &location,
        extension,
        original_stem,
    })
}

/// The number of destination candidates tried before a move gives up.
///
/// A photo library would need ten thousand files claiming the same second and
/// the same location to exhaust this. Giving up is the right end of the range:
/// the alternative to a bounded search is an unbounded one, and the whole
/// point of this module is that no path ends in "overwrite it anyway".
const MAX_COLLISION_ATTEMPTS: usize = 10_000;

/// The `attempt`-th candidate destination for `path`.
///
/// Attempt 0 is `path` itself; attempt *n* is `stem-n.ext`. This is a pure
/// function of the path — it asks the filesystem nothing, deliberately.
/// The previous `resolve_collision` called `Path::exists()` and handed its
/// answer to `fs::rename`, which is wrong twice over: the answer is stale the
/// instant it is returned, and `exists()` follows symlinks, so a dangling link
/// reads as "nothing here" while the directory entry is very much there.
///
/// [`move_no_clobber`] is now the only authority on whether a candidate is
/// free, and it answers by failing rather than by overwriting. This function
/// only says which name to try next.
pub fn collision_candidate(path: &Path, attempt: usize) -> PathBuf {
    if attempt == 0 {
        return path.to_path_buf();
    }

    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let parent = path.parent().unwrap_or(Path::new("."));

    if ext.is_empty() {
        parent.join(format!("{stem}-{attempt}"))
    } else {
        parent.join(format!("{stem}-{attempt}.{ext}"))
    }
}

/// The record of one duplicate group, written before that group's files move.
///
/// Ordering is the whole point. The previous version accumulated the manifest
/// text in a `String` and wrote it after the last move in the group, so a run
/// interrupted anywhere in between — a kill, a full disk, an unplugged drive —
/// left duplicates relocated into `duplicates/NNN/` with nothing on disk saying
/// where they had come from. The files were safe and the map to them was not,
/// which for a photo library is close to the same thing: `photo.jpg` in a
/// numbered directory is unrecoverable without the path it was moved from.
///
/// So the header and the *complete* intended source list are written and synced
/// before the first move is attempted, and each outcome is appended and synced
/// as it happens. A manifest read from a half-finished run therefore says both
/// what was meant to happen and how far it got.
///
/// The format stays compatible with `mmm-dedup-verifier`, which reads every
/// non-`#` line as an intended source path. Outcome lines are comments for that
/// reason.
struct GroupManifest {
    file: fs::File,
    path: PathBuf,
}

impl GroupManifest {
    /// Create the manifest and write everything known before the moves begin.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or the header cannot be
    /// written — in which case the group must not be moved at all, because
    /// moving it would be the unrecorded relocation this type exists to
    /// prevent.
    fn create(path: &Path, index: usize, group: &DuplicateGroup) -> Result<Self> {
        let mut header = format!(
            "# Duplicate group {index:03}\n\
             # BLAKE3 hash: {}\n\
             # File size: {} bytes\n\
             # Original kept at: {}\n\
             # Duplicates intended for this directory: {}\n\
             #\n\
             # The paths below are written before the first move, so an\n\
             # interrupted run still records where every file came from.\n\
             # Outcomes follow, appended one line at a time as each move ends.\n\n",
            group.hash,
            group.size,
            group.files[0].display(),
            group.files.len().saturating_sub(1),
        );
        for source in group.files.iter().skip(1) {
            let _ = writeln!(header, "{}", source.display());
        }
        header.push_str("\n# Outcomes\n");

        let mut file = fs::File::create(path)
            .with_context(|| format!("creating manifest {}", path.display()))?;
        io::Write::write_all(&mut file, header.as_bytes())
            .with_context(|| format!("writing manifest {}", path.display()))?;
        file.sync_data()
            .with_context(|| format!("flushing manifest {} to disk", path.display()))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Append one line and put it on the disk before returning.
    ///
    /// # Errors
    ///
    /// Returns the underlying write or sync error. The caller stops moving the
    /// group rather than continuing without a record.
    fn append(&mut self, line: &str) -> Result<()> {
        io::Write::write_all(&mut self.file, line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .with_context(|| format!("appending to manifest {}", self.path.display()))
    }

    /// Record where a duplicate actually landed — suffix and all, because a
    /// record that says only "moved" cannot be used to put anything back.
    ///
    /// # Errors
    ///
    /// As [`GroupManifest::append`].
    fn record_move(&mut self, src: &Path, dst: &Path) -> Result<()> {
        self.append(&format!(
            "# moved: {} -> {}\n",
            src.display(),
            dst.display()
        ))
    }

    /// Record a move that did not happen, and why.
    ///
    /// # Errors
    ///
    /// As [`GroupManifest::append`].
    fn record_failure(&mut self, src: &Path, reason: &str) -> Result<()> {
        self.append(&format!("# FAILED: {}: {reason}\n", src.display()))
    }
}

/// What a move is for, and therefore what its commit record says.
///
/// The two move passes differ in exactly this and nothing else, so it is the
/// only thing [`recorded_move`] takes beyond the move itself. A duplicate's
/// record carries the group it belongs to, because `duplicates/007/photo.jpg`
/// is meaningless without it, and its BLAKE3 digest.
///
/// An organise move carries a digest only when the cascade reached phase 3 for
/// that file and therefore already paid for one — never by hashing on purpose.
/// See [`crate::hasher::UniqueFile`].
#[derive(Debug, Clone, Copy)]
pub enum MovePurpose<'a> {
    /// A media file moving into the dated output tree.
    Organise { hash: Option<&'a str> },
    /// A duplicate moving into `duplicates/<group>/`.
    Duplicate { group: usize, hash: &'a str },
    /// `mmm undo` putting a file back where a previous run found it.
    ///
    /// Carries the hash the original run recorded, when it recorded one, so an
    /// undo's own journal describes the file it moved as precisely as the run
    /// it is reversing did — which is what makes an undo undoable on the same
    /// terms as everything else.
    Restore { hash: Option<&'a str> },
}

impl MovePurpose<'_> {
    fn intent_kind(self) -> IntentKind {
        match self {
            Self::Organise { .. } => IntentKind::Organise,
            Self::Duplicate { .. } => IntentKind::Duplicate,
            Self::Restore { .. } => IntentKind::Restore,
        }
    }

    fn source_hash(self) -> Option<String> {
        match self {
            Self::Duplicate { hash, .. } => Some(hash.to_string()),
            Self::Organise { hash } | Self::Restore { hash } => hash.map(ToString::to_string),
        }
    }
}

/// Where a move pass writes its record, or nothing.
///
/// The alternative — passing `Option<&mut Journal>` down to each loop — puts
/// the "record, then move" ordering at every call site, which is the one place
/// it must not live: the ordering is the safety property, and a call site that
/// forgets it produces a move nothing can reverse. Here it is written once, in
/// [`recorded_move`], and both passes go through it.
pub struct MoveRecorder<'a> {
    sink: Sink<'a>,
}

enum Sink<'a> {
    /// `--no-journal`, and the tests that are not about journalling.
    Off,
    Open(&'a mut Journal),
    /// Every write fails. An open file descriptor cannot be made to fail from
    /// a test — the disk going away mid-run is not reproducible — so the one
    /// behaviour that matters, *stop rather than move unrecorded*, is driven
    /// through this instead. Same reasoning as the injected `copy` parameter
    /// on [`copy_verify_delete`].
    #[cfg(test)]
    Failing,
}

impl<'a> MoveRecorder<'a> {
    pub fn new(journal: Option<&'a mut Journal>) -> Self {
        Self {
            sink: journal.map_or(Sink::Off, Sink::Open),
        }
    }

    /// A recorder that records nothing.
    pub fn disabled() -> Self {
        Self { sink: Sink::Off }
    }

    #[cfg(test)]
    fn failing() -> Self {
        Self {
            sink: Sink::Failing,
        }
    }

    fn append(&mut self, entry: &JournalEntry) -> Result<()> {
        match &mut self.sink {
            Sink::Off => Ok(()),
            Sink::Open(journal) => journal.append(entry),
            #[cfg(test)]
            Sink::Failing => bail!("the journal is on a disk that went away"),
        }
    }

    /// Record the intent to move `planned`, returning the sequence number its
    /// outcome must be recorded under, or `None` when nothing is being recorded.
    ///
    /// Does not return until the entry is on the disk. A caller may only move
    /// the file once this has succeeded.
    fn intend(&mut self, planned: &PlannedMove, purpose: MovePurpose<'_>) -> Result<Option<u64>> {
        let seq = match &mut self.sink {
            Sink::Off => return Ok(None),
            Sink::Open(journal) => journal.next_seq(),
            #[cfg(test)]
            Sink::Failing => 0,
        };

        // Stat now rather than carrying the scan's figure: undo compares this
        // against the file it finds at the destination, so the size that
        // matters is the one the file had immediately before it moved. A stat
        // that fails means the move is about to fail too and say why — the
        // journal records what it can and lets the move report the cause.
        let source_size = fs::metadata(&planned.source)
            .map(|m| m.len())
            .unwrap_or_default();

        self.append(&JournalEntry::MoveIntent {
            seq,
            source: planned.source.clone(),
            destination: planned.destination.clone(),
            source_size,
            source_hash: purpose.source_hash(),
            kind: purpose.intent_kind(),
        })?;

        Ok(Some(seq))
    }

    /// Record where the file actually landed — which is not always where it was
    /// planned to, once collision resolution has had its say.
    fn commit(
        &mut self,
        seq: Option<u64>,
        purpose: MovePurpose<'_>,
        source: &Path,
        outcome: &MoveOutcome,
    ) -> Result<()> {
        let Some(seq) = seq else { return Ok(()) };

        let entry = match purpose {
            // A restore is an ordinary committed move as far as the journal is
            // concerned — its *reason* is already on the intent line, and
            // giving it a record type of its own would mean a third thing
            // `undo` has to recognise to reverse an undo.
            MovePurpose::Organise { .. } | MovePurpose::Restore { .. } => {
                JournalEntry::MoveCommitted {
                    seq,
                    final_destination: outcome.destination.clone(),
                    move_kind: outcome.kind,
                }
            }
            MovePurpose::Duplicate { group, .. } => JournalEntry::DuplicateMoved {
                seq,
                group,
                source: source.to_path_buf(),
                destination: outcome.destination.clone(),
            },
        };
        self.append(&entry)
    }

    /// Record that the move named by `seq` did not happen. The source is still
    /// where it was, which is what makes this different from an intent with no
    /// outcome at all.
    fn failed(&mut self, seq: Option<u64>, reason: &str) -> Result<()> {
        let Some(seq) = seq else { return Ok(()) };
        self.append(&JournalEntry::MoveFailed {
            seq,
            reason: reason.to_string(),
        })
    }
}

/// Why a recorded move produced no moved file.
///
/// The split is the same one [`MoveError`] makes one level down, for the same
/// reason: the caller has to tell "this photo did not move, carry on" from
/// "stop the run". A journal that cannot be written turns every later move into
/// an unrecorded one, which is the single failure this module exists to prevent.
#[derive(Debug)]
pub enum RecordedMoveError {
    /// The move failed. It has been recorded as such and the run may continue.
    Move(anyhow::Error),
    /// The journal could not be written.
    ///
    /// `moved` says whether the file moved before the record failed: an intent
    /// that cannot be written stops the move from being attempted, but an
    /// outcome that cannot be written does not un-move the file. The caller
    /// needs the difference to count honestly.
    Journal { error: anyhow::Error, moved: bool },
}

/// Record the intent, perform the move, record the outcome.
///
/// The ordering is the whole point and is stated here once: the intent is on
/// the disk *before* [`execute_move`] is called, so a run killed between the
/// two leaves a journal naming a file as possibly moved rather than a library
/// with an unrecorded hole in it.
///
/// # Errors
///
/// [`RecordedMoveError::Move`] if the move failed, and
/// [`RecordedMoveError::Journal`] if any of the three journal writes did.
pub(crate) fn recorded_move(
    recorder: &mut MoveRecorder<'_>,
    planned: &PlannedMove,
    purpose: MovePurpose<'_>,
) -> Result<MoveOutcome, RecordedMoveError> {
    let seq = recorder
        .intend(planned, purpose)
        .map_err(|error| RecordedMoveError::Journal {
            error,
            moved: false,
        })?;

    match execute_move(planned) {
        Ok(outcome) => {
            recorder
                .commit(seq, purpose, &planned.source, &outcome)
                .map_err(|error| RecordedMoveError::Journal { error, moved: true })?;
            Ok(outcome)
        }
        Err(e) => {
            recorder.failed(seq, &format!("{e:#}")).map_err(|error| {
                RecordedMoveError::Journal {
                    error,
                    moved: false,
                }
            })?;
            Err(RecordedMoveError::Move(e))
        }
    }
}

/// Move duplicate files into numbered subdirectories under the duplicates
/// directory — `duplicates/000/`, `duplicates/001/`, and so on, or whatever
/// `duplicates_dir` renamed it to.
/// The first file in each group is the "original" and is NOT moved here.
///
/// `duplicates_dir` is relative to `output_dir` and comes from the run's
/// [`Layout`], which is the proof it cannot leave the output tree.
///
/// Each group's `manifest.txt` is written in full before any of that group's
/// files move, and each outcome is appended as it happens — see
/// [`GroupManifest`]. Every relocation additionally goes through `recorder`, so
/// `mmm undo` can put duplicates back alongside the ordinary moves; the
/// manifest stays because it is the per-group record a person reads, and
/// `mmm-dedup-verifier` parses.
///
/// # Errors
///
/// Returns an error if a `duplicates/NNN/` directory or its `manifest.txt`
/// cannot be created, or if the journal cannot be written — in which case the
/// remaining duplicates are left where they are rather than relocated with no
/// record of where they came from. Individual failed moves are recorded and
/// counted, not propagated.
pub fn move_duplicates(
    groups: &[DuplicateGroup],
    output_dir: &Path,
    duplicates_dir: &Path,
    recorder: &mut MoveRecorder<'_>,
) -> Result<(usize, usize)> {
    let dup_base = output_dir.join(duplicates_dir);
    let mut moved = 0;
    let mut errors = 0;

    for (i, group) in groups.iter().enumerate() {
        let group_dir = dup_base.join(format!("{i:03}"));
        fs::create_dir_all(&group_dir)
            .with_context(|| format!("creating duplicate dir {}", group_dir.display()))?;

        // Before a single file moves.
        let mut manifest = GroupManifest::create(&group_dir.join("manifest.txt"), i, group)?;

        // Skip the first file (kept as original), move the rest
        for (done, dup_path) in group.files.iter().skip(1).enumerate() {
            let filename = dup_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            // No pre-flight collision check: `execute_move` walks the
            // candidate names itself and lets the move be the authority on
            // which one is free.
            let dest = group_dir.join(&filename);

            let planned = PlannedMove {
                source: dup_path.clone(),
                destination: dest,
                date_source: DateSource::None,
                // A duplicate is filed by its group, not by its date, so no
                // wall clock was ever chosen for it.
                timezone_source: None,
                has_location: false,
                // The duplicate pass carries its digest on the purpose, which
                // is where the journal reads it from for this kind of move.
                known_hash: None,
            };

            let purpose = MovePurpose::Duplicate {
                group: i,
                hash: &group.hash,
            };

            let manifested = match recorded_move(recorder, &planned, purpose) {
                Ok(outcome) => {
                    moved += 1;
                    manifest.record_move(dup_path, &outcome.destination)
                }
                Err(RecordedMoveError::Move(e)) => {
                    error!(path = %dup_path.display(), error = %e, "failed to move duplicate");
                    errors += 1;
                    manifest.record_failure(dup_path, &format!("{e:#}"))
                }
                // The journal is what `undo` replays. Without it the rest of
                // this group would be relocated into a numbered directory with
                // nothing on disk saying where the files came from, which for a
                // photo library is close to losing them.
                Err(RecordedMoveError::Journal { error, .. }) => {
                    return Err(error.context(format!(
                        "the run journal could not be written while relocating duplicate {}; \
                         the remaining duplicates have been left where they are",
                        dup_path.display()
                    )))
                }
            };

            // A manifest that cannot be appended to stops the group. The
            // alternative is to keep moving files whose new locations nothing
            // is recording, which is the failure this whole structure exists
            // to avoid — better to leave the rest where the user can still
            // find them.
            if let Err(e) = manifested {
                let abandoned = group.files.len() - 1 - done - 1;
                error!(
                    group = i,
                    error = %e,
                    abandoned,
                    "manifest is no longer writable; leaving the rest of this group in place"
                );
                errors += abandoned;
                break;
            }
        }
    }

    Ok((moved, errors))
}

/// What a completed move actually did.
///
/// Worth recording because the two are not equivalent under interruption: a
/// same-volume move creates a directory entry and drops another, and cannot
/// half-happen to the file's contents, while a cross-volume move reads and
/// rewrites every byte. Callers and the journal want to know which one moved
/// a given photo.
///
/// Serialisable because [`crate::journal`] records it verbatim: one definition
/// of what a move did, shared by the code that performs it and the code that
/// reverses it, rather than a parallel enum in the journal that could drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveKind {
    /// Same volume: the destination was linked to the source's inode and the
    /// source link dropped. No data was copied.
    Renamed,
    /// Different volumes: copied to a temp file, verified, promoted into
    /// place, and only then was the source removed.
    CrossVolume,
}

/// What a move did, and where it ended up.
///
/// The destination is not always the one that was planned: [`execute_move`]
/// walks the collision candidates, so a file planned for `photo.jpg` can land
/// at `photo-1.jpg`. Callers that record the move — the duplicate manifest
/// today, the journal later — need the name the file actually has, since that
/// is the only one that can be used to find it again.
#[derive(Debug, Clone)]
pub struct MoveOutcome {
    pub kind: MoveKind,
    pub destination: PathBuf,
}

impl std::fmt::Display for MoveKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Renamed => f.write_str("same-volume link"),
            Self::CrossVolume => f.write_str("cross-volume copy+verify+delete"),
        }
    }
}

/// Why a no-clobber move did not happen.
///
/// The split exists because the caller has to tell "that name is taken, try
/// the next one" from "stop, something is wrong". `anyhow` alone cannot carry
/// that distinction without downcasting to an `io::Error` that context has
/// already wrapped, and getting it wrong in either direction is expensive: a
/// missed retry loses a photo's move, a spurious one writes a `-1` copy next
/// to a file that failed for an unrelated reason.
#[derive(Debug)]
pub enum MoveError {
    /// `dst` already exists. Not fatal — [`execute_move`] tries the next
    /// candidate name.
    DestinationExists(PathBuf),
    /// Anything else, with its context already attached.
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for MoveError {
    fn from(err: anyhow::Error) -> Self {
        Self::Fatal(err)
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DestinationExists(path) => {
                write!(f, "destination {} already exists", path.display())
            }
            // `{:#}` rather than `{}`: the whole context chain, because the
            // outermost layer alone ("moving X to Y") never says what went
            // wrong, and this is what an operator reads off a failed run.
            Self::Fatal(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for MoveError {}

/// The step of a same-volume move that failed.
///
/// Only a failed `link` can mean "these two paths are on different volumes";
/// a failed `unlink` of the source never does, and must not be allowed to send
/// the move down the copy path.
#[derive(Debug, Clone, Copy)]
enum LinkStep {
    Link,
    UnlinkSource,
}

/// What a failed `link(2)` says about the two paths.
///
/// The whole point of the split is that "the move failed" is not a reason to
/// copy. Only one condition means "these paths cannot be linked, so the bytes
/// have to travel"; everything else is a real problem the operator needs told
/// about, and answering it with a full read-and-rewrite of the file both wastes
/// the work and buries the actual cause under a temp-file error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkFailure {
    /// `EEXIST` — something occupies `dst`. Not fatal: try the next candidate.
    DestinationTaken,
    /// `EXDEV` — source and destination live on different volumes. The only
    /// condition that has ever justified the copy path.
    DifferentVolume,
    /// The destination filesystem has no hard links at all — exFAT and FAT32,
    /// which is what most SD cards and external drives are formatted as. Same
    /// volume, but `link` can never succeed here, so the copy path is the only
    /// route and it is a legitimate one.
    LinksUnsupported,
    /// Anything else: a missing source, a denied write, a read-only mount, a
    /// full disk. The move must fail and say so.
    Fatal,
}

/// Errno values consulted directly because [`io::ErrorKind`] cannot tell the
/// cases apart.
///
/// `EPERM` and `EACCES` both arrive as `PermissionDenied`, and the distinction
/// between them is the whole question here: `link` answers `EPERM` when the
/// filesystem has no hard links to give, and `EACCES` when the caller may not
/// write to the directory. One is a copy, the other is a hard stop. `ENOTSUP`
/// has no mapping at all and arrives as `Uncategorized`.
///
/// `EXDEV` is deliberately absent — `ErrorKind::CrossesDevices` is stable and
/// std maps the errno to it, so a raw check would be a second spelling of the
/// same test.
#[cfg(unix)]
mod errno {
    /// "Operation not permitted": from `link(2)`, the filesystem has no hard
    /// links. Distinct from `EACCES`, which is a permission denial.
    pub const EPERM: i32 = 1;

    /// `ENOTSUP` / `EOPNOTSUPP`, which some filesystems return in place of
    /// `EPERM` for an unsupported `link`. macOS numbers the two separately;
    /// Linux defines them as the same value.
    #[cfg(target_os = "macos")]
    pub const NOT_SUPPORTED: &[i32] = &[45, 102];
    #[cfg(not(target_os = "macos"))]
    pub const NOT_SUPPORTED: &[i32] = &[95];
}

/// Classify a failed `link(2)` into the one question the caller has to answer:
/// try another name, copy the bytes, or stop.
fn classify_link_failure(err: &io::Error) -> LinkFailure {
    match err.kind() {
        io::ErrorKind::AlreadyExists => return LinkFailure::DestinationTaken,
        io::ErrorKind::CrossesDevices => return LinkFailure::DifferentVolume,
        io::ErrorKind::Unsupported => return LinkFailure::LinksUnsupported,
        _ => {}
    }

    #[cfg(unix)]
    if let Some(raw) = err.raw_os_error() {
        if raw == errno::EPERM || errno::NOT_SUPPORTED.contains(&raw) {
            return LinkFailure::LinksUnsupported;
        }
    }

    LinkFailure::Fatal
}

/// `link(src, dst)` then `unlink(src)` — a same-volume move that cannot
/// overwrite `dst`.
///
/// `link(2)` fails with `EEXIST` if anything at all occupies `dst`, including
/// a dangling symlink, which is precisely the question `Path::exists()` gets
/// wrong. It is also the reason this is not `fs::rename`: rename replaces the
/// destination silently and unconditionally, and there is no flag on the
/// stable `std` API to ask it not to.
///
/// Not atomic in the way rename is — there is a window where both names point
/// at the file — but the window contains no state in which data is missing,
/// and the unlink failure path below closes it rather than leaving two names.
fn link_and_unlink(src: &Path, dst: &Path) -> Result<(), (LinkStep, io::Error)> {
    fs::hard_link(src, dst).map_err(|e| (LinkStep::Link, e))?;

    if let Err(e) = fs::remove_file(src) {
        // The link landed but the source will not go away. Undo the link
        // rather than leave the run with two names for one file, which the
        // dedup pass would later "helpfully" report as a duplicate.
        let _ = fs::remove_file(dst);
        return Err((LinkStep::UnlinkSource, e));
    }

    Ok(())
}

/// Move `src` to `dst`, failing if `dst` is taken rather than overwriting it.
///
/// # Errors
///
/// [`MoveError::DestinationExists`] when something already occupies `dst` —
/// the caller is expected to try another name — and [`MoveError::Fatal`] for
/// everything else.
fn move_no_clobber(src: &Path, dst: &Path) -> Result<MoveKind, MoveError> {
    match link_and_unlink(src, dst) {
        Ok(()) => Ok(MoveKind::Renamed),

        Err((LinkStep::Link, e)) => {
            let failure = classify_link_failure(&e);
            match failure {
                LinkFailure::DestinationTaken => {
                    Err(MoveError::DestinationExists(dst.to_path_buf()))
                }

                LinkFailure::DifferentVolume | LinkFailure::LinksUnsupported => {
                    debug!(
                        src = %src.display(),
                        dst = %dst.display(),
                        reason = ?failure,
                        "link is impossible between these paths, copying instead"
                    );
                    cross_volume_move(src, dst).map(|()| MoveKind::CrossVolume)
                }

                LinkFailure::Fatal => {
                    Err(MoveError::Fatal(anyhow::Error::new(e).context(format!(
                        "moving {} to {}",
                        src.display(),
                        dst.display()
                    ))))
                }
            }
        }

        Err((LinkStep::UnlinkSource, e)) => {
            Err(MoveError::Fatal(anyhow::Error::new(e).context(format!(
                "removing source {} after linking it to {}",
                src.display(),
                dst.display()
            ))))
        }
    }
}

/// Execute a planned move, never overwriting an existing file
///
/// Walks the collision candidates for the planned destination and lets
/// [`move_no_clobber`] decide which one is free, rather than asking the
/// filesystem beforehand and trusting the answer to still hold.
///
/// # Errors
///
/// Returns an error if the destination has no parent directory, if that
/// directory cannot be created, if the move itself fails, or if every one of
/// [`MAX_COLLISION_ATTEMPTS`] candidate names is taken.
pub fn execute_move(planned: &PlannedMove) -> Result<MoveOutcome> {
    let dest_dir = planned
        .destination
        .parent()
        .context("destination has no parent directory")?;

    // Create target directory
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating directory {}", dest_dir.display()))?;

    for attempt in 0..MAX_COLLISION_ATTEMPTS {
        let candidate = collision_candidate(&planned.destination, attempt);

        match move_no_clobber(&planned.source, &candidate) {
            Ok(kind) => {
                info!(
                    src = %planned.source.display(),
                    dst = %candidate.display(),
                    kind = %kind,
                    "moved"
                );
                return Ok(MoveOutcome {
                    kind,
                    destination: candidate,
                });
            }
            Err(MoveError::DestinationExists(taken)) => {
                debug!(
                    src = %planned.source.display(),
                    candidate = %taken.display(),
                    "destination taken, trying the next candidate"
                );
            }
            Err(MoveError::Fatal(e)) => return Err(e),
        }
    }

    bail!(
        "no free destination for {} after {} candidates around {}",
        planned.source.display(),
        MAX_COLLISION_ATTEMPTS,
        planned.destination.display()
    )
}

/// What the chunked move phase did.
///
/// Returned rather than acted upon, because the caller owes the operator a
/// summary and a summary is only worth printing if it is complete. Every file
/// handed to [`process_moves`] is accounted for in exactly one of these three
/// counts: `moved + errors + unprocessed == planned.len()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MoveRun {
    /// Files that reached the output tree.
    pub moved: usize,
    /// Files whose move failed. Counted, logged, and stepped over.
    pub errors: usize,
    /// Files never attempted, because the run stopped before reaching them.
    pub unprocessed: usize,
    /// Whether the run ended at a chunk boundary rather than at the end.
    pub stopped_early: bool,
    /// Whether the run stopped because the journal could not be written.
    ///
    /// Distinct from [`MoveRun::stopped_early`], which is the operator's
    /// decision. This one is a failure, and the caller is expected to say so.
    pub journal_failed: bool,
}

/// How the caller observes and steers a chunked run.
///
/// The three concerns the old inline loop mixed together — progress display,
/// asking the operator, and deciding whether to carry on — are the three
/// methods here, and all of them belong to the *caller*. The library moves
/// files; it does not own a terminal, and it must not end a process.
///
/// Every method has a default, so a caller that wants none of it (a test, a
/// future non-interactive mode) implements the trait and writes nothing.
pub trait ChunkController {
    /// A chunk is about to be processed. `chunk_number` is 1-based.
    fn chunk_started(&mut self, chunk_number: usize, chunks: usize) {
        let _ = (chunk_number, chunks);
    }

    /// One file has been dealt with, successfully or not.
    fn file_finished(&mut self) {}

    /// Carry on to the next chunk? Asked only when files remain, so the last
    /// chunk never prompts — there is nothing left to consent to.
    fn should_continue(&mut self, chunk_number: usize, remaining: usize) -> bool {
        let _ = (chunk_number, remaining);
        true
    }
}

/// Execute `planned` in chunks, stopping cleanly when the controller declines.
///
/// This is the whole of phase B, extracted so that stopping is a `break` with a
/// value the summary can consume rather than a `std::process::exit(0)` fired
/// from inside a progress-bar closure. That exit skipped the summary, skipped
/// every destructor between it and `main`, and made the honest question — "what
/// did the run manage before I stopped it?" — answerable only by inspecting the
/// tree afterwards.
///
/// Infallible by signature: a failed move is counted and the next file is
/// attempted, exactly as the scanner and the hashing passes treat an unreadable
/// file. One photo that cannot move is not a reason to abandon the rest.
///
/// A journal that cannot be written *is* such a reason, and is the one
/// condition that stops the run on its own: every move after it would be one
/// `undo` could not reverse. That stop is reported as
/// [`MoveRun::journal_failed`] rather than as a failed move, because the files
/// left behind are untouched, not broken.
pub fn process_moves(
    planned: &[PlannedMove],
    chunk_size: usize,
    controller: &mut impl ChunkController,
    recorder: &mut MoveRecorder<'_>,
) -> MoveRun {
    let total = planned.len();
    // `slice::chunks` panics on a zero size, and `--chunk-size 0` is one
    // keystroke away on the command line. Read as "do not chunk": one chunk
    // holding everything, so the operator who asked for no chunking is not
    // then prompted once per file.
    let chunk_size = if chunk_size == 0 {
        total.max(1)
    } else {
        chunk_size
    };
    let chunks: Vec<&[PlannedMove]> = planned.chunks(chunk_size).collect();
    let chunk_count = chunks.len();

    let mut run = MoveRun::default();

    for (i, chunk) in chunks.iter().enumerate() {
        controller.chunk_started(i + 1, chunk_count);

        for planned in *chunk {
            let purpose = MovePurpose::Organise {
                hash: planned.known_hash.as_deref(),
            };
            match recorded_move(recorder, planned, purpose) {
                Ok(_) => run.moved += 1,
                Err(RecordedMoveError::Move(e)) => {
                    error!(
                        src = %planned.source.display(),
                        dst = %planned.destination.display(),
                        error = %format!("{e:#}"),
                        "move failed"
                    );
                    run.errors += 1;
                }
                Err(RecordedMoveError::Journal { error, moved }) => {
                    // The file may or may not have moved — `moved` says which —
                    // but either way nothing after this point could be
                    // recorded, so nothing after this point is attempted.
                    if moved {
                        run.moved += 1;
                    }
                    error!(
                        src = %planned.source.display(),
                        moved,
                        error = %format!("{error:#}"),
                        "the run journal could not be written; stopping so that no further move \
                         goes unrecorded"
                    );
                    run.journal_failed = true;
                }
            }
            controller.file_finished();

            if run.journal_failed {
                break;
            }
        }

        let remaining = total - (run.moved + run.errors);
        if run.journal_failed {
            run.unprocessed = remaining;
            break;
        }
        if remaining > 0 && !controller.should_continue(i + 1, remaining) {
            run.stopped_early = true;
            run.unprocessed = remaining;
            break;
        }
    }

    run
}

/// Claim `dst` with `O_CREAT | O_EXCL`, then rename `temp` over the placeholder
/// we ourselves just created.
///
/// The fallback for filesystems with no hard links. `create_new` is the same
/// question `link` answers with `EEXIST` — "is this name free?" — asked in a
/// way exFAT and FAT32 can answer, and it is atomic against another writer
/// claiming the name first. It also refuses a dangling symlink, because
/// `O_CREAT | O_EXCL` fails `EEXIST` on a symlink whether or not its target
/// exists, which is the behaviour that made `Path::exists()` unfit.
///
/// The `rename` here is the one overwrite in this module, and the thing it
/// overwrites is the empty placeholder we hold.
fn reserve_and_rename(temp: &Path, dst: &Path) -> Result<(), MoveError> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
    {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(MoveError::DestinationExists(dst.to_path_buf()))
        }
        Err(e) => {
            return Err(MoveError::Fatal(
                anyhow::Error::new(e).context(format!("claiming destination {}", dst.display())),
            ))
        }
    }

    fs::rename(temp, dst).map_err(|e| {
        // Drop our placeholder — leaving an empty file where a photo was meant
        // to go is worse than leaving nothing.
        let _ = fs::remove_file(dst);
        MoveError::Fatal(anyhow::Error::new(e).context(format!(
            "renaming the verified copy {} into place at {}",
            temp.display(),
            dst.display()
        )))
    })
}

/// Move the verified temp file onto `dst`, failing rather than overwriting.
fn promote_into_place(temp: &Path, dst: &Path) -> Result<(), MoveError> {
    match link_and_unlink(temp, dst) {
        Ok(()) => Ok(()),

        Err((LinkStep::Link, e)) => match classify_link_failure(&e) {
            LinkFailure::DestinationTaken => Err(MoveError::DestinationExists(dst.to_path_buf())),

            LinkFailure::LinksUnsupported => reserve_and_rename(temp, dst),

            // The temp file was written into the destination's own directory,
            // so `EXDEV` here would mean the two are on different volumes while
            // sharing a parent. Treat it as the anomaly it is rather than
            // papering over it with another copy.
            LinkFailure::DifferentVolume | LinkFailure::Fatal => {
                Err(MoveError::Fatal(anyhow::Error::new(e).context(format!(
                    "promoting the verified copy {} into place at {}",
                    temp.display(),
                    dst.display()
                ))))
            }
        },

        Err((LinkStep::UnlinkSource, e)) => {
            Err(MoveError::Fatal(anyhow::Error::new(e).context(format!(
                "removing the temp file {} after promoting it to {}",
                temp.display(),
                dst.display()
            ))))
        }
    }
}

/// Deletes the file it names when dropped, unless disarmed.
///
/// The copy path has six ways to leave early — a failed copy, a failed hash, a
/// digest mismatch, an occupied destination, a failed promotion, a failed
/// source removal — and each one used to need its own `let _ = remove_file`.
/// Scattering the cleanup means the next early return added is the one that
/// forgets it, and the symptom is `.tmp-1748…` files accumulating in somebody's
/// photo library, indistinguishable from the photos except by name.
///
/// The guard is deliberately silent on failure: it runs during unwinding, when
/// there is already an error on its way to the operator, and a leftover temp
/// file is not worth displacing it.
struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Stop tracking the file — it has been moved away and the path is either
    /// free or somebody else's now.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A temp filename unique within this process.
///
/// The millisecond alone is not unique — a run moving small files clears
/// several per millisecond — and two moves sharing a temp name would have one
/// overwrite the other's copy. `copy_hashing` creates the temp with
/// `O_CREAT | O_EXCL` and would refuse rather than corrupt, but refusing a move
/// over a clock collision is still a failure nobody should have to read about.
fn temp_file_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    format!(
        ".tmp-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Copy `src` to `dst` via a verified temp file, then delete `src`.
///
/// The source is only ever deleted after the bytes at the destination have been
/// proved identical to the bytes that were read — **identical in content**, not
/// merely in length. Comparing `metadata().len()` is the defect this replaces:
/// a copy truncated and padded, a copy off a failing drive, a copy through a
/// filesystem that silently substituted a block, all have the right length and
/// the wrong contents, and all passed a size check on the way to
/// `fs::remove_file` on the original.
///
/// `copy` is a parameter so the corruption can be injected in a test at the one
/// place a bad drive or cable would introduce it — between reading the source
/// and writing the copy. It is handed the source and the temp path and must
/// return the BLAKE3 digest of what it *read*; [`crate::hasher::copy_hashing`]
/// is the real implementation and streams the file once, hashing as it writes.
///
/// # Errors
///
/// [`MoveError::DestinationExists`] if something occupies `dst`, and
/// [`MoveError::Fatal`] if the copy, the verification or the source removal
/// fails. On every one of those paths the temp file is removed and the source
/// is left exactly where it was.
pub fn copy_verify_delete<C>(src: &Path, dst: &Path, copy: C) -> Result<(), MoveError>
where
    C: FnOnce(&Path, &Path) -> Result<String>,
{
    let dst_dir = dst.parent().context("destination has no parent")?;

    // The temp file lives beside the destination, not beside the source: it has
    // to be on the destination's volume for the promotion at the end to be a
    // link rather than a second copy.
    let mut temp = TempFileGuard::new(dst_dir.join(temp_file_name()));

    let source_digest = copy(src, temp.path()).with_context(|| {
        format!(
            "copying {} to {} via temp file {}",
            src.display(),
            dst.display(),
            temp.path().display()
        )
    })?;

    let copy_digest = crate::hasher::full_hash(temp.path()).with_context(|| {
        format!(
            "verifying the copy of {} written to {}",
            src.display(),
            temp.path().display()
        )
    })?;

    if source_digest != copy_digest {
        return Err(MoveError::Fatal(anyhow::anyhow!(
            "copy verification failed moving {} to {}: source BLAKE3 {}, copy BLAKE3 {} — \
             the copy is corrupt, so it has been discarded and the source left in place",
            src.display(),
            dst.display(),
            source_digest,
            copy_digest
        )));
    }

    debug!(
        src = %src.display(),
        dst = %dst.display(),
        digest = %source_digest,
        "copy verified by content"
    );

    // Promote the temp file into place. Same directory, therefore same volume,
    // so this is the link path — and it refuses an occupied destination for
    // the same reason the first attempt did.
    promote_into_place(temp.path(), dst)?;
    // The temp path is free again and must not be removed: on the
    // `reserve_and_rename` fallback a later move could already have claimed it.
    temp.disarm();

    // Only now delete the source.
    fs::remove_file(src).with_context(|| {
        format!(
            "removing source file {} after copying it to {}",
            src.display(),
            dst.display()
        )
    })?;

    Ok(())
}

/// Safe cross-volume move: copy → verify by content → delete source.
fn cross_volume_move(src: &Path, dst: &Path) -> Result<(), MoveError> {
    copy_verify_delete(src, dst, crate::hasher::copy_hashing)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The layout a run with no config file uses, so that every assertion below
    /// pins the *default* behaviour rather than one invented for the test.
    fn scheme() -> Layout {
        crate::settings::Settings::default()
            .layout()
            .expect("the built-in default formats must be valid")
    }

    /// A layout built from the two formats, with the default directories.
    fn layout_of(date_directory_format: &str, filename_format: &str, location: bool) -> Layout {
        crate::settings::Settings {
            date_directory_format: date_directory_format.to_string(),
            filename_format: filename_format.to_string(),
            include_location: location,
            ..Default::default()
        }
        .layout()
        .expect("the test's formats must be valid")
    }

    #[test]
    fn test_date_directory() {
        assert_eq!(
            scheme().date_directory(
                &chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
                    .unwrap()
                    .and_hms_opt(10, 30, 0)
                    .unwrap()
                    .and_utc()
            ),
            Some(PathBuf::from("2024-03-15"))
        );
    }

    /// The setting the whole of Phase 04 task 5 exists for, asserted through the
    /// function that files the photograph rather than through the format alone.
    #[test]
    fn test_a_configured_date_directory_format_nests_the_tree() {
        let nested = layout_of("%Y/%m/%d", "{date}-{time}{location}.{ext}", true);
        let (dir, filename) = build_target_path(
            &at(2024, 3, 15),
            "jpg",
            "IMG_0001",
            geo(),
            &nested,
            DatePolicy::AnyDate,
        );
        assert_eq!(dir, PathBuf::from("2024/03/15"));
        assert_eq!(filename, "2024-03-15-103000.jpg");
    }

    /// And the other half: the name, with a token the default never uses.
    #[test]
    fn test_a_configured_filename_format_is_applied() {
        let renamed = layout_of("%Y-%m-%d", "{original_stem}-{date}.{ext}", true);
        let (dir, filename) = build_target_path(
            &at(2024, 3, 15),
            "jpg",
            "IMG_0001",
            geo(),
            &renamed,
            DatePolicy::AnyDate,
        );
        assert_eq!(dir, PathBuf::from("2024-03-15"));
        assert_eq!(filename, "IMG_0001-2024-03-15.jpg");
    }

    /// `include_location = false` reaches the filename even for a file that has
    /// coordinates — the token expands to nothing rather than the lookup being
    /// spelled and then discarded.
    #[test]
    fn test_include_location_off_drops_the_location_token() {
        let mut located = at(2024, 3, 15);
        located.latitude = Some(51.5);
        located.longitude = Some(-0.12);

        let (_, with) = build_target_path(
            &located,
            "jpg",
            "IMG_0001",
            geo(),
            &scheme(),
            DatePolicy::AnyDate,
        );
        let without = layout_of("%Y-%m-%d", "{date}-{time}{location}.{ext}", false);
        let (_, plain) = build_target_path(
            &located,
            "jpg",
            "IMG_0001",
            geo(),
            &without,
            DatePolicy::AnyDate,
        );

        assert!(
            with.len() > plain.len(),
            "the located name {with} should carry a place the plain name {plain} does not"
        );
        assert_eq!(plain, "2024-03-15-103000.jpg");
    }

    /// The geocoder loads a dataset; one per test binary is enough.
    fn geo() -> &'static GeoLookup {
        static GEO: std::sync::OnceLock<GeoLookup> = std::sync::OnceLock::new();
        GEO.get_or_init(GeoLookup::new)
    }

    fn at(year: i32, month: u32, day: u32) -> FileMetadata {
        FileMetadata {
            date: Some(
                chrono::NaiveDate::from_ymd_opt(year, month, day)
                    .unwrap()
                    .and_hms_opt(10, 30, 0)
                    .unwrap()
                    .and_utc()
                    .fixed_offset(),
            ),
            timezone_source: Some(TimezoneSource::ExifOffsetTag),
            latitude: None,
            longitude: None,
            date_source: DateSource::Exif,
        }
    }

    /// A year under 1000 is still four digits wide. Without the padding it was
    /// filed under `44-03-15`, which sorts, reads and globs as nothing the tool
    /// documents.
    #[test]
    fn test_a_low_year_is_padded_to_four_digits() {
        let (dir, filename) = build_target_path(
            &at(44, 3, 15),
            "jpg",
            "IMG_0001",
            geo(),
            &scheme(),
            DatePolicy::AnyDate,
        );
        assert_eq!(dir, PathBuf::from("0044-03-15"));
        assert_eq!(filename, "0044-03-15-103000.jpg");
    }

    /// And one that is not four digits wide at all goes to `unsorted/` rather
    /// than to a directory called `-44` holding a file called `-44-…jpg`, which
    /// every command-line tool that met it would read as a flag.
    #[test]
    fn test_a_year_outside_four_digits_goes_to_unsorted() {
        let (dir, filename) = build_target_path(
            &at(-44, 3, 15),
            "jpg",
            "IMG_0001",
            geo(),
            &scheme(),
            DatePolicy::AnyDate,
        );
        assert_eq!(dir, PathBuf::from("unsorted"));
        assert_eq!(filename, "unknown.jpg");
    }

    /// `build_target_path` is public and its extension argument is arbitrary
    /// text. Today's only caller is the scanner, which admits nothing but its
    /// own known-media list — this asserts the function's own invariant rather
    /// than that caller's discipline.
    #[test]
    fn test_a_hostile_extension_cannot_add_path_separators() {
        let (dir, filename) = build_target_path(
            &at(2024, 3, 15),
            "../../etc/passwd",
            "IMG_0001",
            geo(),
            &scheme(),
            DatePolicy::AnyDate,
        );
        assert_eq!(dir, PathBuf::from("2024-03-15"));
        assert_eq!(filename, "2024-03-15-103000.______etc_passwd");
        assert!(!filename.contains('/'));
    }

    /// A planned move with the metadata fields pinned inert — nothing in the
    /// move path reads them.
    fn plan(src: &Path, dst: &Path) -> PlannedMove {
        PlannedMove {
            source: src.to_path_buf(),
            destination: dst.to_path_buf(),
            date_source: DateSource::None,
            timezone_source: None,
            has_location: false,
            known_hash: None,
        }
    }

    #[test]
    fn test_collision_candidate_zero_is_the_path_itself() {
        let path = Path::new("/photos/2024/01/15/photo.jpg");
        assert_eq!(collision_candidate(path, 0), path);
    }

    #[test]
    fn test_collision_candidate_appends_the_attempt_number() {
        let path = Path::new("/photos/photo.jpg");
        assert_eq!(
            collision_candidate(path, 1),
            PathBuf::from("/photos/photo-1.jpg")
        );
        assert_eq!(
            collision_candidate(path, 2),
            PathBuf::from("/photos/photo-2.jpg")
        );
    }

    #[test]
    fn test_collision_candidate_without_extension() {
        let path = Path::new("/photos/photo");
        assert_eq!(
            collision_candidate(path, 3),
            PathBuf::from("/photos/photo-3")
        );
    }

    /// The candidate function must not consult the filesystem: an occupied
    /// path still yields itself at attempt 0. Anything else would be the old
    /// `exists()`-then-rename shape wearing a new name.
    #[test]
    fn test_collision_candidate_ignores_what_is_on_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("photo.jpg");
        fs::write(&path, b"occupied").unwrap();
        assert_eq!(collision_candidate(&path, 0), path);
    }

    #[test]
    fn test_execute_move_same_volume() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst_dir = tmp.path().join("2024-01-15");
        let dst = dst_dir.join("2024-01-15-103000.jpg");
        fs::write(&src, b"image data").unwrap();

        let outcome = execute_move(&plan(&src, &dst)).unwrap();

        assert_eq!(
            outcome.kind,
            MoveKind::Renamed,
            "a move within one volume links"
        );
        assert_eq!(
            outcome.destination, dst,
            "an uncontested move lands on the planned path"
        );
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"image data");
    }

    /// The no-clobber contract at its own level: an occupied destination is a
    /// refusal, not an overwrite, and both files are untouched afterwards.
    #[test]
    fn test_move_no_clobber_refuses_an_occupied_destination() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("taken.jpg");
        fs::write(&src, b"MOVED").unwrap();
        fs::write(&dst, b"PRE-EXISTING").unwrap();

        let err = move_no_clobber(&src, &dst).expect_err("an occupied destination must refuse");

        assert!(
            matches!(err, MoveError::DestinationExists(ref p) if p == &dst),
            "expected DestinationExists({}), got {err:?}",
            dst.display()
        );
        assert_eq!(fs::read(&dst).unwrap(), b"PRE-EXISTING");
        assert_eq!(fs::read(&src).unwrap(), b"MOVED");
    }

    /// `Path::exists()` follows symlinks, so a dangling link reads as "nothing
    /// here" while the directory entry is very much there. `link(2)` asks the
    /// right question and answers `EEXIST`.
    #[cfg(unix)]
    #[test]
    fn test_move_no_clobber_refuses_a_dangling_symlink() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("dangling.jpg");
        fs::write(&src, b"MOVED").unwrap();
        std::os::unix::fs::symlink("./nothing-here.jpg", &dst).unwrap();

        assert!(
            !dst.exists(),
            "the fixture is only meaningful while dangling"
        );

        let err = move_no_clobber(&src, &dst).expect_err("a dangling symlink is an existing entry");

        assert!(
            matches!(err, MoveError::DestinationExists(_)),
            "got {err:?}"
        );
        assert!(fs::symlink_metadata(&dst).unwrap().is_symlink());
    }

    /// `execute_move` walks the candidates until one is free rather than
    /// trusting a single pre-flight check.
    #[test]
    fn test_execute_move_retries_past_taken_candidates() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst = tmp.path().join("out/photo.jpg");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&src, b"MOVED").unwrap();
        fs::write(&dst, b"TAKEN-0").unwrap();
        fs::write(tmp.path().join("out/photo-1.jpg"), b"TAKEN-1").unwrap();

        let outcome = execute_move(&plan(&src, &dst)).unwrap();

        assert_eq!(
            outcome.destination,
            tmp.path().join("out/photo-2.jpg"),
            "the outcome must name the path the file actually reached, not the \
             one that was planned"
        );
        assert_eq!(fs::read(&dst).unwrap(), b"TAKEN-0");
        assert_eq!(
            fs::read(tmp.path().join("out/photo-1.jpg")).unwrap(),
            b"TAKEN-1"
        );
        assert_eq!(
            fs::read(tmp.path().join("out/photo-2.jpg")).unwrap(),
            b"MOVED",
            "the move should have landed on the first free candidate"
        );
        assert!(!src.exists());
    }

    /// When the link succeeds but the source cannot be unlinked, the new link
    /// is undone — the run must not end with two names for one file, which the
    /// dedup pass would later report as a duplicate of itself.
    ///
    /// Skips itself with a printed reason where permission bits do not deny
    /// writes (running as root, as some CI containers do).
    #[cfg(unix)]
    #[test]
    fn test_a_failed_source_unlink_undoes_the_link() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("input");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("source.jpg");
        let dst = tmp.path().join("photo.jpg");
        fs::write(&src, b"MOVED").unwrap();

        let original = fs::metadata(&src_dir).unwrap().permissions().mode();
        fs::set_permissions(&src_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = move_no_clobber(&src, &dst);
        let unlink_denied = fs::remove_file(src_dir.join(".probe")).is_err()
            && fs::write(src_dir.join(".probe"), b"p").is_err();

        // Restore before asserting, or `TempDir` cannot clean up after a panic.
        fs::set_permissions(&src_dir, fs::Permissions::from_mode(original)).unwrap();

        if !unlink_denied {
            eprintln!(
                "SKIPPED test_a_failed_source_unlink_undoes_the_link: writes to a 0o555 \
                 directory succeeded, so this process ignores permission bits (running as root?)"
            );
            return;
        }

        assert!(
            outcome.is_err(),
            "a move that could not drop the source link must not report success"
        );
        assert!(
            !dst.exists(),
            "the link at {} should have been undone",
            dst.display()
        );
        assert_eq!(fs::read(&src).unwrap(), b"MOVED");
    }

    /// The copy path still moves the bytes and still drops the source last.
    ///
    /// Driven directly rather than through a second mounted volume, which no
    /// test runner can be assumed to have. What it covers is the sequencing
    /// this task rearranged — copy, promote, *then* remove the source — not the
    /// content verification, which task 4 of the phase replaces.
    #[test]
    fn test_cross_volume_move_copies_then_removes_the_source() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input/holiday.jpg");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        let dst = tmp.path().join("output/photo.jpg");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&src, b"COPY ME").unwrap();

        cross_volume_move(&src, &dst).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"COPY ME");
        assert!(
            !src.exists(),
            "the source must be gone once the copy landed"
        );
    }

    /// An occupied destination stops the copy path too, and takes its temp
    /// file with it — the caller retries under the next candidate name, and a
    /// run must not leave `.tmp-*` litter behind in the output tree.
    #[test]
    fn test_cross_volume_move_refuses_an_occupied_destination() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input/holiday.jpg");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        let dst_dir = tmp.path().join("output");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("photo.jpg");
        fs::write(&src, b"COPY ME").unwrap();
        fs::write(&dst, b"PRE-EXISTING").unwrap();

        let err = cross_volume_move(&src, &dst).expect_err("an occupied destination must refuse");

        assert!(
            matches!(err, MoveError::DestinationExists(ref p) if p == &dst),
            "got {err:?}"
        );
        assert_eq!(fs::read(&dst).unwrap(), b"PRE-EXISTING");
        assert_eq!(fs::read(&src).unwrap(), b"COPY ME");

        let leftovers: Vec<String> = fs::read_dir(&dst_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temp file should have been cleaned up; found {leftovers:?}"
        );
    }

    /// The classification table, stated as errnos.
    ///
    /// This is the whole of the defect in one assertion: `EACCES` and `ENOENT`
    /// are `Fatal`, not "must be a different volume, copy it". `EPERM` sits
    /// next to `EACCES` in the same `ErrorKind` and goes the other way, which
    /// is why the raw errno is consulted at all.
    #[cfg(unix)]
    #[test]
    fn test_classify_link_failure_routes_each_errno() {
        const EEXIST: i32 = 17;
        const ENOENT: i32 = 2;
        const EACCES: i32 = 13;
        const EXDEV: i32 = 18;
        const EROFS: i32 = 30;
        const ENOSPC: i32 = 28;

        let cases: &[(i32, LinkFailure, &str)] = &[
            (
                EEXIST,
                LinkFailure::DestinationTaken,
                "occupied destination",
            ),
            (EXDEV, LinkFailure::DifferentVolume, "different volumes"),
            (
                errno::EPERM,
                LinkFailure::LinksUnsupported,
                "filesystem without hard links",
            ),
            (EACCES, LinkFailure::Fatal, "permission denied"),
            (ENOENT, LinkFailure::Fatal, "missing source"),
            (EROFS, LinkFailure::Fatal, "read-only filesystem"),
            (ENOSPC, LinkFailure::Fatal, "full disk"),
        ];

        for &(raw, expected, what) in cases {
            let err = io::Error::from_raw_os_error(raw);
            assert_eq!(
                classify_link_failure(&err),
                expected,
                "errno {raw} ({what}, kind {:?}) must classify as {expected:?}",
                err.kind()
            );
        }

        for &raw in errno::NOT_SUPPORTED {
            let err = io::Error::from_raw_os_error(raw);
            assert_eq!(
                classify_link_failure(&err),
                LinkFailure::LinksUnsupported,
                "errno {raw} (link unsupported) must classify as LinksUnsupported"
            );
        }
    }

    /// The two kinds that carry no errno — as they arrive from a non-unix
    /// target, or from any code constructing an error by kind.
    #[test]
    fn test_classify_link_failure_reads_the_kind_without_an_errno() {
        assert_eq!(
            classify_link_failure(&io::Error::from(io::ErrorKind::AlreadyExists)),
            LinkFailure::DestinationTaken
        );
        assert_eq!(
            classify_link_failure(&io::Error::from(io::ErrorKind::CrossesDevices)),
            LinkFailure::DifferentVolume
        );
        assert_eq!(
            classify_link_failure(&io::Error::from(io::ErrorKind::Unsupported)),
            LinkFailure::LinksUnsupported
        );
        assert_eq!(
            classify_link_failure(&io::Error::from(io::ErrorKind::PermissionDenied)),
            LinkFailure::Fatal,
            "an errno-less permission denial must still be fatal"
        );
    }

    /// The link-less promotion fallback: it moves the file, and it refuses an
    /// occupied name rather than overwriting it.
    #[test]
    fn test_reserve_and_rename_moves_into_a_free_name() {
        let tmp = TempDir::new().unwrap();
        let temp_file = tmp.path().join(".tmp-1234");
        let dst = tmp.path().join("photo.jpg");
        fs::write(&temp_file, b"COPIED").unwrap();

        reserve_and_rename(&temp_file, &dst).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"COPIED");
        assert!(!temp_file.exists(), "the temp file should be gone");
    }

    #[test]
    fn test_reserve_and_rename_refuses_an_occupied_destination() {
        let tmp = TempDir::new().unwrap();
        let temp_file = tmp.path().join(".tmp-1234");
        let dst = tmp.path().join("photo.jpg");
        fs::write(&temp_file, b"COPIED").unwrap();
        fs::write(&dst, b"PRE-EXISTING").unwrap();

        let err = reserve_and_rename(&temp_file, &dst).expect_err("an occupied name must refuse");

        assert!(
            matches!(err, MoveError::DestinationExists(ref p) if p == &dst),
            "got {err:?}"
        );
        assert_eq!(fs::read(&dst).unwrap(), b"PRE-EXISTING");
        assert_eq!(fs::read(&temp_file).unwrap(), b"COPIED");
    }

    /// `O_CREAT | O_EXCL` fails `EEXIST` on a symlink whether or not its
    /// target exists — the same question `link(2)` answers, and the one
    /// `Path::exists()` gets wrong.
    #[cfg(unix)]
    #[test]
    fn test_reserve_and_rename_refuses_a_dangling_symlink() {
        let tmp = TempDir::new().unwrap();
        let temp_file = tmp.path().join(".tmp-1234");
        let dst = tmp.path().join("photo.jpg");
        fs::write(&temp_file, b"COPIED").unwrap();
        std::os::unix::fs::symlink("./nothing-here.jpg", &dst).unwrap();

        let err =
            reserve_and_rename(&temp_file, &dst).expect_err("a dangling symlink occupies the name");

        assert!(
            matches!(err, MoveError::DestinationExists(_)),
            "got {err:?}"
        );
        assert!(fs::symlink_metadata(&dst).unwrap().is_symlink());
    }

    /// A destination directory that cannot be written must fail as the
    /// permission problem it is — naming both paths — without a copy ever
    /// being attempted.
    ///
    /// The copy path is not a fallback for "the move failed"; it is the answer
    /// to exactly one question, "are these two paths on different volumes". A
    /// permission denial answered with a copy attempt wastes a full read and
    /// write of the file and then reports a temp file the operator never asked
    /// about, which is the wrong error about the wrong thing.
    ///
    /// Skips itself with a printed reason where permission bits do not deny
    /// writes (running as root, as some CI containers do).
    #[cfg(unix)]
    #[test]
    fn test_a_read_only_destination_fails_without_attempting_a_copy() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("holiday.jpg");
        let dst_dir = tmp.path().join("output");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("photo.jpg");
        fs::write(&src, b"MOVED").unwrap();

        let original = fs::metadata(&dst_dir).unwrap().permissions().mode();
        fs::set_permissions(&dst_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = execute_move(&plan(&src, &dst));
        let writes_denied = fs::write(dst_dir.join(".probe"), b"p").is_err();
        let leftovers: Vec<String> = fs::read_dir(&dst_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        // Restore before asserting, or `TempDir` cannot clean up after a panic.
        fs::set_permissions(&dst_dir, fs::Permissions::from_mode(original)).unwrap();

        if !writes_denied {
            eprintln!(
                "SKIPPED test_a_read_only_destination_fails_without_attempting_a_copy: writes to \
                 a 0o555 directory succeeded, so this process ignores permission bits (running as \
                 root?)"
            );
            return;
        }

        let err = outcome.expect_err("moving into an unwritable directory must not report success");
        let chain = format!("{err:#}");

        assert!(
            chain.contains(&src.display().to_string())
                && chain.contains(&dst.display().to_string()),
            "the error must name both source and destination; got: {chain}"
        );
        assert!(
            chain.contains("Permission denied"),
            "a permission denial must surface as one; got: {chain}"
        );
        assert!(
            !chain.contains("temp"),
            "a permission denial must not be answered with a copy attempt; got: {chain}"
        );
        assert!(
            leftovers.is_empty(),
            "no temp file should have been written into the destination; found {leftovers:?}"
        );
        assert_eq!(fs::read(&src).unwrap(), b"MOVED", "the source must survive");
    }

    /// A source that has gone away between planning and execution must fail as
    /// a missing-source error naming both paths, not as a failed copy.
    #[test]
    fn test_a_missing_source_fails_naming_both_paths() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("gone.jpg");
        let dst = tmp.path().join("output/photo.jpg");

        let err = execute_move(&plan(&src, &dst))
            .expect_err("moving a source that does not exist must not report success");
        let chain = format!("{err:#}");

        assert!(
            chain.contains(&src.display().to_string())
                && chain.contains(&dst.display().to_string()),
            "the error must name both source and destination; got: {chain}"
        );
        assert!(
            !chain.contains("temp"),
            "a missing source must not be answered with a copy attempt; got: {chain}"
        );
        assert!(
            !dst.exists(),
            "nothing should have been created at {}",
            dst.display()
        );
    }

    /// A copy that fails halfway leaves nothing behind and takes nothing away.
    ///
    /// This is the path the old code got wrong by omission: `fs::copy` failing
    /// part-way through still leaves a partial temp file, and the early return
    /// above the size check had no cleanup on it.
    #[test]
    fn test_a_failed_copy_leaves_no_temp_file_and_no_damage() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input/holiday.jpg");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        let dst_dir = tmp.path().join("output");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("photo.jpg");
        fs::write(&src, b"KEEP ME").unwrap();

        // Writes a partial temp file, then gives up — a copy interrupted by a
        // full disk or an unplugged drive.
        let failing_copy = |_from: &Path, temp: &Path| -> Result<String> {
            fs::write(temp, b"KEEP")?;
            bail!("the drive went away")
        };

        let err = copy_verify_delete(&src, &dst, failing_copy)
            .expect_err("a failed copy must not report success");

        assert!(
            format!("{err}").contains("the drive went away"),
            "the underlying cause must survive the context; got: {err}"
        );
        assert_eq!(fs::read(&src).unwrap(), b"KEEP ME");
        assert!(!dst.exists());
        assert_eq!(
            fs::read_dir(&dst_dir).unwrap().count(),
            0,
            "the partial temp file should have been cleaned up"
        );
    }

    /// The size check this replaced would have caught a *short* copy. The
    /// content check has to catch it too — a fix that trades one class of
    /// failure for another is not a fix.
    #[test]
    fn test_a_truncated_copy_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input/holiday.jpg");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        let dst_dir = tmp.path().join("output");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("photo.jpg");
        fs::write(&src, b"THE WHOLE PHOTOGRAPH").unwrap();

        let truncating_copy = |from: &Path, temp: &Path| -> Result<String> {
            let read = fs::read(from)?;
            fs::write(temp, &read[..4])?;
            Ok(blake3::hash(&read).to_hex().to_string())
        };

        let err = copy_verify_delete(&src, &dst, truncating_copy)
            .expect_err("a short copy must not verify");

        assert!(
            format!("{err}").contains("copy verification failed"),
            "got: {err}"
        );
        assert_eq!(fs::read(&src).unwrap(), b"THE WHOLE PHOTOGRAPH");
        assert_eq!(
            fs::read_dir(&dst_dir).unwrap().count(),
            0,
            "the rejected copy should have been cleaned up"
        );
    }

    /// Two moves in the same millisecond must not be handed the same temp path
    /// — one would overwrite the other's copy, or refuse the move outright.
    #[test]
    fn test_temp_file_names_do_not_repeat() {
        let names: std::collections::HashSet<String> = (0..100).map(|_| temp_file_name()).collect();
        assert_eq!(names.len(), 100, "temp names collided within one process");
    }

    #[test]
    fn test_temp_file_guard_removes_the_file_unless_disarmed() {
        let tmp = TempDir::new().unwrap();
        let doomed = tmp.path().join("doomed");
        let spared = tmp.path().join("spared");
        fs::write(&doomed, b"x").unwrap();
        fs::write(&spared, b"x").unwrap();

        drop(TempFileGuard::new(doomed.clone()));
        let mut guard = TempFileGuard::new(spared.clone());
        guard.disarm();
        drop(guard);

        assert!(!doomed.exists(), "an armed guard must remove its file");
        assert!(spared.exists(), "a disarmed guard must leave its file");
    }

    // -----------------------------------------------------------------------
    // Duplicate manifests
    // -----------------------------------------------------------------------

    /// A group of byte-identical files, shaped as the dedup cascade hands it
    /// over: `files[0]` is the retained original, the rest are moved aside.
    fn duplicate_group(files: &[PathBuf], body: &[u8]) -> DuplicateGroup {
        DuplicateGroup {
            hash: blake3::hash(body).to_hex().to_string(),
            size: body.len() as u64,
            files: files.to_vec(),
        }
    }

    /// The whole intended source list is on disk before a single file moves.
    ///
    /// This is the crash-safety property stated at the seam rather than
    /// inferred from the caller: the manifest is complete the moment it is
    /// created, so a run killed between the first and second move still says
    /// where every file in the group came from. The old code built the text in
    /// memory and wrote it *after* the loop, which meant an interruption left
    /// duplicates relocated and no record of their origins at all.
    #[test]
    fn test_a_manifest_lists_every_intended_source_before_any_move_happens() {
        let tmp = TempDir::new().unwrap();
        let group_dir = tmp.path().join("duplicates/000");
        fs::create_dir_all(&group_dir).unwrap();
        let manifest_path = group_dir.join("manifest.txt");

        let group = duplicate_group(
            &[
                PathBuf::from("/input/kept.jpg"),
                PathBuf::from("/input/one/photo.jpg"),
                PathBuf::from("/input/two/photo.jpg"),
            ],
            b"BODY",
        );

        // Deliberately still open — nothing is recorded, nothing is dropped.
        let _manifest = GroupManifest::create(&manifest_path, 0, &group).unwrap();

        let text = fs::read_to_string(&manifest_path).unwrap();
        assert!(
            text.contains(&format!("# BLAKE3 hash: {}", group.hash)),
            "got: {text}"
        );
        assert!(text.contains("# File size: 4 bytes"), "got: {text}");
        assert!(
            text.contains("# Original kept at: /input/kept.jpg"),
            "got: {text}"
        );
        for source in &group.files[1..] {
            assert!(
                text.lines().any(|l| l == source.display().to_string()),
                "the intended source {} is not listed; got: {text}",
                source.display()
            );
        }
    }

    /// Each outcome line reaches the disk as it is recorded, not when the
    /// writer is dropped — a buffered manifest is no manifest at all if the
    /// process dies mid-group.
    #[test]
    fn test_manifest_outcomes_are_readable_as_soon_as_they_are_recorded() {
        let tmp = TempDir::new().unwrap();
        let manifest_path = tmp.path().join("manifest.txt");
        let group = duplicate_group(
            &[PathBuf::from("/input/kept.jpg"), PathBuf::from("/in/a.jpg")],
            b"BODY",
        );

        let mut manifest = GroupManifest::create(&manifest_path, 0, &group).unwrap();
        manifest
            .record_move(
                Path::new("/in/a.jpg"),
                Path::new("/out/duplicates/000/a.jpg"),
            )
            .unwrap();

        let text = fs::read_to_string(&manifest_path).unwrap();
        assert!(
            text.contains("/in/a.jpg -> /out/duplicates/000/a.jpg"),
            "the outcome should be on disk before the writer is dropped; got: {text}"
        );

        manifest
            .record_failure(Path::new("/in/b.jpg"), "the drive went away")
            .unwrap();
        let text = fs::read_to_string(&manifest_path).unwrap();
        assert!(
            text.contains("/in/b.jpg") && text.contains("the drive went away"),
            "got: {text}"
        );
    }

    /// A failed move inside a group must be recorded, not dropped — and the
    /// files that never moved must still be named, because the manifest is the
    /// only record of where they were meant to go.
    #[test]
    fn test_move_duplicates_records_the_intended_sources_even_when_a_move_fails() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let output = tmp.path().join("output");

        let kept = input.join("kept.jpg");
        let doomed = input.join("gone.jpg");
        let survivor = input.join("survivor.jpg");
        for path in [&kept, &doomed, &survivor] {
            fs::write(path, b"BODY").unwrap();
        }

        // The scan saw three copies; one is deleted between planning and
        // execution, which is the everyday shape of a mid-run failure.
        let group = duplicate_group(&[kept, doomed.clone(), survivor.clone()], b"BODY");
        fs::remove_file(&doomed).unwrap();

        let (moved, errors) = move_duplicates(
            &[group],
            &output,
            Path::new("duplicates"),
            &mut MoveRecorder::disabled(),
        )
        .unwrap();

        assert_eq!((moved, errors), (1, 1), "one move must fail, one succeed");

        let text = fs::read_to_string(output.join("duplicates/000/manifest.txt")).unwrap();
        for source in [&doomed, &survivor] {
            assert!(
                text.contains(&source.display().to_string()),
                "the manifest must name the intended source {}; got: {text}",
                source.display()
            );
        }
        assert!(
            text.lines()
                .any(|l| l.contains("FAILED") && l.contains(&doomed.display().to_string())),
            "the failed move must be recorded as a failure; got: {text}"
        );
        assert!(
            fs::read(output.join("duplicates/000/survivor.jpg")).unwrap() == b"BODY",
            "the surviving duplicate should still have been moved"
        );
    }

    /// The outcome lines name where each file actually landed, suffix and all
    /// — a record that says only "moved" cannot be used to put anything back.
    #[test]
    fn test_move_duplicates_records_the_destination_each_duplicate_reached() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(input.join("one")).unwrap();
        fs::create_dir_all(input.join("two")).unwrap();
        let output = tmp.path().join("output");

        let kept = input.join("kept.jpg");
        let first = input.join("one/photo.jpg");
        let second = input.join("two/photo.jpg");
        for path in [&kept, &first, &second] {
            fs::write(path, b"BODY").unwrap();
        }

        let group = duplicate_group(&[kept, first.clone(), second.clone()], b"BODY");
        let (moved, errors) = move_duplicates(
            &[group],
            &output,
            Path::new("duplicates"),
            &mut MoveRecorder::disabled(),
        )
        .unwrap();
        assert_eq!((moved, errors), (2, 0));

        let group_dir = output.join("duplicates/000");
        let text = fs::read_to_string(group_dir.join("manifest.txt")).unwrap();

        // Both flattened into one directory, so the second took a suffix. The
        // manifest has to say so, or the two records are indistinguishable.
        assert!(
            text.contains(&format!(
                "{} -> {}",
                first.display(),
                group_dir.join("photo.jpg").display()
            )),
            "got: {text}"
        );
        assert!(
            text.contains(&format!(
                "{} -> {}",
                second.display(),
                group_dir.join("photo-1.jpg").display()
            )),
            "got: {text}"
        );
    }

    // -----------------------------------------------------------------
    // Chunked processing and the early stop
    // -----------------------------------------------------------------

    /// A controller that answers a fixed script and records what it was asked.
    struct ScriptedController {
        /// `should_continue` returns `answers[n]` for the nth question, and
        /// `true` once the script runs out.
        answers: Vec<bool>,
        asked: Vec<(usize, usize)>,
        chunks_started: Vec<(usize, usize)>,
        files_finished: usize,
    }

    impl ScriptedController {
        fn new(answers: &[bool]) -> Self {
            Self {
                answers: answers.to_vec(),
                asked: Vec::new(),
                chunks_started: Vec::new(),
                files_finished: 0,
            }
        }
    }

    impl ChunkController for ScriptedController {
        fn chunk_started(&mut self, chunk_number: usize, chunks: usize) {
            self.chunks_started.push((chunk_number, chunks));
        }

        fn file_finished(&mut self) {
            self.files_finished += 1;
        }

        fn should_continue(&mut self, chunk_number: usize, remaining: usize) -> bool {
            self.asked.push((chunk_number, remaining));
            self.answers
                .get(self.asked.len() - 1)
                .copied()
                .unwrap_or(true)
        }
    }

    /// Four sources in `input/`, planned into `output/`, in a stable order.
    fn four_planned_moves(tmp: &Path) -> Vec<PlannedMove> {
        let input = tmp.join("input");
        let output = tmp.join("output");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();

        (0..4)
            .map(|i| {
                let src = input.join(format!("photo-{i}.jpg"));
                fs::write(&src, format!("BODY {i}").as_bytes()).unwrap();
                plan(&src, &output.join(format!("photo-{i}.jpg")))
            })
            .collect()
    }

    /// Declining at the first prompt stops the run and reports what was left,
    /// rather than taking the process down with it.
    #[test]
    fn test_declining_at_a_chunk_boundary_stops_and_reports_the_remainder() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        let mut controller = ScriptedController::new(&[false]);

        let run = process_moves(&planned, 2, &mut controller, &mut MoveRecorder::disabled());

        assert_eq!(
            (run.moved, run.errors, run.unprocessed, run.stopped_early),
            (2, 0, 2, true),
            "the first chunk should have moved and the second been left alone"
        );

        // The count is only as good as the files behind it.
        for planned in &planned[..2] {
            assert!(
                !planned.source.exists(),
                "the first chunk should have moved"
            );
            assert!(planned.destination.exists());
        }
        for planned in &planned[2..] {
            assert!(
                planned.source.exists(),
                "a file the operator stopped before must still be where they left it"
            );
            assert!(!planned.destination.exists());
        }

        assert_eq!(controller.asked, vec![(1, 2)], "asked once, after chunk 1");
        assert_eq!(controller.files_finished, 2);
    }

    /// Continuing through every chunk processes everything and asks nothing
    /// after the last one — there is nothing left to consent to.
    #[test]
    fn test_continuing_processes_every_chunk_and_never_prompts_at_the_end() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        let mut controller = ScriptedController::new(&[true, true, true]);

        let run = process_moves(&planned, 2, &mut controller, &mut MoveRecorder::disabled());

        assert_eq!(
            (run.moved, run.errors, run.unprocessed, run.stopped_early),
            (4, 0, 0, false)
        );
        assert_eq!(
            controller.asked,
            vec![(1, 2)],
            "the final chunk leaves nothing remaining, so it must not prompt"
        );
        assert_eq!(controller.chunks_started, vec![(1, 2), (2, 2)]);
        assert_eq!(controller.files_finished, 4);
    }

    /// A single failed move is counted and the run carries on — the early stop
    /// is the operator's decision, not a failure mode.
    #[test]
    fn test_a_failed_move_is_counted_without_stopping_the_run() {
        let tmp = TempDir::new().unwrap();
        let mut planned = four_planned_moves(tmp.path());
        fs::remove_file(&planned[1].source).unwrap();
        let mut controller = ScriptedController::new(&[true, true, true]);

        let run = process_moves(&planned, 2, &mut controller, &mut MoveRecorder::disabled());

        assert_eq!(
            (run.moved, run.errors, run.unprocessed, run.stopped_early),
            (3, 1, 0, false)
        );
        planned.remove(1);
        for planned in &planned {
            assert!(planned.destination.exists());
        }
    }

    /// `--chunk-size 0` is reachable from the command line, and `chunks(0)`
    /// panics. A crate that bans `unwrap` in the move path must not take the
    /// process down over an argument value either.
    #[test]
    fn test_a_zero_chunk_size_processes_everything_instead_of_panicking() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        let mut controller = ScriptedController::new(&[]);

        let run = process_moves(&planned, 0, &mut controller, &mut MoveRecorder::disabled());

        assert_eq!((run.moved, run.errors, run.unprocessed), (4, 0, 0));
        assert!(controller.asked.is_empty(), "one chunk asks nothing");
    }

    // -----------------------------------------------------------------
    // Journalling the move passes
    // -----------------------------------------------------------------

    /// A journal under `<dir>/.mmm/journal`, as a real run would have.
    fn open_journal(dir: &Path) -> Journal {
        Journal::create(
            &dir.join(".mmm/journal"),
            &crate::journal::RunHeader::new("20240315-103000-abc123", dir, vec!["mmm".to_string()]),
        )
        .unwrap()
    }

    /// Every entry in `journal`, in the order it was written.
    fn entries_of(journal: &Journal) -> Vec<JournalEntry> {
        Journal::read(journal.path()).unwrap().1
    }

    /// The organise pass writes an intent for each file and an outcome after
    /// it, in that order, naming both ends of the move.
    #[test]
    fn test_the_organise_pass_journals_an_intent_then_an_outcome_per_file() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        let mut journal = open_journal(tmp.path());
        let mut controller = ScriptedController::new(&[]);

        let run = process_moves(
            &planned,
            0,
            &mut controller,
            &mut MoveRecorder::new(Some(&mut journal)),
        );
        assert_eq!((run.moved, run.errors), (4, 0));

        let entries = entries_of(&journal);
        assert_eq!(
            entries.len(),
            8,
            "four intents and four outcomes: {entries:?}"
        );

        for (i, planned) in planned.iter().enumerate() {
            let JournalEntry::MoveIntent {
                seq,
                source,
                destination,
                source_size,
                kind,
                ..
            } = &entries[i * 2]
            else {
                panic!(
                    "entry {} should be the intent; got {:?}",
                    i * 2,
                    entries[i * 2]
                );
            };
            assert_eq!(source, &planned.source);
            assert_eq!(destination, &planned.destination);
            assert_eq!(*kind, IntentKind::Organise);
            assert_eq!(
                *source_size,
                fs::metadata(&planned.destination).unwrap().len(),
                "the recorded size must be the size of the file that moved"
            );

            let JournalEntry::MoveCommitted {
                seq: committed_seq,
                final_destination,
                ..
            } = &entries[i * 2 + 1]
            else {
                panic!(
                    "entry {} should be the outcome; got {:?}",
                    i * 2 + 1,
                    entries[i * 2 + 1]
                );
            };
            assert_eq!(
                committed_seq, seq,
                "the outcome must be paired to its intent by sequence number"
            );
            assert_eq!(final_destination, &planned.destination);
        }
    }

    /// The recorded destination is the one the file reached, suffix and all.
    /// A record naming the *planned* path cannot be used to find the file.
    #[test]
    fn test_the_journal_records_the_destination_the_file_actually_reached() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        fs::write(&planned[0].destination, b"TAKEN").unwrap();
        let mut journal = open_journal(tmp.path());
        let mut controller = ScriptedController::new(&[]);

        process_moves(
            &planned[..1],
            0,
            &mut controller,
            &mut MoveRecorder::new(Some(&mut journal)),
        );

        let expected = collision_candidate(&planned[0].destination, 1);
        let entries = entries_of(&journal);
        assert!(
            matches!(
                &entries[1],
                JournalEntry::MoveCommitted { final_destination, .. } if final_destination == &expected
            ),
            "expected a commit naming {}; got {:?}",
            expected.display(),
            entries[1]
        );
    }

    /// A move that failed is recorded as failed. The distinction matters to
    /// undo: an intent with a `MoveFailed` means the source never left, while
    /// an intent with nothing after it means nobody knows.
    #[test]
    fn test_a_failed_move_is_journalled_as_failed() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        fs::remove_file(&planned[0].source).unwrap();
        let mut journal = open_journal(tmp.path());
        let mut controller = ScriptedController::new(&[]);

        let run = process_moves(
            &planned[..1],
            0,
            &mut controller,
            &mut MoveRecorder::new(Some(&mut journal)),
        );
        assert_eq!((run.moved, run.errors), (0, 1));

        let entries = entries_of(&journal);
        assert!(matches!(
            entries[0],
            JournalEntry::MoveIntent { seq: 0, .. }
        ));
        assert!(
            matches!(&entries[1], JournalEntry::MoveFailed { seq: 0, reason } if !reason.is_empty()),
            "got {:?}",
            entries[1]
        );
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, JournalEntry::MoveCommitted { .. })),
            "a move that did not happen must not be recorded as committed"
        );
    }

    /// The ordering the whole module exists for, asserted where it is
    /// observable: a recorder that cannot write refuses *before* the move, so
    /// the file is still where it was. Were the move attempted first, the
    /// source would be gone and the journal would say nothing about it.
    #[test]
    fn test_an_unwritable_journal_stops_the_run_before_anything_moves() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        let mut controller = ScriptedController::new(&[]);

        let run = process_moves(&planned, 2, &mut controller, &mut MoveRecorder::failing());

        assert!(run.journal_failed, "the run must report why it stopped");
        assert!(
            !run.stopped_early,
            "a journal failure is not the operator's decision and must not be reported as one"
        );
        assert_eq!(
            (run.moved, run.errors, run.unprocessed),
            (0, 0, 4),
            "nothing moved, nothing failed, and every file is accounted for"
        );
        for planned in &planned {
            assert!(
                planned.source.exists(),
                "{} moved despite the journal refusing the intent",
                planned.source.display()
            );
            assert!(!planned.destination.exists());
        }
        assert_eq!(
            controller.asked,
            Vec::new(),
            "a halted run must not ask the operator whether to continue"
        );
    }

    /// Duplicates are journalled through the same mechanism, carrying the group
    /// they landed in and the digest the dedup cascade already computed.
    #[test]
    fn test_duplicates_are_journalled_with_their_group_and_digest() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let output = tmp.path().join("output");

        let kept = input.join("kept.jpg");
        let copy = input.join("copy.jpg");
        for path in [&kept, &copy] {
            fs::write(path, b"BODY").unwrap();
        }
        let group = duplicate_group(&[kept, copy.clone()], b"BODY");
        let digest = group.hash.clone();

        let mut journal = open_journal(tmp.path());
        let (moved, errors) = move_duplicates(
            &[group],
            &output,
            Path::new("duplicates"),
            &mut MoveRecorder::new(Some(&mut journal)),
        )
        .unwrap();
        assert_eq!((moved, errors), (1, 0));

        let entries = entries_of(&journal);
        assert!(
            matches!(
                &entries[0],
                JournalEntry::MoveIntent { source, kind, source_hash, .. }
                    if source == &copy
                        && *kind == IntentKind::Duplicate
                        && source_hash.as_deref() == Some(digest.as_str())
            ),
            "got {:?}",
            entries[0]
        );
        assert!(
            matches!(
                &entries[1],
                JournalEntry::DuplicateMoved { group: 0, source, destination, .. }
                    if source == &copy && destination == &output.join("duplicates/000/copy.jpg")
            ),
            "got {:?}",
            entries[1]
        );
    }

    /// One counter across both passes. Duplicates move first and the organise
    /// pass follows; if each kept its own sequence, undo could not tell two
    /// records apart.
    #[test]
    fn test_both_passes_draw_from_one_sequence_counter() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let kept = input.join("kept.jpg");
        let copy = input.join("copy.jpg");
        for path in [&kept, &copy] {
            fs::write(path, b"BODY").unwrap();
        }
        let group = duplicate_group(&[kept, copy], b"BODY");

        let mut journal = open_journal(tmp.path());
        move_duplicates(
            &[group],
            &tmp.path().join("output"),
            Path::new("duplicates"),
            &mut MoveRecorder::new(Some(&mut journal)),
        )
        .unwrap();

        let planned = four_planned_moves(tmp.path());
        let mut controller = ScriptedController::new(&[]);
        process_moves(
            &planned,
            0,
            &mut controller,
            &mut MoveRecorder::new(Some(&mut journal)),
        );

        let seqs: Vec<u64> = entries_of(&journal)
            .iter()
            .filter_map(|e| match e {
                JournalEntry::MoveIntent { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(
            seqs,
            vec![0, 1, 2, 3, 4],
            "the duplicate pass takes seq 0 and the organise pass carries on from there"
        );
    }

    /// `--no-journal`, and every test above that is not about journalling: the
    /// moves still happen and nothing is written.
    #[test]
    fn test_a_disabled_recorder_moves_files_and_records_nothing() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());
        let mut controller = ScriptedController::new(&[]);

        let run = process_moves(&planned, 0, &mut controller, &mut MoveRecorder::disabled());

        assert_eq!((run.moved, run.errors, run.journal_failed), (4, 0, false));
        assert!(
            !tmp.path().join(".mmm").exists(),
            "a disabled recorder must not create the metadata directory"
        );
    }

    #[test]
    fn test_build_target_path_no_date() {
        let meta = FileMetadata {
            date: None,
            timezone_source: None,
            latitude: None,
            longitude: None,
            date_source: DateSource::None,
        };
        let (dir, name) = build_target_path(
            &meta,
            "jpg",
            "IMG_0001",
            geo(),
            &scheme(),
            DatePolicy::AnyDate,
        );
        assert_eq!(dir, PathBuf::from("unsorted"));
        assert_eq!(name, "unknown.jpg");
    }
}
