use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// Scan one or more directories recursively for media files.
///
/// Infallible by construction. Every per-entry failure is a warning and a
/// skip, never an abort, because the caller is about to organise whatever
/// comes back and "the scan died two directories in" must not be
/// indistinguishable from "that is all there was".
pub fn scan_directories(dirs: &[PathBuf]) -> ScanResult {
    let image_ext: HashSet<&str> = IMAGE_EXTENSIONS.iter().copied().collect();
    let video_ext: HashSet<&str> = VIDEO_EXTENSIONS.iter().copied().collect();

    let mut files = Vec::new();
    let mut skipped = 0;

    for dir in dirs {
        if !dir.is_dir() {
            warn!("skipping non-directory path: {}", dir.display());
            continue;
        }

        for entry in WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !is_excluded(entry))
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

            let is_image = image_ext.contains(ext.as_str());
            let is_video = video_ext.contains(ext.as_str());

            if !is_image && !is_video {
                continue;
            }

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
                is_video,
            });
        }
    }

    ScanResult { files, skipped }
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

        let scan = scan_directories(&[tmp.path().to_path_buf()]);
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

        let scan = scan_directories(&[tmp.path().to_path_buf()]);
        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].is_video);
    }

    #[test]
    fn test_scan_skips_non_media() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("readme.txt"), b"text").unwrap();
        fs::write(tmp.path().join("doc.pdf"), b"pdf").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()]);
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

        let scan = scan_directories(&[tmp.path().to_path_buf()]);
        assert_eq!(scan.files.len(), 1);
    }

    #[test]
    fn test_scan_multiple_dirs() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        fs::write(tmp1.path().join("a.jpg"), b"data").unwrap();
        fs::write(tmp2.path().join("b.mp4"), b"data").unwrap();

        let scan = scan_directories(&[tmp1.path().to_path_buf(), tmp2.path().to_path_buf()]);
        assert_eq!(scan.files.len(), 2);
    }

    #[test]
    fn test_extension_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo.JPG"), b"data").unwrap();
        fs::write(tmp.path().join("clip.MOV"), b"data").unwrap();

        let scan = scan_directories(&[tmp.path().to_path_buf()]);
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

        let scan = scan_directories(&[tmp.path().to_path_buf()]);

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

        let scan = scan_directories(&[tmp.path().to_path_buf()]);

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

        let scan = scan_directories(&[meta]);
        assert_eq!(scan.files.len(), 1);
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
        let scan = scan_directories(&[tmp.path().to_path_buf()]);

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
