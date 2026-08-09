use std::collections::HashMap;
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
use crate::sidecar::{Sidecar, SidecarIndex};
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
    /// The companions that travel with this file — see [`crate::sidecar`].
    ///
    /// Carried on the plan rather than moved by a pass of their own, because a
    /// sidecar's destination is not knowable until its parent has actually
    /// landed: [`execute_move`] resolves collisions, so the photograph planned
    /// for `photo.jpg` may arrive as `photo-1.jpg` and the sidecar has to follow
    /// the name it really got. A separate pass would either have to re-read the
    /// journal to find that out or guess, and guessing here silently unpairs the
    /// two files.
    ///
    /// Empty for a sidecar's own move: a sidecar has no sidecars, and a plan
    /// that allowed one would be a recursion nothing bounds.
    pub sidecars: Vec<Sidecar>,
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
            Self::EmbeddedOnly => source.is_recorded(),
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
    sidecars: &SidecarIndex,
    known_hash: Option<String>,
) -> Result<PlannedMove> {
    // Read once and used twice: the companions decide where the sidecars go
    // after the move, and — for an `.xmp` — they may also decide the date the
    // move is planned around in the first place.
    let companions = sidecars.for_parent(&file.path);

    let meta = metadata::extract_metadata(&file.path, file.is_video, tz)?;
    let meta = metadata::apply_sidecar_date(meta, companions, tz);

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
        // `&&` rather than `||` is unreachable today and stays on purpose: the
        // EXIF and ISO 6709 extractors both set the two coordinates together or
        // set neither, so no metadata this can be handed has one without the
        // other. A half-located file would be one this code cannot place, and
        // reporting it as located would be the wrong half of the guess.
        has_location: meta.latitude.is_some() && meta.longitude.is_some(),
        known_hash,
        sidecars: companions.to_vec(),
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

/// Which destinations a run has already spoken for.
///
/// The preview used to print `planned.destination` — the *unsuffixed* name —
/// for every file, because collision resolution happened only inside
/// [`execute_move`]. Three files sharing a second therefore all previewed to
/// one path, which is not merely imprecise: it is an outcome that cannot
/// happen, and it reads as "two of these are about to be overwritten" to the
/// one person a dry run exists to reassure. Duplicates were worse — never
/// planned at all, so a preview did not say where any of them would land.
///
/// This makes the plan say what the run will do. It does **not** take authority
/// away from the move: [`execute_move`] still walks candidates and lets
/// [`move_no_clobber`] be the only thing that decides a name is free. That is
/// deliberate, and it is what closed the TOCTOU overwrite in Phase 02. A ledger
/// that asked the filesystem and then trusted its own answer would put the bug
/// back. So the plan is a *prediction* — accurate for every file this run
/// places, and honest about nothing else touching the tree meanwhile — while
/// the move remains the *arbiter*, and the journal records where a file
/// actually went if the two ever diverge.
#[derive(Debug, Default)]
pub struct DestinationLedger {
    claimed: std::collections::HashSet<PathBuf>,
    /// Directories whose existing contents have already been read in.
    ///
    /// Seeded lazily, and per directory rather than per file, because a run
    /// into a library that is already organised is the ordinary case for this
    /// tool — and a ledger that knew only about this run's own files would
    /// mispredict every suffix the moment one name was already taken. One
    /// `read_dir` per date directory, not one `exists()` per file.
    scanned: std::collections::HashSet<PathBuf>,
}

impl DestinationLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim a destination, returning the name this run will actually use.
    ///
    /// Walks the same candidate sequence [`execute_move`] does, so preview and
    /// commit agree by construction rather than by two implementations
    /// happening to match.
    pub fn claim(&mut self, destination: &Path) -> PathBuf {
        if let Some(dir) = destination.parent() {
            self.seed_from_disk(dir);
        }

        for attempt in 0..MAX_COLLISION_ATTEMPTS {
            let candidate = collision_candidate(destination, attempt);
            if !self.claimed.contains(&candidate) {
                self.claimed.insert(candidate.clone());
                return candidate;
            }
        }

        // Ten thousand files claiming one name. `execute_move` gives up here
        // too, and reports it properly; predicting the unsuffixed name is the
        // truthful thing to hand it.
        destination.to_path_buf()
    }

    /// Read `dir` into the ledger once, so names already on disk are not
    /// predicted as free.
    ///
    /// An unreadable directory is not an error here: the ledger is a
    /// prediction, and a directory the run cannot list is one whose contents it
    /// cannot predict around. The move still refuses to clobber whatever is
    /// there.
    fn seed_from_disk(&mut self, dir: &Path) {
        if !self.scanned.insert(dir.to_path_buf()) {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            self.claimed.insert(entry.path());
        }
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

// Test-only: how many appends a manifest created on this thread accepts before
// every subsequent one fails.
//
// A write to an already-open file descriptor cannot be made to fail portably
// from a test — the disk going away mid-group is not reproducible — and "stop
// the group rather than relocate files nothing is recording" is exactly the
// behaviour that must not be shipped unexecuted. Same reasoning as
// `Sink::Failing` for the journal and the injected `copy` parameter on
// `copy_verify_delete`: the failure is introduced at the seam where a real one
// would appear.
//
// Thread-local rather than a `static`, so one test arming it cannot change what
// another test executes.
#[cfg(test)]
thread_local! {
    static MANIFEST_APPENDS_ACCEPTED: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
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
            Self::escape_for_manifest(&group.files[0]),
            group.files.len().saturating_sub(1),
        );
        for source in group.files.iter().skip(1) {
            let _ = writeln!(header, "{}", Self::escape_for_manifest(source));
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
        #[cfg(test)]
        if let Some(remaining) = MANIFEST_APPENDS_ACCEPTED.get() {
            if remaining == 0 {
                bail!(
                    "appending to manifest {}: the disk went away",
                    self.path.display()
                );
            }
            MANIFEST_APPENDS_ACCEPTED.set(Some(remaining - 1));
        }

        io::Write::write_all(&mut self.file, line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .with_context(|| format!("appending to manifest {}", self.path.display()))
    }

    /// Record where a duplicate actually landed — suffix and all, because a
    /// record that says only "moved" cannot be used to put anything back.
    ///
    /// A path rendered safe to write into a line-oriented text file.
    ///
    /// `manifest.txt` is the record a *person* reads, and it is one line per
    /// fact. A filename may legally contain a newline — on every filesystem
    /// this runs on — so a file called `holiday.jpg\n# moved: evidence.jpg`
    /// would write two lines and the second would read as an outcome that never
    /// happened. The same goes for a carriage return, which can hide the rest
    /// of a line on a terminal.
    ///
    /// The escape is deliberately lossy and deliberately visible: `\n` is
    /// written as the two characters `\` and `n`, so the manifest says what the
    /// name contains rather than obeying it. Nothing parses these lines — undo
    /// reads the JSONL journal, which escapes properly through `serde_json` —
    /// so readability is the only requirement.
    fn escape_for_manifest(path: &Path) -> String {
        path.display()
            .to_string()
            .chars()
            .map(|c| match c {
                '\n' => "\\n".to_string(),
                '\r' => "\\r".to_string(),
                c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
                c => c.to_string(),
            })
            .collect()
    }

    /// # Errors
    ///
    /// As [`GroupManifest::append`].
    fn record_move(&mut self, src: &Path, dst: &Path) -> Result<()> {
        self.append(&format!(
            "# moved: {} -> {}\n",
            Self::escape_for_manifest(src),
            Self::escape_for_manifest(dst)
        ))
    }

    /// Reopen an existing manifest for appending.
    ///
    /// The dedup pass closes its manifests before the organise pass begins, and
    /// the organise pass owes each of them one more line. Opening in append
    /// mode rather than holding every handle open across both passes keeps the
    /// file-descriptor count independent of how many duplicate groups a library
    /// turns out to have.
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest cannot be opened for appending.
    fn reopen(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("reopening manifest {}", path.display()))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Record where the retained original finally came to rest.
    ///
    /// Appended rather than written into the header, and appended *after* the
    /// organise pass rather than before it, because at manifest-creation time
    /// the original has not moved yet — the dedup pass runs first. The header's
    /// `# Original kept at:` is therefore an input path that the organise pass
    /// is about to empty, which is exactly what made `mmm-dedup-verifier`
    /// resolve nothing and report an all-clear over zero confirmed groups.
    ///
    /// Appending keeps the crash-safety the header was written for: nothing is
    /// rewritten, so an interrupted run still has every line it managed to
    /// flush.
    ///
    /// # Errors
    ///
    /// As [`GroupManifest::append`].
    fn record_original_destination(&mut self, dst: &Path) -> Result<()> {
        self.append(&format!(
            "# Original moved to: {}\n",
            Self::escape_for_manifest(dst)
        ))
    }

    /// Record a move that did not happen, and why.
    ///
    /// # Errors
    ///
    /// As [`GroupManifest::append`].
    fn record_failure(&mut self, src: &Path, reason: &str) -> Result<()> {
        self.append(&format!(
            "# FAILED: {}: {reason}\n",
            Self::escape_for_manifest(src)
        ))
    }

    /// Record a sidecar that followed a duplicate into this directory.
    ///
    /// A comment line, like every outcome — `mmm-dedup-verifier` reads
    /// non-`#` lines as intended sources, and a sidecar is not one. It is here
    /// because the manifest is the record a *person* reads, and a directory
    /// holding more files than the manifest accounts for is a directory nobody
    /// can act on.
    ///
    /// # Errors
    ///
    /// As [`GroupManifest::append`].
    fn record_sidecar(&mut self, src: &Path, dst: &Path) -> Result<()> {
        self.append(&format!(
            "# sidecar: {} -> {}\n",
            Self::escape_for_manifest(src),
            Self::escape_for_manifest(dst)
        ))
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
    /// A sidecar following its parent — see [`crate::sidecar`].
    ///
    /// Carries no hash, and not because one is unavailable: a sidecar is never
    /// hashed, because it is never deduplicated. Two identical `.xmp` files are
    /// two photographs' worth of edits that happen to agree, and relocating one
    /// of them to `duplicates/` would detach it from its photograph — which is
    /// the exact failure this whole module exists to prevent. Undo therefore
    /// verifies a sidecar by size alone, as it does any move made without a
    /// digest.
    Sidecar,
}

impl MovePurpose<'_> {
    fn intent_kind(self) -> IntentKind {
        match self {
            Self::Organise { .. } => IntentKind::Organise,
            Self::Duplicate { .. } => IntentKind::Duplicate,
            Self::Restore { .. } => IntentKind::Restore,
            Self::Sidecar => IntentKind::Sidecar,
        }
    }

    fn source_hash(self) -> Option<String> {
        match self {
            Self::Duplicate { hash, .. } => Some(hash.to_string()),
            Self::Organise { hash } | Self::Restore { hash } => hash.map(ToString::to_string),
            Self::Sidecar => None,
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
    /// Writes fail once `accepted` of them have succeeded. An open file
    /// descriptor cannot be made to fail from a test — the disk going away
    /// mid-run is not reproducible — so the one behaviour that matters, *stop
    /// rather than move unrecorded*, is driven through this instead. Same
    /// reasoning as the injected `copy` parameter on [`copy_verify_delete`].
    ///
    /// The counter is what makes "the journal died *after* the file moved"
    /// reachable: with `accepted = 1` the intent is written, the move happens,
    /// and the outcome is refused — the state undo has to survive.
    #[cfg(test)]
    Failing {
        accepted: usize,
    },
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
        Self::failing_after(0)
    }

    /// A recorder whose first `accepted` writes succeed and whose every write
    /// after that fails.
    #[cfg(test)]
    fn failing_after(accepted: usize) -> Self {
        Self {
            sink: Sink::Failing { accepted },
        }
    }

    fn append(&mut self, entry: &JournalEntry) -> Result<()> {
        match &mut self.sink {
            // Unreachable in production: `intend` returns before appending when
            // the sink is off, and `commit`/`failed` return before appending
            // when there is no sequence number — which there never is. Kept as
            // the total match it has to be rather than an `unreachable!()`,
            // because a panic here is a panic in the middle of moving somebody's
            // photograph.
            Sink::Off => Ok(()),
            Sink::Open(journal) => journal.append(entry),
            #[cfg(test)]
            Sink::Failing { accepted } => {
                if *accepted == 0 {
                    bail!("the journal is on a disk that went away");
                }
                *accepted -= 1;
                Ok(())
            }
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
            Sink::Failing { .. } => 0,
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
            MovePurpose::Organise { .. } | MovePurpose::Restore { .. } | MovePurpose::Sidecar => {
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

/// What moving one file's sidecars did.
///
/// Separate counts rather than additions to [`MoveRun`]'s, because that type
/// holds an invariant — `moved + errors + unprocessed == planned.len()` — and a
/// sidecar was never in `planned`. Folding them in would make the arithmetic
/// stop meaning anything at exactly the moment a reader most wants to trust it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SidecarRun {
    /// Sidecars that reached their parent's new directory.
    pub moved: usize,
    /// Sidecars whose move failed. Counted, logged, and stepped over — the
    /// photograph itself has already arrived, and unwinding that to keep the
    /// pair together would be a destructive answer to a non-destructive
    /// problem.
    pub errors: usize,
    /// Whether the journal refused a write. The caller stops the run.
    pub journal_failed: bool,
}

impl SidecarRun {
    fn add(&mut self, other: Self) {
        self.moved += other.moved;
        self.errors += other.errors;
        self.journal_failed |= other.journal_failed;
    }
}

/// Move every sidecar of a file that has just landed at `parent_destination`.
///
/// Called after the parent's move has succeeded and never before: a sidecar
/// whose parent did not move must not move either, or the pairing is broken by
/// the very code that exists to preserve it. There is no "sidecar moved, parent
/// failed" state to recover from because there is no way to reach one.
///
/// Each sidecar is journalled as its own intent-and-outcome pair, exactly like a
/// photograph, so `mmm undo` puts both files back with no special case at all.
fn move_sidecars(
    recorder: &mut MoveRecorder<'_>,
    sidecars: &[Sidecar],
    parent_destination: &Path,
    mut record: impl FnMut(&Path, &Path),
) -> SidecarRun {
    let mut run = SidecarRun::default();

    for sidecar in sidecars {
        let planned = PlannedMove {
            source: sidecar.path.clone(),
            destination: sidecar.destination_beside(parent_destination),
            // A sidecar is filed by its parent, not by a date: nothing ever read
            // one out of it, and claiming a source here would put a file in the
            // date tally that was never dated.
            date_source: DateSource::None,
            timezone_source: None,
            has_location: false,
            known_hash: None,
            sidecars: Vec::new(),
        };

        match recorded_move(recorder, &planned, MovePurpose::Sidecar) {
            Ok(outcome) => {
                debug!(
                    src = %planned.source.display(),
                    dst = %outcome.destination.display(),
                    "moved a sidecar with its parent"
                );
                record(&planned.source, &outcome.destination);
                run.moved += 1;
            }
            Err(RecordedMoveError::Move(e)) => {
                error!(
                    src = %planned.source.display(),
                    dst = %planned.destination.display(),
                    error = %format!("{e:#}"),
                    "a sidecar could not follow its parent; the photograph moved and the \
                     sidecar did not"
                );
                run.errors += 1;
            }
            Err(RecordedMoveError::Journal { error, moved }) => {
                if moved {
                    run.moved += 1;
                }
                error!(
                    src = %planned.source.display(),
                    moved,
                    error = %format!("{error:#}"),
                    "the run journal could not be written while moving a sidecar; stopping so \
                     that no further move goes unrecorded"
                );
                run.journal_failed = true;
                break;
            }
        }
    }

    run
}

/// One duplicate relocation, planned but not yet done.
///
/// Exists so the preview and the committing run read from the *same*
/// computation rather than two that happen to agree. Duplicates used never to
/// be planned at all — [`crate::reporter::print_duplicates`] reported them as
/// group counts — so a dry run genuinely did not say where any of them would
/// land, which is the one question a dry run is for.
#[derive(Debug, Clone)]
pub struct DuplicatePlan {
    /// Which `duplicates/NNN/` directory this belongs to.
    pub group: usize,
    pub planned: PlannedMove,
}

/// Work out where every duplicate will go, claiming each name as it goes.
///
/// Pure but for the ledger it writes into, and called on both postures: the
/// preview prints these, and the committing run executes exactly these. The
/// ledger is shared with the organise pass, so a duplicate and a photograph can
/// never be predicted onto the same path.
pub fn plan_duplicate_moves(
    groups: &[DuplicateGroup],
    output_dir: &Path,
    duplicates_dir: &Path,
    sidecars: &SidecarIndex,
    ledger: &mut DestinationLedger,
) -> Vec<DuplicatePlan> {
    let dup_base = output_dir.join(duplicates_dir);
    let mut plans = Vec::new();

    for (i, group) in groups.iter().enumerate() {
        let group_dir = dup_base.join(format!("{i:03}"));

        // The first file is the retained original and is organised by the other
        // pass; everything after it is set aside here.
        for dup_path in group.files.iter().skip(1) {
            let filename = dup_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            plans.push(DuplicatePlan {
                group: i,
                planned: PlannedMove {
                    source: dup_path.clone(),
                    destination: ledger.claim(&group_dir.join(&filename)),
                    date_source: DateSource::None,
                    // A duplicate is filed by its group, not by its date, so no
                    // wall clock was ever chosen for it.
                    timezone_source: None,
                    has_location: false,
                    // The duplicate pass carries its digest on the purpose,
                    // which is where the journal reads it from for this kind of
                    // move.
                    known_hash: None,
                    // A duplicate's sidecar travels with it. It is the same
                    // argument as for an organised file and it bites harder
                    // here: a photograph in `duplicates/007/` is already the
                    // copy nobody is looking at, and an `.xmp` left behind in
                    // the source tree with no file to pair against is the one
                    // that gets deleted in the next tidy-up.
                    sidecars: sidecars.for_parent(dup_path).to_vec(),
                },
            });
        }
    }

    plans
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
    plans: &[DuplicatePlan],
    recorder: &mut MoveRecorder<'_>,
) -> Result<DuplicateRun> {
    let dup_base = output_dir.join(duplicates_dir);
    let mut moved = 0;
    let mut errors = 0;
    let mut sidecar_run = SidecarRun::default();
    let mut original_manifests = HashMap::new();

    for (i, group) in groups.iter().enumerate() {
        let group_dir = dup_base.join(format!("{i:03}"));
        fs::create_dir_all(&group_dir)
            .with_context(|| format!("creating duplicate dir {}", group_dir.display()))?;

        // Before a single file moves.
        let manifest_path = group_dir.join("manifest.txt");
        let mut manifest = GroupManifest::create(&manifest_path, i, group)?;

        // The retained original is still at its input path; the organise pass
        // will move it and owes this manifest the destination.
        if let Some(original) = group.files.first() {
            original_manifests.insert(original.clone(), manifest_path.clone());
        }

        // Exactly the moves the preview printed — `plan_duplicate_moves` is the
        // only thing that decides a duplicate's destination, so a run cannot
        // put a file somewhere the plan did not say.
        let group_plans: Vec<&DuplicatePlan> =
            plans.iter().filter(|plan| plan.group == i).collect();

        for (done, plan) in group_plans.iter().enumerate() {
            let planned = &plan.planned;
            let dup_path = &planned.source;

            let purpose = MovePurpose::Duplicate {
                group: i,
                hash: &group.hash,
            };

            let manifested = match recorded_move(recorder, planned, purpose) {
                Ok(outcome) => {
                    moved += 1;
                    // The duplicate's own line first, then its sidecars', so the
                    // manifest reads in the order things happened. Each is
                    // appended as it happens, for the same reason the header is
                    // written before the first move: a record assembled
                    // afterwards is one an interruption costs entirely.
                    let listed = manifest.record_move(dup_path, &outcome.destination);
                    if listed.is_ok() {
                        let mut manifest_error = None;
                        sidecar_run.add(move_sidecars(
                            recorder,
                            &planned.sidecars,
                            &outcome.destination,
                            |src, dst| {
                                if manifest_error.is_none() {
                                    manifest_error = manifest.record_sidecar(src, dst).err();
                                }
                            },
                        ));
                        if sidecar_run.journal_failed {
                            return Err(anyhow::anyhow!(
                                "the run journal could not be written while moving a sidecar of \
                                 duplicate {}; the remaining duplicates have been left where they \
                                 are",
                                dup_path.display()
                            ));
                        }
                        match manifest_error {
                            Some(e) => Err(e),
                            None => Ok(()),
                        }
                    } else {
                        listed
                    }
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

    Ok(DuplicateRun {
        moved,
        errors,
        sidecars: sidecar_run,
        original_manifests,
    })
}

/// What the duplicate pass did.
///
/// A struct rather than the `(usize, usize)` it used to return, because a third
/// figure arrived and a three-element tuple at a call site says nothing about
/// which number is which. The sidecar counts are held apart from the duplicate
/// ones for the reason on [`SidecarRun`]: a sidecar was never a duplicate, and
/// adding it to a duplicate count would misreport how much of somebody's library
/// this run considered redundant.
// Not `Copy`: `original_manifests` owns its paths. The counts were copyable and
// the backlink map is not, which is the right trade — the alternative is
// handing the organise pass a reference to something the dedup pass has to keep
// alive for it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DuplicateRun {
    /// Duplicates relocated into `duplicates/NNN/`.
    pub moved: usize,
    /// Duplicates whose move failed, plus those abandoned when a manifest
    /// stopped being writable.
    pub errors: usize,
    /// What became of the sidecars travelling with them.
    pub sidecars: SidecarRun,
    /// Where each group's retained original was, and which manifest is waiting
    /// to be told where it ended up.
    ///
    /// The dedup pass runs before the organise pass, so a manifest is written
    /// naming an original that has not moved yet — and the organise pass then
    /// moves it, leaving the recorded path empty. This is how the organise pass
    /// finds the manifests it owes a line to. See
    /// [`GroupManifest::record_original_destination`].
    ///
    /// Keyed by the original's *source* path because that is what the organise
    /// pass has in hand for each planned move. One entry per group, not per
    /// file, so it costs nothing on a library with few duplicates and does not
    /// grow with the library.
    pub original_manifests: HashMap<PathBuf, PathBuf>,
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
    /// What became of the sidecars travelling with those files.
    ///
    /// Held apart from the three counts above so their invariant survives — see
    /// [`SidecarRun`].
    pub sidecars: SidecarRun,
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
    original_manifests: &HashMap<PathBuf, PathBuf, impl std::hash::BuildHasher>,
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
                Ok(outcome) => {
                    run.moved += 1;
                    // After the photograph has landed, and using where it landed
                    // rather than where it was planned to: collision resolution
                    // may have given it a suffix, and a sidecar derived from the
                    // planned name would be unpaired by the very suffix that
                    // saved its parent.
                    run.sidecars.add(move_sidecars(
                        recorder,
                        &planned.sidecars,
                        &outcome.destination,
                        |_, _| {},
                    ));
                    if run.sidecars.journal_failed {
                        run.journal_failed = true;
                    }

                    // If this file was a duplicate group's retained original,
                    // its manifest still names the input path this move has
                    // just emptied. Tell it where the file actually went.
                    //
                    // A failure here is logged and stepped over rather than
                    // stopping the run: the photograph has already moved and is
                    // recorded in the journal, which is what `undo` reads. The
                    // manifest is the verifier's record, and a run that halted
                    // over it would be trading a real library for a report.
                    if let Some(manifest_path) = original_manifests.get(&planned.source) {
                        if let Err(e) = GroupManifest::reopen(manifest_path)
                            .and_then(|mut m| m.record_original_destination(&outcome.destination))
                        {
                            error!(
                                manifest = %manifest_path.display(),
                                dst = %outcome.destination.display(),
                                error = %format!("{e:#}"),
                                "could not record where the retained original landed; \
                                 mmm-dedup-verifier will report this group as missing"
                            );
                        }
                    }
                }
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

/// A temp filename unique within this process, and not guessable outside it.
///
/// The millisecond alone is not unique — a run moving small files clears
/// several per millisecond — and two moves sharing a temp name would have one
/// overwrite the other's copy. `copy_hashing` creates the temp with
/// `O_CREAT | O_EXCL` and would refuse rather than corrupt, but refusing a move
/// over a clock collision is still a failure nobody should have to read about.
///
/// The random component answers a different question. A name of only a
/// timestamp and a counter can be predicted by anyone who can watch the output
/// directory, and pre-creating that path makes the move fail — `O_CREAT |
/// O_EXCL` turns it into a clean refusal rather than a lost file, but a refusal
/// that somebody else chose is still a denial of service. Guessing a name is
/// cheap; guessing six base36 characters per file is not.
fn temp_file_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    format!(
        ".tmp-{}-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed),
        crate::journal::short_random()
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
    use crate::sidecar::Convention;
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

    /// A newline in a filename cannot forge a manifest line.
    ///
    /// `manifest.txt` is one line per fact, and a filename may legally hold a
    /// newline on every filesystem this runs on. Unescaped, a file named to
    /// look like an outcome line writes one.
    #[test]
    fn a_newline_in_a_filename_cannot_forge_a_manifest_line() {
        let hostile = Path::new("holiday.jpg\n# moved: /evidence.jpg -> /gone.jpg");
        let rendered = GroupManifest::escape_for_manifest(hostile);

        assert!(
            !rendered.contains('\n'),
            "the rendering still holds a real newline: {rendered:?}"
        );
        assert!(
            rendered.contains("\\n"),
            "the newline should be visible as an escape: {rendered:?}"
        );
        assert!(
            rendered.starts_with("holiday.jpg"),
            "the ordinary part of the name must survive: {rendered:?}"
        );

        // A carriage return hides the rest of a line on a terminal, and a raw
        // control character can reposition the cursor.
        assert!(!GroupManifest::escape_for_manifest(Path::new("a\rb")).contains('\r'));
        assert_eq!(
            GroupManifest::escape_for_manifest(Path::new("a\u{1b}b")),
            "a\\u{1b}b",
            "an escape character is rendered, not emitted"
        );

        // And an ordinary path is left exactly as it was.
        assert_eq!(
            GroupManifest::escape_for_manifest(Path::new("/photos/2024-03-15/a.jpg")),
            "/photos/2024-03-15/a.jpg"
        );
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
            sidecars: Vec::new(),
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

        let run = move_duplicates(
            std::slice::from_ref(&group),
            &output,
            Path::new("duplicates"),
            &dup_plans(
                std::slice::from_ref(&group),
                &output,
                &SidecarIndex::empty(),
            ),
            &mut MoveRecorder::disabled(),
        )
        .unwrap();
        let (moved, errors) = (run.moved, run.errors);

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
        let run = move_duplicates(
            std::slice::from_ref(&group),
            &output,
            Path::new("duplicates"),
            &dup_plans(
                std::slice::from_ref(&group),
                &output,
                &SidecarIndex::empty(),
            ),
            &mut MoveRecorder::disabled(),
        )
        .unwrap();
        let (moved, errors) = (run.moved, run.errors);
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
    /// No duplicate group's original is among these moves.
    ///
    /// Most move-path tests are about chunking and failure handling, not about
    /// duplicates, so they hand `process_moves` an empty backlink map — the
    /// same thing a run over a library with no duplicates gives it.
    /// The duplicate plans production would build for these groups.
    ///
    /// Tests call `move_duplicates` with the plans rather than letting it
    /// derive destinations, because that is now the only way it works — one
    /// planner, used by the preview and the run alike.
    fn dup_plans(
        groups: &[DuplicateGroup],
        output: &Path,
        sidecars: &SidecarIndex,
    ) -> Vec<DuplicatePlan> {
        plan_duplicate_moves(
            groups,
            output,
            Path::new("duplicates"),
            sidecars,
            &mut DestinationLedger::new(),
        )
    }

    fn no_backlinks() -> HashMap<PathBuf, PathBuf> {
        HashMap::new()
    }

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

        let run = process_moves(
            &planned,
            2,
            &mut controller,
            &mut MoveRecorder::disabled(),
            &no_backlinks(),
        );

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

        let run = process_moves(
            &planned,
            2,
            &mut controller,
            &mut MoveRecorder::disabled(),
            &no_backlinks(),
        );

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

        let run = process_moves(
            &planned,
            2,
            &mut controller,
            &mut MoveRecorder::disabled(),
            &no_backlinks(),
        );

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

        let run = process_moves(
            &planned,
            0,
            &mut controller,
            &mut MoveRecorder::disabled(),
            &no_backlinks(),
        );

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
            &no_backlinks(),
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
            &no_backlinks(),
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
            &no_backlinks(),
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

        let run = process_moves(
            &planned,
            2,
            &mut controller,
            &mut MoveRecorder::failing(),
            &no_backlinks(),
        );

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
        let run = move_duplicates(
            std::slice::from_ref(&group),
            &output,
            Path::new("duplicates"),
            &dup_plans(
                std::slice::from_ref(&group),
                &output,
                &SidecarIndex::empty(),
            ),
            &mut MoveRecorder::new(Some(&mut journal)),
        )
        .unwrap();
        assert_eq!((run.moved, run.errors), (1, 0));

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
            std::slice::from_ref(&group),
            &tmp.path().join("output"),
            Path::new("duplicates"),
            &dup_plans(
                std::slice::from_ref(&group),
                &tmp.path().join("output"),
                &SidecarIndex::empty(),
            ),
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
            &no_backlinks(),
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

        let run = process_moves(
            &planned,
            0,
            &mut controller,
            &mut MoveRecorder::disabled(),
            &no_backlinks(),
        );

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

    // -----------------------------------------------------------------
    // The failure paths
    //
    // Everything below drives a branch that only runs when something has
    // already gone wrong. They are the branches that matter most and the ones
    // a passing run never touches, so without a test here they are shipped
    // unexecuted — and the code that runs after a photograph has moved but
    // before it has been recorded is the code with the least room to be wrong.
    // -----------------------------------------------------------------

    /// Run `f` with a subscriber attached, so the `error!`/`debug!` calls on
    /// the failure paths actually render their arguments.
    ///
    /// Without one, `tracing` short-circuits before evaluating the format
    /// arguments and a `Display` impl that panicked mid-run would never be
    /// exercised by the suite. Scoped rather than global: `with_default` is
    /// thread-local, so one test enabling logging cannot silently change what
    /// another test executes.
    ///
    /// The output goes to `io::sink` — this is about running the formatting
    /// code, not about reading it.
    fn with_logs<T>(f: impl FnOnce() -> T) -> T {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        tracing::subscriber::with_default(subscriber, f)
    }

    /// True when the process ignores permission bits, which is what running as
    /// root means for every test below that revokes them.
    fn permission_bits_apply(dir: &Path) -> bool {
        fs::write(dir.join(".probe"), b"p").is_err()
    }

    /// A planned move carrying one sidecar.
    fn plan_with_sidecar(src: &Path, dst: &Path, sidecar: &Path) -> PlannedMove {
        PlannedMove {
            sidecars: vec![Sidecar {
                path: sidecar.to_path_buf(),
                convention: Convention::Stem,
            }],
            ..plan(src, dst)
        }
    }

    /// A sidecar that will not move is counted and stepped over. The
    /// photograph has already arrived, and unwinding that to keep the pair
    /// together would answer a non-destructive problem destructively.
    #[test]
    fn a_sidecar_that_cannot_move_is_counted_and_the_photograph_is_left_where_it_landed() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        let src = input.join("photo.jpg");
        fs::write(&src, b"BODY").unwrap();

        // The sidecar is named but never written, so its move fails at the
        // link with `ENOENT` — the shape of a file deleted between the scan
        // and the move.
        let planned = plan_with_sidecar(&src, &output.join("photo.jpg"), &input.join("photo.xmp"));

        let run = with_logs(|| {
            process_moves(
                std::slice::from_ref(&planned),
                0,
                &mut ScriptedController::new(&[]),
                &mut MoveRecorder::disabled(),
                &no_backlinks(),
            )
        });

        assert_eq!((run.moved, run.errors), (1, 0));
        assert_eq!(
            (run.sidecars.moved, run.sidecars.errors),
            (0, 1),
            "the sidecar failure belongs to the sidecar counts, not the file ones"
        );
        assert!(!run.journal_failed);
        assert!(output.join("photo.jpg").exists(), "the photograph landed");
    }

    /// The journal refusing a write while a sidecar is in flight stops the run
    /// — the same rule as for a photograph, because `undo` replays both from
    /// the same record.
    #[test]
    fn a_journal_failure_while_moving_a_sidecar_stops_the_run() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        let src = input.join("photo.jpg");
        let sidecar = input.join("photo.xmp");
        fs::write(&src, b"BODY").unwrap();
        fs::write(&sidecar, b"<xmp/>").unwrap();

        let planned = plan_with_sidecar(&src, &output.join("photo.jpg"), &sidecar);

        // Two writes accepted — the photograph's intent and its outcome — then
        // the sidecar's intent is refused.
        let run = with_logs(|| {
            process_moves(
                std::slice::from_ref(&planned),
                0,
                &mut ScriptedController::new(&[]),
                &mut MoveRecorder::failing_after(2),
                &no_backlinks(),
            )
        });

        assert!(
            run.sidecars.journal_failed && run.journal_failed,
            "a sidecar's journal failure must stop the whole run, not just its own loop"
        );
        assert_eq!(
            run.moved, 1,
            "the photograph did move and is recorded as such"
        );
        assert_eq!(run.sidecars.moved, 0);
        assert!(
            sidecar.exists(),
            "the sidecar must not move once its intent was refused"
        );
    }

    /// The journal dying between the move and the outcome line is the state
    /// `undo` exists to survive: the file *has* moved, so it is counted as
    /// moved, and the run stops rather than accumulating more of them.
    #[test]
    fn a_journal_failure_after_the_file_moved_still_counts_the_move() {
        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());

        // One write accepted: the first file's intent. Its outcome is refused.
        let run = with_logs(|| {
            process_moves(
                &planned,
                0,
                &mut ScriptedController::new(&[]),
                &mut MoveRecorder::failing_after(1),
                &no_backlinks(),
            )
        });

        assert!(run.journal_failed);
        assert_eq!(
            (run.moved, run.errors, run.unprocessed),
            (1, 0, 3),
            "the file that moved is counted, and every other file is accounted for"
        );
        assert!(
            planned[0].destination.exists(),
            "the first file moved before the journal refused its outcome"
        );
        assert!(
            planned[1].source.exists(),
            "nothing after the failure was attempted"
        );
    }

    /// A move that fails *and* a journal that cannot record the failure is
    /// reported as the journal failure, because that is the condition that
    /// stops the run — the failed move on its own would not have.
    #[test]
    fn a_journal_that_cannot_record_a_failed_move_reports_the_journal() {
        let tmp = TempDir::new().unwrap();
        let planned = plan(
            &tmp.path().join("gone.jpg"),
            &tmp.path().join("output/photo.jpg"),
        );

        // The intent is accepted; the move fails on the missing source; the
        // `failed` line is refused.
        let mut recorder = MoveRecorder::failing_after(1);
        let err = with_logs(|| {
            recorded_move(
                &mut recorder,
                &planned,
                MovePurpose::Organise { hash: None },
            )
            .expect_err("a refused journal write must not report a successful move")
        });

        match err {
            RecordedMoveError::Journal { moved, .. } => assert!(
                !moved,
                "nothing moved, so the caller must not be told a file is unaccounted for"
            ),
            RecordedMoveError::Move(e) => {
                panic!("the journal failure must win over the move failure: {e}")
            }
        }
    }

    /// The duplicate pass stops the whole run on a journal failure rather than
    /// relocating the rest of the group into a numbered directory with nothing
    /// on disk saying where those files came from.
    #[test]
    fn a_journal_failure_while_relocating_a_duplicate_abandons_the_rest() {
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

        let err = with_logs(|| {
            move_duplicates(
                std::slice::from_ref(&group),
                &output,
                Path::new("duplicates"),
                &dup_plans(
                    std::slice::from_ref(&group),
                    &output,
                    &SidecarIndex::empty(),
                ),
                &mut MoveRecorder::failing(),
            )
            .expect_err("a refused journal write must stop the duplicate pass")
        });

        assert!(
            format!("{err:#}").contains("left where they are"),
            "the operator has to be told the rest were not touched; got: {err:#}"
        );
        assert!(copy.exists(), "the duplicate must not have moved");
    }

    /// And the same for a duplicate's sidecar: the photograph reached
    /// `duplicates/000/`, its `.xmp` could not be recorded, and the pass stops
    /// rather than carrying on unrecorded.
    #[test]
    fn a_journal_failure_while_moving_a_duplicates_sidecar_abandons_the_rest() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let output = tmp.path().join("output");

        let kept = input.join("kept.jpg");
        let copy = input.join("copy.jpg");
        let sidecar = input.join("copy.xmp");
        for path in [&kept, &copy] {
            fs::write(path, b"BODY").unwrap();
        }
        fs::write(&sidecar, b"<xmp/>").unwrap();

        let group = duplicate_group(&[kept, copy.clone()], b"BODY");
        let index = SidecarIndex::build(
            &[ScannedFile {
                path: copy.clone(),
                size: 4,
                extension: "jpg".to_string(),
                is_video: false,
            }],
            std::slice::from_ref(&sidecar),
        );
        assert_eq!(index.paired(), 1, "the fixture must actually pair");

        // The duplicate's own intent and outcome are accepted; the sidecar's
        // intent is refused.
        let err = with_logs(|| {
            move_duplicates(
                std::slice::from_ref(&group),
                &output,
                Path::new("duplicates"),
                &dup_plans(std::slice::from_ref(&group), &output, &index),
                &mut MoveRecorder::failing_after(2),
            )
            .expect_err("a refused journal write must stop the duplicate pass")
        });

        assert!(
            format!("{err:#}").contains("sidecar of duplicate"),
            "the error must name what was in flight; got: {err:#}"
        );
        assert!(sidecar.exists(), "the sidecar must not have moved");
    }

    /// Arm the manifest failure injection for the duration of `f`, and disarm
    /// it afterwards however `f` ends.
    fn with_manifest_failing_after<T>(accepted: usize, f: impl FnOnce() -> T) -> T {
        struct Disarm;
        impl Drop for Disarm {
            fn drop(&mut self) {
                MANIFEST_APPENDS_ACCEPTED.set(None);
            }
        }
        MANIFEST_APPENDS_ACCEPTED.set(Some(accepted));
        let _disarm = Disarm;
        f()
    }

    /// A manifest that stops being writable stops its group. The rest of the
    /// group is left where the user can still find it, and counted as errors
    /// rather than quietly dropped from the totals — the alternative is files
    /// relocated into a numbered directory with nothing on disk saying where
    /// they came from.
    #[test]
    fn a_manifest_that_stops_being_writable_abandons_the_rest_of_its_group() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let output = tmp.path().join("output");

        let files: Vec<PathBuf> = (0..4)
            .map(|i| {
                let path = input.join(format!("photo-{i}.jpg"));
                fs::write(&path, b"BODY").unwrap();
                path
            })
            .collect();
        let group = duplicate_group(&files, b"BODY");

        // One outcome line accepted — the first duplicate's — then the
        // manifest refuses. Three duplicates are in the group, so two are
        // abandoned.
        let run = with_manifest_failing_after(1, || {
            with_logs(|| {
                move_duplicates(
                    std::slice::from_ref(&group),
                    &output,
                    Path::new("duplicates"),
                    &dup_plans(
                        std::slice::from_ref(&group),
                        &output,
                        &SidecarIndex::empty(),
                    ),
                    &mut MoveRecorder::disabled(),
                )
            })
        })
        .expect("an unwritable manifest stops the group, it does not fail the pass");

        assert_eq!(
            (run.moved, run.errors),
            (2, 1),
            "two moves happened before the manifest refused; the last is counted as abandoned"
        );
        assert!(
            files[3].exists(),
            "the abandoned duplicate must still be where the user can find it"
        );
    }

    /// The manifest refusing the *first* line stops the group before a second
    /// file moves — the same ordering as the journal's, and for the same
    /// reason.
    #[test]
    fn a_manifest_that_refuses_the_first_outcome_stops_the_group_immediately() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let output = tmp.path().join("output");

        let files: Vec<PathBuf> = (0..3)
            .map(|i| {
                let path = input.join(format!("photo-{i}.jpg"));
                fs::write(&path, b"BODY").unwrap();
                path
            })
            .collect();
        let group = duplicate_group(&files, b"BODY");

        let run = with_manifest_failing_after(0, || {
            with_logs(|| {
                move_duplicates(
                    std::slice::from_ref(&group),
                    &output,
                    Path::new("duplicates"),
                    &dup_plans(
                        std::slice::from_ref(&group),
                        &output,
                        &SidecarIndex::empty(),
                    ),
                    &mut MoveRecorder::disabled(),
                )
            })
        })
        .expect("an unwritable manifest stops the group, it does not fail the pass");

        assert_eq!((run.moved, run.errors), (1, 1));
        assert!(
            files[2].exists(),
            "nothing after the refused line may be relocated"
        );
    }

    /// A sidecar's manifest line is subject to the same rule: the group stops
    /// rather than carry on with an incomplete record.
    #[test]
    fn a_manifest_that_refuses_a_sidecar_line_stops_the_group() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        fs::create_dir_all(&input).unwrap();
        let output = tmp.path().join("output");

        let kept = input.join("kept.jpg");
        let copy = input.join("copy.jpg");
        let other = input.join("other.jpg");
        for path in [&kept, &copy, &other] {
            fs::write(path, b"BODY").unwrap();
        }
        let sidecar = input.join("copy.xmp");
        fs::write(&sidecar, b"<xmp/>").unwrap();

        let group = duplicate_group(&[kept, copy.clone(), other.clone()], b"BODY");
        let index = SidecarIndex::build(
            &[ScannedFile {
                path: copy,
                size: 4,
                extension: "jpg".to_string(),
                is_video: false,
            }],
            &[sidecar],
        );

        // The duplicate's own line is accepted; its sidecar's is refused.
        let run = with_manifest_failing_after(1, || {
            with_logs(|| {
                move_duplicates(
                    std::slice::from_ref(&group),
                    &output,
                    Path::new("duplicates"),
                    &dup_plans(std::slice::from_ref(&group), &output, &index),
                    &mut MoveRecorder::disabled(),
                )
            })
        })
        .expect("an unwritable manifest stops the group, it does not fail the pass");

        assert_eq!(run.sidecars.moved, 1, "the sidecar itself did travel");
        assert!(
            other.exists(),
            "the group stops once its record stops being complete"
        );
    }

    /// Every candidate name being taken is a hard stop, not a silent skip: the
    /// alternative is a photograph reported as moved that is still in the
    /// source tree.
    ///
    /// Ten thousand occupied names is the documented cap, so this creates them.
    #[test]
    fn a_destination_with_every_candidate_taken_fails_rather_than_overwriting() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();

        let src = input.join("photo.jpg");
        fs::write(&src, b"BODY").unwrap();

        let dst = output.join("photo.jpg");
        fs::write(&dst, b"OCCUPIED").unwrap();
        for attempt in 1..MAX_COLLISION_ATTEMPTS {
            fs::write(collision_candidate(&dst, attempt), b"OCCUPIED").unwrap();
        }

        let err =
            with_logs(|| execute_move(&plan(&src, &dst)).expect_err("no free name means no move"));
        let chain = format!("{err:#}");

        assert!(
            chain.contains("no free destination") && chain.contains(&src.display().to_string()),
            "the error must name the file that could not be filed; got: {chain}"
        );
        assert!(src.exists(), "the source must survive a failed move");
        assert_eq!(
            fs::read(&dst).unwrap(),
            b"OCCUPIED",
            "nothing may be overwritten"
        );
    }

    /// A source `link(2)` cannot handle is answered with a copy, not with a
    /// failure. Linking a *directory* returns `EPERM` on both Linux and macOS,
    /// which is the same errno an exFAT card returns for a file — so this
    /// drives the "link is impossible, copy instead" decision without needing
    /// a second volume or a FAT filesystem on the machine running the tests.
    ///
    /// The copy then fails, because a directory has no bytes to copy. That is
    /// the assertion: the decision was taken, and the failure that followed
    /// left nothing behind.
    #[test]
    #[cfg(unix)]
    fn a_source_that_cannot_be_linked_is_answered_with_a_copy() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("not-a-file");
        fs::create_dir(&src).unwrap();
        let output = tmp.path().join("output");
        fs::create_dir_all(&output).unwrap();
        let dst = output.join("photo.jpg");

        let err = with_logs(|| {
            execute_move(&plan(&src, &dst)).expect_err("a directory cannot be copied into place")
        });

        assert!(
            format!("{err:#}").contains("temp file"),
            "the copy path must have been taken; got: {err:#}"
        );
        let leftovers: Vec<String> = fs::read_dir(&output)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed copy must leave no temp file behind; found {leftovers:?}"
        );
    }

    /// Claiming the destination can fail for reasons that are not "the name is
    /// taken" — a directory that has gone away, most obviously. That must
    /// surface as fatal rather than as a retryable collision, or the caller
    /// walks ten thousand candidate names against a directory that is not
    /// there.
    #[test]
    fn reserve_and_rename_reports_a_missing_directory_as_fatal() {
        let tmp = TempDir::new().unwrap();
        let temp_file = tmp.path().join("temp");
        fs::write(&temp_file, b"BODY").unwrap();

        let err = reserve_and_rename(&temp_file, &tmp.path().join("gone/photo.jpg"))
            .expect_err("a missing destination directory must not read as a free name");

        match err {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("claiming destination"),
                "got: {e:#}"
            ),
            MoveError::DestinationExists(p) => {
                panic!(
                    "a missing directory is not an occupied name: {}",
                    p.display()
                )
            }
        }
    }

    /// The placeholder `reserve_and_rename` creates is its own to clean up. A
    /// rename that fails after the claim must not leave an empty file sitting
    /// where a photograph was meant to go — indistinguishable from a
    /// zero-length photo to everything except this code.
    #[test]
    fn a_failed_rename_removes_the_placeholder_it_claimed() {
        let tmp = TempDir::new().unwrap();
        let dst = tmp.path().join("photo.jpg");

        let err = reserve_and_rename(&tmp.path().join("never-written"), &dst)
            .expect_err("renaming a temp file that does not exist must fail");

        match err {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("renaming the verified copy"),
                "got: {e:#}"
            ),
            MoveError::DestinationExists(p) => panic!("unexpected collision: {}", p.display()),
        }
        assert!(
            !dst.exists(),
            "the empty placeholder must not be left behind at {}",
            dst.display()
        );
    }

    /// Promotion failing at the link is fatal and says so. The temp file was
    /// written beside the destination, so there is no second volume to fall
    /// back to and no honest reason to copy again.
    #[test]
    fn promotion_that_cannot_link_is_fatal() {
        let tmp = TempDir::new().unwrap();
        let err = promote_into_place(
            &tmp.path().join("never-written"),
            &tmp.path().join("dst.jpg"),
        )
        .expect_err("promoting a temp file that does not exist must fail");

        match err {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("promoting the verified copy"),
                "got: {e:#}"
            ),
            MoveError::DestinationExists(p) => panic!("unexpected collision: {}", p.display()),
        }
    }

    /// Promotion onto an occupied name is a collision, not a failure — the
    /// caller walks to the next candidate.
    #[test]
    fn promotion_onto_an_occupied_name_is_a_collision() {
        let tmp = TempDir::new().unwrap();
        let temp_file = tmp.path().join("temp");
        let dst = tmp.path().join("dst.jpg");
        fs::write(&temp_file, b"BODY").unwrap();
        fs::write(&dst, b"OCCUPIED").unwrap();

        match promote_into_place(&temp_file, &dst) {
            Err(MoveError::DestinationExists(p)) => assert_eq!(p, dst),
            other => panic!("an occupied destination must be reported as one: {other:?}"),
        }
        assert_eq!(fs::read(&dst).unwrap(), b"OCCUPIED");
    }

    /// A temp file that links into place but will not go away leaves two names
    /// for one file. `link_and_unlink` drops the new name rather than leave
    /// that state, and the failure is reported as the source-removal problem
    /// it is.
    #[test]
    #[cfg(unix)]
    fn promotion_that_cannot_remove_the_temp_file_is_fatal_and_unwinds() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        let output = tmp.path().join("output");
        fs::create_dir_all(&locked).unwrap();
        fs::create_dir_all(&output).unwrap();

        let temp_file = locked.join("temp");
        fs::write(&temp_file, b"BODY").unwrap();
        let dst = output.join("photo.jpg");

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let applies = permission_bits_apply(&locked);
        let result = if applies {
            Some(promote_into_place(&temp_file, &dst))
        } else {
            None
        };
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let Some(result) = result else {
            eprintln!(
                "SKIPPED promotion_that_cannot_remove_the_temp_file_is_fatal_and_unwinds: writes \
                 to a 0o555 directory succeeded, so this process ignores permission bits (running \
                 as root?)"
            );
            return;
        };

        match result.expect_err("a temp file that will not unlink must not report success") {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("removing the temp file"),
                "got: {e:#}"
            ),
            MoveError::DestinationExists(p) => panic!("unexpected collision: {}", p.display()),
        }
        assert!(
            !dst.exists(),
            "the link must be undone rather than leave two names for one file"
        );
        assert!(temp_file.exists(), "the temp file is still where it was");
    }

    /// Both kinds of move name themselves. The strings reach the operator via
    /// the `moved` log line and the journal's rendering, so a `Display` that
    /// panicked or said the wrong thing would be read as fact about what
    /// happened to a photograph.
    #[test]
    fn both_move_kinds_describe_themselves() {
        assert_eq!(MoveKind::Renamed.to_string(), "same-volume link");
        assert_eq!(
            MoveKind::CrossVolume.to_string(),
            "cross-volume copy+verify+delete"
        );
    }

    /// And both move errors. The occupied-name case is the one an operator
    /// sees most, since it is what a second photograph with the same computed
    /// name produces.
    #[test]
    fn both_move_errors_describe_themselves() {
        let taken = MoveError::DestinationExists(PathBuf::from("/photos/2024-03-15/photo.jpg"));
        assert_eq!(
            taken.to_string(),
            "destination /photos/2024-03-15/photo.jpg already exists"
        );

        let fatal = MoveError::Fatal(anyhow::anyhow!("inner").context("outer"));
        assert_eq!(
            fatal.to_string(),
            "outer: inner",
            "the whole context chain, because the outermost layer never says what went wrong"
        );
    }

    /// The successful paths log too, and a `Display` that panicked while
    /// rendering a path would take the run down *after* the photograph had
    /// moved. Driving them with a subscriber attached is the only way the
    /// suite executes that formatting at all.
    #[test]
    fn the_success_paths_render_their_log_lines() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        let src = input.join("photo.jpg");
        let sidecar = input.join("photo.xmp");
        fs::write(&src, b"BODY").unwrap();
        fs::write(&sidecar, b"<xmp/>").unwrap();

        let planned = plan_with_sidecar(&src, &output.join("photo.jpg"), &sidecar);
        let run = with_logs(|| {
            process_moves(
                std::slice::from_ref(&planned),
                0,
                &mut ScriptedController::new(&[]),
                &mut MoveRecorder::disabled(),
                &no_backlinks(),
            )
        });

        assert_eq!((run.moved, run.sidecars.moved), (1, 1));
        assert!(output.join("photo.xmp").exists());
    }

    /// A failed move logs what it could not do. Same argument as above: the
    /// line is rendered on the path where something has already gone wrong.
    #[test]
    fn a_failed_move_renders_its_log_line() {
        let tmp = TempDir::new().unwrap();
        let planned = plan(
            &tmp.path().join("gone.jpg"),
            &tmp.path().join("output/photo.jpg"),
        );

        let run = with_logs(|| {
            process_moves(
                std::slice::from_ref(&planned),
                0,
                &mut ScriptedController::new(&[]),
                &mut MoveRecorder::disabled(),
                &no_backlinks(),
            )
        });

        assert_eq!((run.moved, run.errors), (0, 1));
    }

    /// A sidecar that moves but whose outcome cannot be recorded is still a
    /// sidecar that moved, and the count has to say so — the alternative is a
    /// summary that under-reports what is now in the output tree.
    #[test]
    fn a_sidecar_that_moved_before_the_journal_refused_is_still_counted() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        let src = input.join("photo.jpg");
        let sidecar = input.join("photo.xmp");
        fs::write(&src, b"BODY").unwrap();
        fs::write(&sidecar, b"<xmp/>").unwrap();

        let planned = plan_with_sidecar(&src, &output.join("photo.jpg"), &sidecar);

        // Three writes accepted — the photograph's intent and outcome, and the
        // sidecar's intent — then the sidecar's outcome is refused.
        let run = with_logs(|| {
            process_moves(
                std::slice::from_ref(&planned),
                0,
                &mut ScriptedController::new(&[]),
                &mut MoveRecorder::failing_after(3),
                &no_backlinks(),
            )
        });

        assert!(run.journal_failed);
        assert_eq!(
            run.sidecars.moved, 1,
            "the sidecar reached the output tree and must be counted"
        );
        assert!(output.join("photo.xmp").exists());
    }

    /// A temp file `link(2)` cannot handle falls back to claim-and-rename,
    /// which is the exFAT/FAT32 route. As with the move path, a directory
    /// returns the same `EPERM` an unsupported filesystem does.
    #[test]
    #[cfg(unix)]
    fn promotion_falls_back_to_claim_and_rename_when_links_are_unsupported() {
        let tmp = TempDir::new().unwrap();
        let temp_dir_as_file = tmp.path().join("temp");
        fs::create_dir(&temp_dir_as_file).unwrap();
        let dst = tmp.path().join("photo.jpg");

        // The fallback claims `dst`, then fails to rename a directory over
        // it — so the assertion is that the claim was cleaned up, which is the
        // property the fallback owes its caller.
        let err = promote_into_place(&temp_dir_as_file, &dst)
            .expect_err("a directory cannot be renamed over a claimed file");

        match err {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("renaming the verified copy"),
                "the claim-and-rename fallback must have been taken; got: {e:#}"
            ),
            MoveError::DestinationExists(p) => panic!("unexpected collision: {}", p.display()),
        }
        assert!(
            !dst.exists(),
            "the placeholder the fallback claimed must not be left behind"
        );
    }

    /// A copy that reports success without writing anything must not be
    /// believed. The verification reads the file back, and there is no file to
    /// read — which has to fail loudly rather than delete the source.
    #[test]
    fn a_copy_that_wrote_nothing_fails_verification_and_spares_the_source() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("photo.jpg");
        fs::write(&src, b"BODY").unwrap();
        let dst = tmp.path().join("output.jpg");

        let err = with_logs(|| {
            copy_verify_delete(&src, &dst, |_, _| {
                Ok(blake3::hash(b"BODY").to_hex().to_string())
            })
            .expect_err("a copy that wrote no file must not pass verification")
        });

        match err {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("verifying the copy"),
                "the failure must name the verification; got: {e:#}"
            ),
            MoveError::DestinationExists(p) => panic!("unexpected collision: {}", p.display()),
        }
        assert_eq!(fs::read(&src).unwrap(), b"BODY", "the source must survive");
        assert!(!dst.exists());
    }

    /// The source is deleted last, and a deletion that fails is reported
    /// rather than swallowed: the file now exists at both ends, and the next
    /// run's dedup pass would otherwise "helpfully" find it as a duplicate of
    /// itself.
    #[test]
    #[cfg(unix)]
    fn a_source_that_cannot_be_removed_after_the_copy_is_reported() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        let output = tmp.path().join("output");
        fs::create_dir_all(&locked).unwrap();
        fs::create_dir_all(&output).unwrap();

        let src = locked.join("photo.jpg");
        fs::write(&src, b"BODY").unwrap();
        let dst = output.join("photo.jpg");

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let applies = permission_bits_apply(&locked);
        let result = if applies {
            Some(with_logs(|| {
                copy_verify_delete(&src, &dst, crate::hasher::copy_hashing)
            }))
        } else {
            None
        };
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let Some(result) = result else {
            eprintln!(
                "SKIPPED a_source_that_cannot_be_removed_after_the_copy_is_reported: writes to a \
                 0o555 directory succeeded, so this process ignores permission bits (running as \
                 root?)"
            );
            return;
        };

        match result.expect_err("a source that will not go away must not report success") {
            MoveError::Fatal(e) => assert!(
                format!("{e:#}").contains("removing source file"),
                "got: {e:#}"
            ),
            MoveError::DestinationExists(p) => panic!("unexpected collision: {}", p.display()),
        }
        assert_eq!(
            fs::read(&dst).unwrap(),
            b"BODY",
            "the copy is verified and in place; only the removal failed"
        );
    }

    /// The trait's defaults are the documented way to drive a run with no
    /// terminal attached — "implements the trait and writes nothing". A
    /// default that did not compile, or that answered `false`, would stop
    /// every such run at the first chunk boundary.
    #[test]
    fn a_controller_that_implements_nothing_runs_every_chunk() {
        struct Silent;
        impl ChunkController for Silent {}

        let tmp = TempDir::new().unwrap();
        let planned = four_planned_moves(tmp.path());

        let run = process_moves(
            &planned,
            1,
            &mut Silent,
            &mut MoveRecorder::disabled(),
            &no_backlinks(),
        );

        assert_eq!(
            (run.moved, run.errors, run.unprocessed),
            (4, 0, 0),
            "four chunks of one file each, none of them declined"
        );
        assert!(!run.stopped_early);
    }
}
