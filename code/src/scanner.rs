use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use thiserror::Error;
use tracing::{debug, warn};
use walkdir::{DirEntry, WalkDir};

use crate::METADATA_DIR_NAME;

/// Known image extensions (lowercase, no dot)
///
/// Public because [`crate::settings`] defaults to exactly this list, and a
/// second copy of it over there would be a second thing to remember to update
/// the day a new RAW format appears.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "tiff", "tif", "raw", "cr2", "cr3", "nef", "arw", "dng",
    "orf", "rw2", "raf", "srw", "pef", "webp", "avif", "bmp",
];

/// Known video extensions (lowercase, no dot)
///
/// Public for the same reason as [`IMAGE_EXTENSIONS`].
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mov", "mp4", "m4v", "avi", "mkv", "wmv", "flv", "webm", "3gp", "mts", "m2ts",
];

/// A `skip_patterns` entry that is not a glob.
///
/// Refused where it was written rather than passed over, for the reason
/// `deny_unknown_fields` exists one layer up: a pattern that silently matches
/// nothing is indistinguishable from a setting that does not work, and the
/// person who typed it would find out by wondering why their `.thumbnails`
/// directory keeps being organised.
#[derive(Debug, Error)]
#[error("`skip_patterns` entry {pattern:?} is not a valid glob: {message}")]
pub struct PatternError {
    pub pattern: String,
    pub message: String,
}

/// What the scan admits, and what it passes over.
///
/// Built once from the resolved [`crate::settings::Settings`] and handed to
/// [`scan_directories`], so the extensions a run recognises and the paths it
/// skips are decided in exactly one place.
///
/// # How a skip pattern is matched
///
/// * A pattern with **no `/`** is matched against each path component's own
///   name — `*.tmp` skips those files anywhere in the tree, and `node_modules`
///   or `.thumbnails` prunes that directory wherever it appears. This is the
///   form people reach for, and matching it against the whole path instead
///   would make the common case silently match nothing.
/// * A pattern **containing `/`** is matched against the path relative to the
///   scan root it was found under — `raw/**` skips one subtree and nothing with
///   the same name elsewhere. `*` does not cross a separator; `**` does.
///
/// A matching directory is not descended into, so a skipped tree costs nothing
/// to walk.
#[derive(Debug)]
pub struct ScanFilter {
    image: HashSet<String>,
    video: HashSet<String>,
    /// Patterns matched against one component's name.
    names: GlobSet,
    /// Patterns matched against the path relative to the scan root.
    paths: GlobSet,
}

impl Default for ScanFilter {
    /// The built-in extensions, skipping nothing — what a run with no config
    /// does.
    fn default() -> Self {
        Self {
            image: IMAGE_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
            video: VIDEO_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
            names: GlobSet::empty(),
            paths: GlobSet::empty(),
        }
    }
}

impl ScanFilter {
    /// Build a filter from the resolved settings' extension lists and skip
    /// patterns.
    ///
    /// Extensions are lowercased here rather than trusted, because the scanner
    /// compares against a lowercased extension and a config file saying `"JPG"`
    /// would otherwise match nothing at all.
    ///
    /// # Errors
    ///
    /// [`PatternError`] naming the first skip pattern that is not a glob.
    pub fn new(
        image: &[String],
        video: &[String],
        skip_patterns: &[String],
    ) -> Result<Self, PatternError> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();

        for pattern in skip_patterns {
            let glob = compile(pattern)?;
            if pattern.contains('/') {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }

        Ok(Self {
            image: image.iter().map(|e| e.to_lowercase()).collect(),
            video: video.iter().map(|e| e.to_lowercase()).collect(),
            names: build(&names, skip_patterns)?,
            paths: build(&paths, skip_patterns)?,
        })
    }

    /// Whether `name` — one path component — is skipped outright.
    fn skips_name(&self, name: &OsStr) -> bool {
        self.names.is_match(name)
    }

    /// Whether `path` is skipped by a pattern written with a separator in it.
    ///
    /// `relative` is the path below the scan root; a path that is somehow not
    /// below it is not matched rather than being matched against an absolute
    /// path a pattern could never have meant.
    fn skips_path(&self, relative: &Path) -> bool {
        self.paths.is_match(relative)
    }

    /// The kind of media `extension` names, if it names one.
    fn kind(&self, extension: &str) -> Option<MediaKind> {
        if self.image.contains(extension) {
            Some(MediaKind::Image)
        } else if self.video.contains(extension) {
            Some(MediaKind::Video)
        } else {
            None
        }
    }
}

/// Which of the two extension lists admitted a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

/// Compile one pattern, naming it if it will not compile.
fn compile(pattern: &str) -> Result<Glob, PatternError> {
    // `literal_separator` is what makes `*` stop at a `/`: without it,
    // `raw/*` would match `raw/2024/a.jpg` and a pattern meant for one
    // directory would take a whole subtree with it.
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| PatternError {
            pattern: pattern.to_string(),
            message: error.to_string(),
        })
}

/// Finish a set, attributing a failure to the patterns that went into it.
fn build(builder: &GlobSetBuilder, patterns: &[String]) -> Result<GlobSet, PatternError> {
    builder.build().map_err(|error| PatternError {
        pattern: patterns.join(", "),
        message: error.to_string(),
    })
}

/// A discovered media file with basic filesystem metadata
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub size: u64,
    pub extension: String,
    pub is_video: bool,
}

/// What a scan found, and what it had to pass over.
///
/// One unreadable directory used to abort the entire walk, so a single
/// permission-denied folder could hide a whole photo library behind an error
/// about that folder. The rest of the tree is worth returning.
///
/// `skipped` is not decoration. It is the difference between "there was
/// nothing else there" and "we could not look", and those two must never
/// arrive at the operator looking the same — [`crate::reporter::print_summary`]
/// prints it for exactly that reason.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Media files successfully discovered, in filesystem walk order.
    pub files: Vec<ScannedFile>,
    /// Entries inside the scanned trees that could not be read and were
    /// passed over — a directory the walk could not descend into, or a file
    /// whose metadata could not be read. Each one is logged at `warn`.
    ///
    /// A caller-supplied path that is not a directory is *not* counted here:
    /// it is a distinct condition, loud in its own right, and mixing it into
    /// this figure would make the number mean two things at once.
    pub skipped: usize,
    /// Entries a `skip_patterns` entry matched — files passed over, and
    /// directories the walk did not descend into (one each, whatever they
    /// hold).
    ///
    /// Separate from `skipped` for the reason that field's own comment gives:
    /// "we could not look" and "you told us not to" are different answers, and
    /// one number meaning both would tell the operator neither. Reported so a
    /// pattern that is quietly excluding a whole library is visible rather than
    /// inferred from a count that came out lower than expected.
    pub excluded: usize,
}

/// Whether the walk should refuse to descend into this entry.
///
/// `mmm` writes its journals into `<output>/.mmm/`, and organising a library
/// in place makes that directory a subdirectory of an input tree. Descending
/// into it would put the tool's own record of the run in front of the code
/// that moves files around — the one directory a media organiser must never
/// treat as media.
///
/// The test is on the directory's *name*, at any depth, rather than on one
/// resolved path: a run with several input directories, or one organised into
/// a tree it also scans, has more than one `.mmm/` to avoid.
///
/// A `.mmm` passed on the command line as a root is honoured. Naming it
/// explicitly is a deliberate act, and the exclusion exists to stop the walk
/// wandering into metadata it was never asked about, not to overrule the
/// operator.
fn is_excluded(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry.file_name() == OsStr::new(METADATA_DIR_NAME)
}

/// Whether `filter` says to pass this entry over.
///
/// The root itself is exempt, as it is for `.mmm/`: a directory named on the
/// command line was pointed at deliberately, and a pattern that happened to
/// match its name would silently scan nothing at all.
fn is_skipped(entry: &DirEntry, root: &Path, filter: &ScanFilter) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    if filter.skips_name(entry.file_name()) {
        return true;
    }
    entry
        .path()
        .strip_prefix(root)
        .is_ok_and(|relative| filter.skips_path(relative))
}

/// Scan one or more directories recursively for media files.
///
/// Infallible by construction. Every per-entry failure is a warning and a
/// skip, never an abort, because the caller is about to organise whatever
/// comes back and "the scan died two directories in" must not be
/// indistinguishable from "that is all there was".
///
/// What counts as media, and what is passed over, is `filter`'s business —
/// see [`ScanFilter`]. Pass [`ScanFilter::default`] for the built-in lists.
pub fn scan_directories(dirs: &[PathBuf], filter: &ScanFilter) -> ScanResult {
    let mut files = Vec::new();
    let mut skipped = 0;
    // A `Cell` because the count is incremented from inside `filter_entry`'s
    // closure, which is where a pruned directory is decided and the only place
    // it is visible — a directory that is never descended into produces no
    // entry to count later.
    let excluded = Cell::new(0usize);

    for dir in dirs {
        if !dir.is_dir() {
            warn!("skipping non-directory path: {}", dir.display());
            continue;
        }

        for entry in WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if is_excluded(entry) {
                    return false;
                }
                if is_skipped(entry, dir, filter) {
                    debug!(path = %entry.path().display(), "skipped by skip_patterns");
                    excluded.set(excluded.get() + 1);
                    return false;
                }
                true
            })
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warn!(
                        root = %dir.display(),
                        error = %e,
                        "skipping an entry the scan could not read"
                    );
                    skipped += 1;
                    continue;
                }
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let Some(ext) = normalised_extension(path) else {
                continue;
            };

            let Some(kind) = filter.kind(&ext) else {
                continue;
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping a media file whose metadata could not be read"
                    );
                    skipped += 1;
                    continue;
                }
            };

            debug!(path = %path.display(), size = metadata.len(), "found media file");

            files.push(ScannedFile {
                path: path.to_path_buf(),
                size: metadata.len(),
                extension: ext,
                is_video: kind == MediaKind::Video,
            });
        }
    }

    ScanResult {
        files,
        skipped,
        excluded: excluded.get(),
    }
}

fn normalised_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_scan_finds_jpeg() {
        let tmp = TempDir::new().unwrap();
        let jpg = tmp.path().join("photo.jpg");
        fs::write(&jpg, b"fake jpeg data").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].extension, "jpg");
        assert!(!scan.files[0].is_video);
        assert_eq!(scan.skipped, 0);
    }

    #[test]
    fn test_scan_finds_video() {
        let tmp = TempDir::new().unwrap();
        let mov = tmp.path().join("clip.mov");
        fs::write(&mov, b"fake mov data").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());
        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].is_video);
    }

    #[test]
    fn test_scan_skips_non_media() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("readme.txt"), b"text").unwrap();
        fs::write(tmp.path().join("doc.pdf"), b"pdf").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());
        assert!(scan.files.is_empty());
        assert_eq!(
            scan.skipped, 0,
            "a file the scanner is not interested in was not skipped, it was never a candidate"
        );
    }

    #[test]
    fn test_scan_recursive() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("deep.png"), b"png data").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());
        assert_eq!(scan.files.len(), 1);
    }

    #[test]
    fn test_scan_multiple_dirs() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        fs::write(tmp1.path().join("a.jpg"), b"data").unwrap();
        fs::write(tmp2.path().join("b.mp4"), b"data").unwrap();

        let scan = scan_directories(
            &[tmp1.path().to_path_buf(), tmp2.path().to_path_buf()],
            &ScanFilter::default(),
        );
        assert_eq!(scan.files.len(), 2);
    }

    #[test]
    fn test_extension_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo.JPG"), b"data").unwrap();
        fs::write(tmp.path().join("clip.MOV"), b"data").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());
        assert_eq!(scan.files.len(), 2);
    }

    /// `mmm`'s own metadata directory is not media, however photo-shaped its
    /// contents look. A run organising a library in place writes `.mmm/` into
    /// the tree it is scanning; a scanner that walked back into it would offer
    /// to organise the record of the run that made it.
    #[test]
    fn test_scan_skips_the_metadata_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("real.jpg"), b"data").unwrap();

        let journal = tmp.path().join(".mmm").join("journal");
        fs::create_dir_all(&journal).unwrap();
        fs::write(journal.join("20240315-103000-abc123.jsonl"), b"{}\n").unwrap();
        // A media file *inside* .mmm is the case that matters — the extension
        // filter alone would already have passed over the .jsonl.
        fs::write(journal.join("thumbnail.jpg"), b"data").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());

        assert_eq!(
            scan.files.len(),
            1,
            "only the real photo should be found; got {:?}",
            scan.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert!(scan.files[0].path.ends_with("real.jpg"));
        assert_eq!(
            scan.skipped, 0,
            "an excluded directory is not an unreadable one, and must not inflate the \
             count that tells the operator something could not be looked at"
        );
    }

    /// The exclusion is by name at any depth, not by one precomputed path: a
    /// library organised in place can carry `.mmm/` well below the roots given
    /// on the command line.
    #[test]
    fn test_scan_skips_a_nested_metadata_directory() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("2024").join("holiday");
        fs::create_dir_all(nested.join(".mmm").join("journal")).unwrap();
        fs::write(nested.join("beach.jpg"), b"data").unwrap();
        fs::write(nested.join(".mmm").join("journal").join("stale.png"), b"x").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());

        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].path.ends_with("beach.jpg"));
    }

    /// Naming `.mmm` on the command line is a deliberate act. The exclusion
    /// stops the walk *wandering* into metadata, and must not overrule an
    /// operator who pointed at it on purpose.
    #[test]
    fn test_an_explicitly_named_metadata_directory_is_still_scanned() {
        let tmp = TempDir::new().unwrap();
        let meta = tmp.path().join(".mmm");
        fs::create_dir(&meta).unwrap();
        fs::write(meta.join("recovered.jpg"), b"data").unwrap();

        let scan = scan_directories(&[meta], &ScanFilter::default());
        assert_eq!(scan.files.len(), 1);
    }

    // -----------------------------------------------------------------
    // ScanFilter: extensions
    // -----------------------------------------------------------------

    /// A configured extension list *replaces* the built-in one, which is the
    /// merge rule `PartialSettings` documents. A user who lists two formats
    /// wants those two, not those two and twenty more.
    #[test]
    fn a_configured_extension_list_replaces_the_built_in_one() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("keep.dng"), b"data").unwrap();
        fs::write(tmp.path().join("drop.jpg"), b"data").unwrap();

        let filter = ScanFilter::new(&["dng".to_string()], &[], &[]).unwrap();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &filter);

        assert_eq!(scan.files.len(), 1, "got {:?}", scan.files);
        assert!(scan.files[0].path.ends_with("keep.dng"));
    }

    /// A file's extension is lowercased before the comparison, so the list has
    /// to be too — otherwise `image = ["JPG"]` would silently match nothing and
    /// read as a scanner that cannot see photographs.
    #[test]
    fn a_configured_extension_is_matched_case_insensitively() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo.JPG"), b"data").unwrap();

        let filter = ScanFilter::new(&["JPG".to_string()], &[], &[]).unwrap();
        assert_eq!(
            scan_directories(&[tmp.path().to_path_buf()], &filter)
                .files
                .len(),
            1
        );
    }

    /// Which list an extension came from decides how its date is read later, so
    /// a configured video extension has to arrive as a video.
    #[test]
    fn a_configured_video_extension_is_scanned_as_video() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("clip.insv"), b"data").unwrap();

        let filter = ScanFilter::new(&[], &["insv".to_string()], &[]).unwrap();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &filter);

        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].is_video);
    }

    // -----------------------------------------------------------------
    // ScanFilter: skip_patterns
    // -----------------------------------------------------------------

    /// The form people write: a bare glob, matched against a file's own name
    /// anywhere in the tree.
    #[test]
    fn a_bare_pattern_skips_matching_files_at_any_depth() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("holiday")).unwrap();
        fs::write(tmp.path().join("keep.jpg"), b"data").unwrap();
        fs::write(tmp.path().join("holiday/thumb_small.jpg"), b"data").unwrap();

        let filter =
            ScanFilter::new(&["jpg".to_string()], &[], &["thumb_*.jpg".to_string()]).unwrap();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &filter);

        assert_eq!(scan.files.len(), 1, "got {:?}", scan.files);
        assert!(scan.files[0].path.ends_with("keep.jpg"));
        assert_eq!(scan.excluded, 1);
        assert_eq!(
            scan.skipped, 0,
            "a file the operator asked to pass over is not one the scan could not read"
        );
    }

    /// A directory the patterns match is not descended into, which is the case
    /// the setting is actually for — a cache or an export folder somebody does
    /// not want organised.
    #[test]
    fn a_matching_directory_is_pruned_rather_than_walked() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join(".thumbnails");
        fs::create_dir_all(cache.join("nested")).unwrap();
        fs::write(cache.join("a.jpg"), b"data").unwrap();
        fs::write(cache.join("nested/b.jpg"), b"data").unwrap();
        fs::write(tmp.path().join("real.jpg"), b"data").unwrap();

        let filter =
            ScanFilter::new(&["jpg".to_string()], &[], &[".thumbnails".to_string()]).unwrap();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &filter);

        assert_eq!(scan.files.len(), 1, "got {:?}", scan.files);
        assert!(scan.files[0].path.ends_with("real.jpg"));
        assert_eq!(
            scan.excluded, 1,
            "a pruned directory counts once, whatever it holds — the walk never enumerated it"
        );
    }

    /// A pattern with a separator is anchored to the scan root, so it can name
    /// one subtree without taking every directory of that name with it.
    #[test]
    fn a_pattern_with_a_separator_is_relative_to_the_scan_root() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("raw/2024")).unwrap();
        fs::create_dir_all(tmp.path().join("holiday/raw")).unwrap();
        fs::write(tmp.path().join("raw/2024/a.jpg"), b"data").unwrap();
        fs::write(tmp.path().join("holiday/raw/b.jpg"), b"data").unwrap();

        let filter = ScanFilter::new(&["jpg".to_string()], &[], &["raw/**".to_string()]).unwrap();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &filter);

        assert_eq!(
            scan.files.len(),
            1,
            "only the top-level raw/ subtree should be skipped; got {:?}",
            scan.files
        );
        assert!(scan.files[0].path.ends_with("holiday/raw/b.jpg"));
    }

    /// `*` stops at a separator: `raw/*.jpg` is a statement about the files
    /// directly inside `raw/`, and the subdirectory beside them is untouched.
    ///
    /// Note what `raw/*` on its own would do instead — it matches the *directory*
    /// `raw/2024` as well, and a matched directory is pruned, so it takes the
    /// subtree with it. That is the gitignore reading and it is the one people
    /// expect; this test pins the narrower pattern, where the difference is
    /// observable.
    #[test]
    fn a_star_does_not_cross_a_separator() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("raw/2024")).unwrap();
        fs::write(tmp.path().join("raw/top.jpg"), b"data").unwrap();
        fs::write(tmp.path().join("raw/2024/deep.jpg"), b"data").unwrap();

        let filter =
            ScanFilter::new(&["jpg".to_string()], &[], &["raw/*.jpg".to_string()]).unwrap();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &filter);

        assert_eq!(scan.files.len(), 1, "got {:?}", scan.files);
        assert!(
            scan.files[0].path.ends_with("2024/deep.jpg"),
            "the file one level down is not what `raw/*.jpg` named"
        );
    }

    /// A root named on the command line was pointed at deliberately. A pattern
    /// that happened to match its name must not turn the whole run into a scan
    /// of nothing.
    #[test]
    fn a_pattern_cannot_skip_the_root_it_was_pointed_at() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("photo.jpg"), b"data").unwrap();

        let filter = ScanFilter::new(&["jpg".to_string()], &[], &["cache".to_string()]).unwrap();
        let scan = scan_directories(&[root], &filter);

        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.excluded, 0);
    }

    /// A pattern that will not compile is refused where it was written, not
    /// dropped: a skip that silently matches nothing is indistinguishable from
    /// a setting that does not work.
    #[test]
    fn a_pattern_that_is_not_a_glob_is_refused_and_named() {
        let error = ScanFilter::new(&[], &[], &["[unclosed".to_string()]).unwrap_err();
        assert_eq!(error.pattern, "[unclosed");
        assert!(
            error.to_string().contains("skip_patterns"),
            "the refusal must name the setting: {error}"
        );
    }

    /// One directory the walk cannot descend into must cost that directory
    /// and nothing else.
    ///
    /// Before this, `WalkDir`'s error went out through `?` and the caller got
    /// *no* files at all — the readable sibling below would have been invisible
    /// behind "Permission denied" on a folder the user may not even care about.
    ///
    /// Skips itself with a printed reason where permission bits do not deny
    /// reads (running as root, as some CI containers do).
    #[cfg(unix)]
    #[test]
    fn test_an_unreadable_directory_costs_one_entry_not_the_whole_scan() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("readable.jpg"), b"visible").unwrap();
        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden.jpg"), b"invisible").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let readable = fs::read_dir(&locked).is_ok();
        let scan = scan_directories(&[tmp.path().to_path_buf()], &ScanFilter::default());

        // Restore before asserting, or `TempDir` cannot clean up after a panic.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        if readable {
            eprintln!(
                "SKIPPED test_an_unreadable_directory_costs_one_entry_not_the_whole_scan: \
                 a 0o000 directory was still readable, so this process ignores permission \
                 bits (running as root?)"
            );
            return;
        }

        assert_eq!(
            scan.files.len(),
            1,
            "the readable sibling must survive an unreadable directory; got {:?}",
            scan.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert!(scan.files[0].path.ends_with("readable.jpg"));
        assert_eq!(
            scan.skipped, 1,
            "the directory that could not be read must be counted, not silently dropped"
        );
    }
}
