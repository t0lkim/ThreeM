//! The run journal: an append-only record of what a run intended and what it
//! actually did.
//!
//! Everything `mmm` does to somebody's photo library is a move, and a move is
//! only reversible if something wrote down where the file came from *before*
//! the filesystem forgot. That is this module's whole job. The rules it keeps:
//!
//! * **Intent is recorded before the act.** The caller appends a
//!   [`JournalEntry::MoveIntent`], which does not return until the bytes are on
//!   the disk, and only then attempts the move. A run killed between the two
//!   leaves a journal that names a file "possibly moved" rather than a library
//!   with an unrecorded hole in it.
//! * **Durability is per entry, not per run.** [`Journal::append`] calls
//!   `File::sync_data()` before returning. Buffering entries would make the
//!   journal exactly as lossy as the crash it exists to survive.
//! * **A truncated tail is expected, not corrupt.** A run interrupted mid-write
//!   leaves a partial final line. [`Journal::read`] discards it with a warning
//!   and returns every complete entry before it; a bad line anywhere *else* is
//!   real corruption and is reported as an error.
//!
//! The format is JSONL: one [`RunHeader`] on the first line, then one
//! [`JournalEntry`] per line. Line-oriented because appending a line is the
//! only write a crash can leave half-finished in a recoverable way, and because
//! a journal has to stay readable by a person and by `jq` at three in the
//! morning when the tool itself is what is suspect.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::organiser::MoveKind;

/// The version of the on-disk journal format written by this build.
///
/// Bumped whenever an existing field changes meaning or disappears. Adding a
/// new optional field does not need a bump: [`Journal::read`] ignores unknown
/// fields, so an older reader survives a newer writer's additions.
pub const SCHEMA_VERSION: u32 = 1;

/// The first line of every journal: what this run was, and what it was told to
/// do.
///
/// `argv` and `output_dir` are here because "undo this run" is answerable only
/// if the journal says which run it was. A journal found on a disk months later
/// has to explain itself without the shell history that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHeader {
    /// The format version of the lines that follow. See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Sortable run identifier, and the journal's own file stem. See
    /// [`generate_run_id`].
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    /// The `mmm` build that wrote this journal, so a later undo can say which
    /// version's behaviour it is reversing.
    pub mmm_version: String,
    /// The output tree this run organised into.
    pub output_dir: PathBuf,
    /// The command line, verbatim.
    pub argv: Vec<String>,
}

impl RunHeader {
    /// Build a header for a run starting now.
    pub fn new(
        run_id: impl Into<String>,
        output_dir: impl Into<PathBuf>,
        argv: Vec<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            run_id: run_id.into(),
            started_at: Utc::now(),
            mmm_version: env!("CARGO_PKG_VERSION").to_string(),
            output_dir: output_dir.into(),
            argv,
        }
    }

    /// The process's own command line, for [`RunHeader::new`].
    ///
    /// Lossy on purpose: an argument that is not valid UTF-8 is worth recording
    /// approximately, and is not worth failing a run over.
    pub fn current_argv() -> Vec<String> {
        std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}

/// Why a file was being moved.
///
/// The undo path treats the three differently — a duplicate goes back to a path
/// the organiser never planned, and a restore is itself undoable — so the
/// reason has to survive on the disk rather than be inferred from the shape of
/// the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    /// A media file moving into the dated output tree.
    Organise,
    /// A duplicate moving into `duplicates/NNN/`.
    Duplicate,
    /// A file being put back where it came from by `mmm undo`.
    Restore,
}

/// One line of the journal after the header.
///
/// Externally the enum is tagged with a `type` field, so every line is a flat
/// JSON object a human or `jq` can read without knowing the Rust type. Each
/// entry that concerns a single file carries the `seq` of the intent it belongs
/// to: an intent with no matching [`JournalEntry::MoveCommitted`] or
/// [`JournalEntry::MoveFailed`] is precisely the interrupted-mid-rename case,
/// and pairing them by sequence number is how the undo path finds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEntry {
    /// Written and synced *before* the move is attempted.
    ///
    /// `source_size` and the optional `source_hash` are the evidence undo uses
    /// to decide whether the file at the destination is still the file that was
    /// moved there. The hash is optional because the organise path does not
    /// always have one — a unique file skips full hashing — and a size-only
    /// check is still better than restoring blind.
    MoveIntent {
        seq: u64,
        source: PathBuf,
        destination: PathBuf,
        source_size: u64,
        source_hash: Option<String>,
        kind: IntentKind,
    },
    /// Written immediately after a successful move.
    ///
    /// `final_destination` is not always the planned one: collision resolution
    /// can land a file at `photo-1.jpg`. The name the file actually has is the
    /// only one undo can find it by.
    MoveCommitted {
        seq: u64,
        final_destination: PathBuf,
        move_kind: MoveKind,
    },
    /// The move named by `seq` did not happen. The source is still where it was.
    MoveFailed { seq: u64, reason: String },
    /// A duplicate relocated into `duplicates/<group>/`, recorded so undo can
    /// put it back alongside the ordinary moves.
    DuplicateMoved {
        seq: u64,
        group: usize,
        source: PathBuf,
        destination: PathBuf,
    },
    /// The last line of a journal whose run reached an exit path. Its absence
    /// means the run was interrupted.
    RunCompleted {
        moved: usize,
        failed: usize,
        skipped: usize,
        ended_at: DateTime<Utc>,
    },
}

/// An open journal, owned for the length of a run.
///
/// Created with its header already on the disk, so a journal file that exists
/// at all is a journal that can be read.
#[derive(Debug)]
pub struct Journal {
    file: fs::File,
    path: PathBuf,
    run_id: String,
    next_seq: u64,
}

impl Journal {
    /// Create `<dir>/<run_id>.jsonl` and write `header` to it durably.
    ///
    /// The run id is taken from `header.run_id` rather than passed separately:
    /// the file stem and the header have to name the same run, and two
    /// parameters that must agree are one parameter with extra steps.
    ///
    /// # Errors
    ///
    /// Returns an error if `dir` cannot be created, if a journal for this run
    /// already exists, or if the header cannot be written and synced. Every one
    /// of those means the run must not proceed to move files — an unjournalled
    /// move is the thing this module exists to prevent.
    pub fn create(dir: &Path, header: &RunHeader) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating journal directory {}", dir.display()))?;

        let path = dir.join(format!("{}.jsonl", header.run_id));

        // `create_new`: a run id collision must be a refusal, never an
        // overwrite. Appending this run's entries to another run's journal
        // would make both unusable for undo.
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creating journal {}", path.display()))?;

        let mut journal = Self {
            file,
            path,
            run_id: header.run_id.clone(),
            next_seq: 0,
        };
        journal
            .write_line(&serde_json::to_string(header).context("serialising the run header")?)?;

        Ok(journal)
    }

    /// Append one entry and put it on the disk before returning.
    ///
    /// The `sync_data` is the point of the method. Without it the caller's
    /// "record, then move" ordering is a statement about a buffer in this
    /// process rather than about the disk, and survives nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry cannot be serialised, written, or synced.
    /// A caller that cannot record what it is about to do must not do it.
    pub fn append(&mut self, entry: &JournalEntry) -> Result<()> {
        let line = serde_json::to_string(entry).context("serialising a journal entry")?;
        self.write_line(&line)
    }

    /// Write one line and sync it.
    fn write_line(&mut self, line: &str) -> Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');

        // One `write_all` for the line and its terminator: two calls could be
        // interrupted between them, leaving a complete-looking entry with no
        // newline that the next append would then run into.
        self.file
            .write_all(&bytes)
            .with_context(|| format!("appending to journal {}", self.path.display()))?;
        self.file
            .sync_data()
            .with_context(|| format!("flushing journal {} to disk", self.path.display()))
    }

    /// The next sequence number, consumed by taking it.
    ///
    /// The allocator lives here because the sequence is a property of the
    /// journal, not of whichever loop happens to be writing to it — the
    /// organise pass and the duplicate pass both draw from it and must not
    /// collide.
    pub fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Where this journal lives — printed in the run summary, because a user
    /// who does not know the path cannot undo the run.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Read a journal back, tolerating an interrupted final line.
    ///
    /// A partial last line is discarded with a warning: it is what a crash
    /// mid-append leaves behind, and every complete entry before it is still
    /// good. A bad line anywhere else is not recoverable in that way — nothing
    /// truncates the middle of a file — so it is reported as the corruption it
    /// is rather than silently skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, if it has no header line,
    /// if the header does not parse, if it was written by a newer schema
    /// version than this build understands, or if any line other than the last
    /// fails to parse.
    pub fn read(path: &Path) -> Result<(RunHeader, Vec<JournalEntry>)> {
        // Bytes, not `read_to_string`: a run killed mid-write can truncate in
        // the middle of a multi-byte character, and a journal must not become
        // unreadable in its entirety because its last line is half a codepoint.
        let bytes =
            fs::read(path).with_context(|| format!("reading journal {}", path.display()))?;

        let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
        // A complete file ends with a newline, which `split` reports as a final
        // empty element. Drop it so the "is this the last line?" test below
        // means what it says.
        if lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }

        let Some((header_line, entry_lines)) = lines.split_first() else {
            bail!(
                "journal {} is empty — it has no run header, so there is nothing to undo",
                path.display()
            );
        };

        let header: RunHeader = parse_line(header_line)
            .with_context(|| format!("reading the header of journal {}", path.display()))?;

        if header.schema_version > SCHEMA_VERSION {
            bail!(
                "journal {} was written by mmm {} using schema version {}, and this build \
                 understands only up to version {}. Undo it with the version that wrote it \
                 rather than guessing at fields this build does not know.",
                path.display(),
                header.mmm_version,
                header.schema_version,
                SCHEMA_VERSION
            );
        }

        let mut entries = Vec::with_capacity(entry_lines.len());
        for (index, line) in entry_lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }

            let is_last = index + 1 == entry_lines.len();
            match parse_line::<JournalEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(err) if is_last => {
                    warn!(
                        journal = %path.display(),
                        error = %format!("{err:#}"),
                        "discarding a truncated final journal line — the run was interrupted \
                         while writing it; every complete entry before it has been recovered"
                    );
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "journal {} is corrupt at line {} of {} — only a truncated *final* \
                             line is recoverable",
                            path.display(),
                            index + 2,
                            lines.len()
                        )
                    })
                }
            }
        }

        Ok((header, entries))
    }
}

/// The file extension every journal carries.
const JOURNAL_EXTENSION: &str = "jsonl";

/// Where the journal of run `run_id` lives inside `dir`.
///
/// The inverse of [`Journal::create`]'s naming, stated once so `undo --run`
/// cannot look for a file under a name nothing writes.
pub fn journal_path(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(format!("{run_id}.{JOURNAL_EXTENSION}"))
}

/// The run id a journal file is named for.
pub fn run_id_of(path: &Path) -> Option<String> {
    path.file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// Every journal in `dir`, newest first.
///
/// Sorted by file name and reversed rather than by mtime: the run id *is* a
/// timestamp, so this is chronological by construction and stays right when a
/// library is copied to another disk and every mtime becomes the copy's.
///
/// A directory that does not exist is an empty list, not an error: `.mmm/journal`
/// is created by the first committing run, so its absence means no such run has
/// happened here. That is a fact about the library, and the caller has a better
/// sentence for it than this function does. Anything else — a denied read, a
/// broken mount — really is "could not look" and is reported as such.
///
/// # Errors
///
/// Returns an error if `dir` exists but cannot be read.
pub fn journals_newest_first(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("reading the journal directory {}", dir.display())))
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == JOURNAL_EXTENSION))
        .collect();

    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(paths)
}

/// Parse one JSONL line into `T`.
fn parse_line<T: DeserializeOwned>(line: &[u8]) -> Result<T> {
    let text = std::str::from_utf8(line)
        .context("journal line is not valid UTF-8 (truncated mid-character?)")?;
    serde_json::from_str(text).with_context(|| format!("parsing journal line: {}", elide(text)))
}

/// Trim a line for an error message. A truncated JSON line can be the whole
/// remainder of a large entry, and an operator reading a failure does not need
/// all of it.
fn elide(text: &str) -> String {
    const LIMIT: usize = 160;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let cut = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= LIMIT)
        .last()
        .unwrap_or(0);
    format!("{}… ({} bytes)", &text[..cut], text.len())
}

/// A sortable, unique run identifier: `YYYYMMDD-HHMMSS-<six base36 chars>`.
///
/// Sortable because the journal directory is browsed by a human looking for
/// "the run I did this morning", and lexical order over these strings is
/// chronological order. Random-suffixed because the second is not fine enough
/// to separate two runs started together, and a collision would mean one run's
/// journal refusing to be created (see [`Journal::create`]) or, worse, two runs
/// sharing one.
///
/// The randomness comes from `RandomState`, which the standard library seeds
/// from the OS. That is deliberate: a UUID crate would be a dependency, and a
/// timestamp-derived "random" suffix would be no more unique than the timestamp
/// it came from.
pub fn generate_run_id() -> String {
    format!("{}-{}", Utc::now().format("%Y%m%d-%H%M%S"), short_random())
}

/// Six base36 characters of entropy.
fn short_random() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Distinguishes two ids generated within the same nanosecond by the same
    /// process, which the hasher seed alone is not guaranteed to do.
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    const LENGTH: usize = 6;

    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(SEQUENCE.fetch_add(1, Ordering::Relaxed));
    hasher.write_i64(Utc::now().timestamp_nanos_opt().unwrap_or_default());
    let mut value = hasher.finish();

    let mut out = String::with_capacity(LENGTH);
    for _ in 0..LENGTH {
        let index = usize::try_from(value % 36).unwrap_or(0);
        out.push(char::from(ALPHABET[index]));
        value /= 36;
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use tempfile::TempDir;

    fn header(run_id: &str) -> RunHeader {
        RunHeader::new(
            run_id,
            "/photos",
            vec!["mmm".to_string(), "/photos".to_string()],
        )
    }

    fn intent(seq: u64) -> JournalEntry {
        JournalEntry::MoveIntent {
            seq,
            source: PathBuf::from("/photos/IMG_0001.JPG"),
            destination: PathBuf::from("/photos/2024-03-15/2024-03-15-103000.jpg"),
            source_size: 1234,
            source_hash: Some("abc123".to_string()),
            kind: IntentKind::Organise,
        }
    }

    fn committed(seq: u64) -> JournalEntry {
        JournalEntry::MoveCommitted {
            seq,
            final_destination: PathBuf::from("/photos/2024-03-15/2024-03-15-103000-1.jpg"),
            move_kind: MoveKind::Renamed,
        }
    }

    #[test]
    fn a_created_journal_is_named_for_its_run() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".mmm/journal");
        let journal = Journal::create(&dir, &header("20240315-103000-abc123")).unwrap();

        assert_eq!(journal.path(), dir.join("20240315-103000-abc123.jsonl"));
        assert_eq!(journal.run_id(), "20240315-103000-abc123");
        assert!(
            journal.path().exists(),
            "the directory should have been created along with the file"
        );
    }

    #[test]
    fn a_journal_round_trips_every_entry_type() {
        let tmp = TempDir::new().unwrap();
        let written = header("20240315-103000-abc123");
        let mut journal = Journal::create(tmp.path(), &written).unwrap();

        let entries = vec![
            intent(0),
            committed(0),
            JournalEntry::MoveFailed {
                seq: 1,
                reason: "destination filesystem is full".to_string(),
            },
            JournalEntry::DuplicateMoved {
                seq: 2,
                group: 7,
                source: PathBuf::from("/photos/copy.jpg"),
                destination: PathBuf::from("/photos/duplicates/007/copy.jpg"),
            },
            JournalEntry::RunCompleted {
                moved: 1,
                failed: 1,
                skipped: 3,
                ended_at: Utc::now(),
            },
        ];
        for entry in &entries {
            journal.append(entry).unwrap();
        }

        let (read_header, read_entries) = Journal::read(journal.path()).unwrap();
        assert_eq!(read_header, written);
        assert_eq!(read_entries, entries);
    }

    /// The header is the first line, and it says which schema wrote the rest.
    #[test]
    fn the_header_is_the_first_line() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(0)).unwrap();

        let text = fs::read_to_string(journal.path()).unwrap();
        let first = text.lines().next().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(first).unwrap();

        assert_eq!(parsed["schema_version"], SCHEMA_VERSION);
        assert_eq!(parsed["run_id"], "20240315-103000-abc123");
        assert_eq!(parsed["mmm_version"], env!("CARGO_PKG_VERSION"));
    }

    /// Every line is a self-describing flat object. The tag is what lets a
    /// person — or `jq` — read a journal without this crate.
    #[test]
    fn entries_are_tagged_flat_json_objects() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(4)).unwrap();
        journal.append(&committed(4)).unwrap();

        let text = fs::read_to_string(journal.path()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(lines[0]["type"], "move_intent");
        assert_eq!(lines[0]["seq"], 4);
        assert_eq!(lines[0]["kind"], "organise");
        assert_eq!(lines[1]["type"], "move_committed");
        assert_eq!(lines[1]["move_kind"], "renamed");
    }

    /// `append` must not return before the entry is on the disk. Reading the
    /// file through a second handle while the journal is still open is the
    /// closest a test can get to that without pulling the power: it proves the
    /// entry is not sitting in this process's buffer.
    #[test]
    fn an_appended_entry_is_visible_before_the_journal_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(0)).unwrap();

        let mut text = String::new();
        fs::File::open(journal.path())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();

        assert_eq!(
            text.lines().count(),
            2,
            "the header and the entry should both be on disk while the journal is still open"
        );
        drop(journal);
    }

    /// The interrupted-run case: the process died part-way through writing the
    /// last line. Everything before it is still good, and the partial line is
    /// dropped rather than failing the whole read.
    #[test]
    fn a_truncated_final_line_is_discarded_and_the_rest_recovered() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(0)).unwrap();
        journal.append(&committed(0)).unwrap();
        journal.append(&intent(1)).unwrap();
        let path = journal.path().to_path_buf();
        drop(journal);

        // Cut the file in the middle of its last line.
        let full = fs::read(&path).unwrap();
        let last_newline = full
            .iter()
            .rposition(|&b| b == b'\n')
            .expect("the file ends with a newline");
        let previous_newline = full[..last_newline]
            .iter()
            .rposition(|&b| b == b'\n')
            .expect("there are at least three lines");
        let cut = previous_newline + (last_newline - previous_newline) / 2;
        fs::write(&path, &full[..cut]).unwrap();

        let (read_header, entries) = Journal::read(&path).unwrap();

        assert_eq!(read_header.run_id, "20240315-103000-abc123");
        assert_eq!(
            entries,
            vec![intent(0), committed(0)],
            "the two complete entries survive; the half-written third does not"
        );
    }

    /// Truncation can land in the middle of a multi-byte character. That must
    /// still be a discarded line, not an unreadable journal.
    #[test]
    fn a_line_truncated_mid_character_is_discarded_too() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(0)).unwrap();
        journal
            .append(&JournalEntry::MoveFailed {
                seq: 1,
                // Three bytes each; cutting one byte short of the end lands
                // inside the last character.
                reason: "café … ☂".to_string(),
            })
            .unwrap();
        let path = journal.path().to_path_buf();
        drop(journal);

        let full = fs::read(&path).unwrap();
        fs::write(&path, &full[..full.len() - 3]).unwrap();

        let (_, entries) = Journal::read(&path).unwrap();
        assert_eq!(entries, vec![intent(0)]);
    }

    /// A file that ends cleanly loses nothing — the "is this the last line?"
    /// tolerance must not eat a complete final entry.
    #[test]
    fn a_complete_final_line_is_kept() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(0)).unwrap();
        journal.append(&committed(0)).unwrap();

        let (_, entries) = Journal::read(journal.path()).unwrap();
        assert_eq!(entries, vec![intent(0), committed(0)]);
    }

    /// Nothing truncates the middle of a file. A bad line there is corruption,
    /// and reporting it as such is the difference between "your undo is
    /// incomplete" and a silent partial restore.
    #[test]
    fn a_corrupt_middle_line_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        journal.append(&intent(0)).unwrap();
        journal.append(&committed(0)).unwrap();
        let path = journal.path().to_path_buf();
        drop(journal);

        let text = fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "{ this is not json";
        fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let err = Journal::read(&path).expect_err("a corrupt middle line must not be swallowed");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("corrupt"), "{rendered}");
        assert!(rendered.contains("line 2"), "{rendered}");
    }

    #[test]
    fn an_empty_journal_has_no_header_and_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        fs::write(&path, b"").unwrap();

        let err = Journal::read(&path).expect_err("a headerless journal is not a journal");
        assert!(format!("{err:#}").contains("no run header"), "{err:#}");
    }

    /// A journal whose header itself is half-written is unrecoverable — there
    /// is no run to attribute the entries to.
    #[test]
    fn a_truncated_header_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("partial.jsonl");
        fs::write(&path, br#"{"schema_version":1,"run_id":"202403"#).unwrap();

        let err = Journal::read(&path).expect_err("a truncated header cannot be recovered");
        assert!(format!("{err:#}").contains("header"), "{err:#}");
    }

    /// Guessing at a format this build does not know is how an undo deletes
    /// something. Refuse instead, and name the version that can read it.
    #[test]
    fn a_newer_schema_version_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("future.jsonl");
        let mut future = header("20240315-103000-abc123");
        future.schema_version = SCHEMA_VERSION + 1;
        future.mmm_version = "9.9.9".to_string();
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&future).unwrap()),
        )
        .unwrap();

        let err = Journal::read(&path).expect_err("a future schema must be refused");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("schema version"), "{rendered}");
        assert!(rendered.contains("9.9.9"), "{rendered}");
    }

    /// Reusing a run id would either destroy another run's journal or blend two
    /// runs into one unusable record. Refuse.
    #[test]
    fn creating_a_second_journal_for_the_same_run_refuses() {
        let tmp = TempDir::new().unwrap();
        let first = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();
        drop(first);

        let err = Journal::create(tmp.path(), &header("20240315-103000-abc123"))
            .expect_err("an existing journal must not be reopened or overwritten");
        assert!(format!("{err:#}").contains("creating journal"), "{err:#}");
    }

    #[test]
    fn sequence_numbers_are_handed_out_once_each() {
        let tmp = TempDir::new().unwrap();
        let mut journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();

        let seqs: Vec<u64> = (0..5).map(|_| journal.next_seq()).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_run_id_is_sortable_and_shaped_as_documented() {
        let id = generate_run_id();
        let parts: Vec<&str> = id.split('-').collect();

        assert_eq!(parts.len(), 3, "{id}");
        assert_eq!(parts[0].len(), 8, "YYYYMMDD: {id}");
        assert_eq!(parts[1].len(), 6, "HHMMSS: {id}");
        assert_eq!(parts[2].len(), 6, "random suffix: {id}");
        assert!(
            parts[0]
                .chars()
                .chain(parts[1].chars())
                .all(|c| c.is_ascii_digit()),
            "the sortable part must be all digits: {id}"
        );
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "the suffix must be base36: {id}"
        );
    }

    /// Lexical order over run ids is chronological order — the property the
    /// whole `YYYYMMDD-HHMMSS` shape exists for, since `journal list` sorts by
    /// it rather than by mtime.
    #[test]
    fn run_ids_sort_chronologically() {
        let earlier = format!("20240315-103000-{}", short_random());
        let later = format!("20240315-103001-{}", short_random());
        let next_day = format!("20240316-000000-{}", short_random());

        let mut ids = vec![next_day.clone(), later.clone(), earlier.clone()];
        ids.sort();
        assert_eq!(ids, vec![earlier, later, next_day]);
    }

    /// Two runs starting in the same second must not collide — the suffix is
    /// the only thing standing between them and a refused `create`.
    #[test]
    fn run_ids_generated_together_are_distinct() {
        let ids: std::collections::HashSet<String> = (0..256).map(|_| generate_run_id()).collect();
        assert_eq!(ids.len(), 256, "run ids collided within one second");
    }

    /// `undo --run <id>` has to find the file `create` wrote. Naming it in two
    /// places would be two chances to disagree.
    #[test]
    fn a_journal_is_found_under_the_name_it_was_created_with() {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::create(tmp.path(), &header("20240315-103000-abc123")).unwrap();

        assert_eq!(
            journal_path(tmp.path(), "20240315-103000-abc123"),
            journal.path()
        );
        assert_eq!(
            run_id_of(journal.path()).as_deref(),
            Some("20240315-103000-abc123")
        );
    }

    #[test]
    fn journals_are_listed_newest_first() {
        let tmp = TempDir::new().unwrap();
        for run_id in [
            "20240315-103000-aaaaaa",
            "20240316-090000-bbbbbb",
            "20240315-110000-cccccc",
        ] {
            drop(Journal::create(tmp.path(), &header(run_id)).unwrap());
        }
        // Something that is not a journal, which the listing must ignore.
        fs::write(tmp.path().join("notes.txt"), b"not a journal").unwrap();

        let listed: Vec<String> = journals_newest_first(tmp.path())
            .unwrap()
            .iter()
            .filter_map(|p| run_id_of(p))
            .collect();

        assert_eq!(
            listed,
            vec![
                "20240316-090000-bbbbbb",
                "20240315-110000-cccccc",
                "20240315-103000-aaaaaa",
            ]
        );
    }

    /// The journal directory is created by the first committing run, so its
    /// absence is not a failure to look — it is the answer. `mmm undo` in a
    /// library nobody has organised should say "nothing to undo", not report an
    /// io error at the operator.
    #[test]
    fn a_library_that_was_never_organised_lists_no_runs() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            journals_newest_first(&tmp.path().join("never-run")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
