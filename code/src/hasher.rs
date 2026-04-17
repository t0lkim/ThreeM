use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use tracing::debug;

use crate::scanner::ScannedFile;

/// Size of the partial hash read (first + last N bytes)
const PARTIAL_HASH_BYTES: u64 = 64 * 1024; // 64KB

/// Result of the three-phase dedup analysis
#[derive(Debug)]
pub struct DedupResult {
    /// Files that are unique (no duplicates found)
    pub unique: Vec<ScannedFile>,
    /// Groups of duplicate files (each group shares identical content)
    pub duplicate_groups: Vec<DuplicateGroup>,
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub hash: String,
    pub size: u64,
    pub files: Vec<PathBuf>,
}

/// Three-phase dedup cascade:
/// 1. Group by file size (free — metadata only)
/// 2. Partial BLAKE3 hash (first 64KB + last 64KB)
/// 3. Full BLAKE3 hash (only for partial-hash matches)
pub fn find_duplicates(files: Vec<ScannedFile>, progress: &ProgressBar) -> Result<DedupResult> {
    progress.set_message("Phase 1: grouping by file size");
    let size_groups = group_by_size(&files);

    // Files with unique sizes are immediately unique
    let mut unique: Vec<ScannedFile> = Vec::new();
    let mut candidates: Vec<Vec<&ScannedFile>> = Vec::new();

    for group in size_groups.values() {
        if group.len() == 1 {
            unique.push(group[0].clone());
        } else {
            candidates.push(group.iter().collect());
        }
    }

    debug!(
        unique = unique.len(),
        candidate_groups = candidates.len(),
        "phase 1 complete"
    );

    // Phase 2: Partial hash
    progress.set_message("Phase 2: partial hashing size-matched files");
    let mut phase3_candidates: Vec<Vec<&ScannedFile>> = Vec::new();

    for group in &candidates {
        let partial_groups = group_by_partial_hash(group)?;
        for (_hash, pgroup) in partial_groups {
            if pgroup.len() == 1 {
                unique.push(pgroup[0].clone());
            } else {
                phase3_candidates.push(pgroup);
            }
        }
        progress.inc(group.len() as u64);
    }

    debug!(phase3_groups = phase3_candidates.len(), "phase 2 complete");

    // Phase 3: Full hash
    progress.set_message("Phase 3: full hashing confirmed candidates");
    let mut duplicate_groups: Vec<DuplicateGroup> = Vec::new();

    for group in &phase3_candidates {
        let full_groups = group_by_full_hash(group)?;
        for (hash, fgroup) in full_groups {
            if fgroup.len() == 1 {
                unique.push(fgroup[0].clone());
            } else {
                duplicate_groups.push(DuplicateGroup {
                    hash,
                    size: fgroup[0].size,
                    files: fgroup.iter().map(|f| f.path.clone()).collect(),
                });
                // Keep the first file as the "original", rest are duplicates
                unique.push(fgroup[0].clone());
            }
        }
        progress.inc(group.len() as u64);
    }

    Ok(DedupResult {
        unique,
        duplicate_groups,
    })
}

fn group_by_size(files: &[ScannedFile]) -> HashMap<u64, Vec<ScannedFile>> {
    let mut groups: HashMap<u64, Vec<ScannedFile>> = HashMap::new();
    for file in files {
        groups.entry(file.size).or_default().push(file.clone());
    }
    groups
}

fn group_by_partial_hash<'a>(
    files: &[&'a ScannedFile],
) -> Result<HashMap<String, Vec<&'a ScannedFile>>> {
    let mut groups: HashMap<String, Vec<&'a ScannedFile>> = HashMap::new();
    for file in files {
        let hash = partial_hash(&file.path, file.size)?;
        groups.entry(hash).or_default().push(file);
    }
    Ok(groups)
}

fn group_by_full_hash<'a>(
    files: &[&'a ScannedFile],
) -> Result<HashMap<String, Vec<&'a ScannedFile>>> {
    let mut groups: HashMap<String, Vec<&'a ScannedFile>> = HashMap::new();
    for file in files {
        let hash = full_hash(&file.path)?;
        groups.entry(hash).or_default().push(file);
    }
    Ok(groups)
}

/// Hash first 64KB + last 64KB of a file using BLAKE3
fn partial_hash(path: &PathBuf, size: u64) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for partial hash", path.display()))?;

    let mut hasher = blake3::Hasher::new();

    // Read first chunk
    let first_bytes = std::cmp::min(PARTIAL_HASH_BYTES, size);
    let mut buf = vec![0u8; first_bytes as usize];
    file.read_exact(&mut buf)
        .with_context(|| format!("reading first bytes of {}", path.display()))?;
    hasher.update(&buf);

    // Read last chunk (if file is large enough for it to differ from the first)
    if size > PARTIAL_HASH_BYTES * 2 {
        file.seek(SeekFrom::End(-(PARTIAL_HASH_BYTES as i64)))
            .with_context(|| format!("seeking in {}", path.display()))?;
        file.read_exact(&mut buf)
            .with_context(|| format!("reading last bytes of {}", path.display()))?;
        hasher.update(&buf);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Full streaming BLAKE3 hash of a file
fn full_hash(path: &PathBuf) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} for full hash", path.display()))?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024]; // 128KB read buffer

    loop {
        let bytes_read = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buf[..bytes_read]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Create a progress bar styled for hashing operations
pub fn hashing_progress_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
            .expect("valid progress template")
            .progress_chars("##-"),
    );
    pb
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_scanned(path: PathBuf, size: u64) -> ScannedFile {
        ScannedFile {
            path,
            size,
            extension: "jpg".to_string(),
            is_video: false,
        }
    }

    #[test]
    fn test_unique_files_by_size() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        fs::write(&f1, b"short").unwrap();
        fs::write(&f2, b"much longer content here").unwrap();

        let files = vec![make_scanned(f1, 5), make_scanned(f2, 24)];

        let pb = ProgressBar::hidden();
        let result = find_duplicates(files, &pb).unwrap();
        assert_eq!(result.unique.len(), 2);
        assert!(result.duplicate_groups.is_empty());
    }

    #[test]
    fn test_exact_duplicates_detected() {
        let tmp = TempDir::new().unwrap();
        let content = b"identical content for both files";
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        fs::write(&f1, content).unwrap();
        fs::write(&f2, content).unwrap();

        let size = content.len() as u64;
        let files = vec![make_scanned(f1, size), make_scanned(f2, size)];

        let pb = ProgressBar::hidden();
        let result = find_duplicates(files, &pb).unwrap();
        assert_eq!(result.duplicate_groups.len(), 1);
        assert_eq!(result.duplicate_groups[0].files.len(), 2);
    }

    #[test]
    fn test_same_size_different_content() {
        let tmp = TempDir::new().unwrap();
        let f1 = tmp.path().join("a.jpg");
        let f2 = tmp.path().join("b.jpg");
        // Same length, different content
        fs::write(&f1, b"aaaa1234").unwrap();
        fs::write(&f2, b"bbbb5678").unwrap();

        let files = vec![make_scanned(f1, 8), make_scanned(f2, 8)];

        let pb = ProgressBar::hidden();
        let result = find_duplicates(files, &pb).unwrap();
        assert_eq!(result.unique.len(), 2);
        assert!(result.duplicate_groups.is_empty());
    }
}
