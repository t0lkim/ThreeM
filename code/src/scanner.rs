use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, warn};
use walkdir::WalkDir;

/// Known image extensions (lowercase, no dot)
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "tiff", "tif", "raw", "cr2", "cr3", "nef", "arw", "dng",
    "orf", "rw2", "raf", "srw", "pef", "webp", "avif", "bmp",
];

/// Known video extensions (lowercase, no dot)
const VIDEO_EXTENSIONS: &[&str] = &[
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

/// Scan one or more directories recursively for media files
pub fn scan_directories(dirs: &[PathBuf]) -> Result<Vec<ScannedFile>> {
    let image_ext: HashSet<&str> = IMAGE_EXTENSIONS.iter().copied().collect();
    let video_ext: HashSet<&str> = VIDEO_EXTENSIONS.iter().copied().collect();

    let mut files = Vec::new();

    for dir in dirs {
        if !dir.is_dir() {
            warn!("skipping non-directory path: {}", dir.display());
            continue;
        }

        for entry in WalkDir::new(dir).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", dir.display()))?;

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let ext = match normalised_extension(path) {
                Some(e) => e,
                None => continue,
            };

            let is_image = image_ext.contains(ext.as_str());
            let is_video = video_ext.contains(ext.as_str());

            if !is_image && !is_video {
                continue;
            }

            let metadata = entry
                .metadata()
                .with_context(|| format!("reading metadata for {}", path.display()))?;

            debug!(path = %path.display(), size = metadata.len(), "found media file");

            files.push(ScannedFile {
                path: path.to_path_buf(),
                size: metadata.len(),
                extension: ext,
                is_video,
            });
        }
    }

    Ok(files)
}

fn normalised_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_scan_finds_jpeg() {
        let tmp = TempDir::new().unwrap();
        let jpg = tmp.path().join("photo.jpg");
        fs::write(&jpg, b"fake jpeg data").unwrap();

        let files = scan_directories(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension, "jpg");
        assert!(!files[0].is_video);
    }

    #[test]
    fn test_scan_finds_video() {
        let tmp = TempDir::new().unwrap();
        let mov = tmp.path().join("clip.mov");
        fs::write(&mov, b"fake mov data").unwrap();

        let files = scan_directories(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].is_video);
    }

    #[test]
    fn test_scan_skips_non_media() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("readme.txt"), b"text").unwrap();
        fs::write(tmp.path().join("doc.pdf"), b"pdf").unwrap();

        let files = scan_directories(&[tmp.path().to_path_buf()]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_scan_recursive() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("deep.png"), b"png data").unwrap();

        let files = scan_directories(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_scan_multiple_dirs() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        fs::write(tmp1.path().join("a.jpg"), b"data").unwrap();
        fs::write(tmp2.path().join("b.mp4"), b"data").unwrap();

        let files =
            scan_directories(&[tmp1.path().to_path_buf(), tmp2.path().to_path_buf()]).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_extension_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo.JPG"), b"data").unwrap();
        fs::write(tmp.path().join("clip.MOV"), b"data").unwrap();

        let files = scan_directories(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(files.len(), 2);
    }
}
