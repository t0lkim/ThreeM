use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use tracing::{error, info};

use crate::geocoder::GeoLookup;
use crate::hasher::DuplicateGroup;
use crate::metadata::{self, DateSource, FileMetadata};
use crate::scanner::ScannedFile;

/// A planned file operation (computed during scan, executed during process)
#[derive(Debug, Clone)]
pub struct PlannedMove {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub date_source: DateSource,
    pub has_location: bool,
}

/// Build the target path for a file based on its metadata
pub fn plan_move(file: &ScannedFile, output_dir: &Path, geo: &GeoLookup) -> Result<PlannedMove> {
    let meta = metadata::extract_metadata(&file.path, file.is_video)?;

    let (date_dir, filename) = build_target_path(&meta, &file.extension, geo);
    let destination = output_dir.join(date_dir).join(filename);

    Ok(PlannedMove {
        source: file.path.clone(),
        destination,
        date_source: meta.date_source,
        has_location: meta.latitude.is_some() && meta.longitude.is_some(),
    })
}

/// Build the directory path (YYYY/MM/DD) and filename (YYYY-MM-DD-HHMMSS[-location].ext)
fn build_target_path(meta: &FileMetadata, extension: &str, geo: &GeoLookup) -> (PathBuf, String) {
    match meta.date {
        Some(dt) => {
            let dir = date_directory(&dt);
            let filename = date_filename(&dt, meta, extension, geo);
            (dir, filename)
        }
        None => {
            let dir = PathBuf::from("unsorted");
            let filename = format!("unknown.{}", extension);
            (dir, filename)
        }
    }
}

fn date_directory(dt: &DateTime<Utc>) -> PathBuf {
    PathBuf::from(format!("{}/{:02}/{:02}", dt.year(), dt.month(), dt.day()))
}

fn date_filename(
    dt: &DateTime<Utc>,
    meta: &FileMetadata,
    extension: &str,
    geo: &GeoLookup,
) -> String {
    let base = format!(
        "{}-{:02}-{:02}-{:02}{:02}{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );

    let location_part = match (meta.latitude, meta.longitude) {
        (Some(lat), Some(lon)) => geo
            .lookup(lat, lon)
            .map(|info| format!("-{}", info.filename_part)),
        _ => None,
    };

    match location_part {
        Some(loc) => format!("{}{}.{}", base, loc, extension),
        None => format!("{}.{}", base, extension),
    }
}

/// Resolve filename collisions by appending a numeric suffix
pub fn resolve_collision(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }

    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let parent = path.parent().unwrap_or(Path::new("."));

    for i in 1..10000 {
        let candidate = if ext.is_empty() {
            parent.join(format!("{}-{}", stem, i))
        } else {
            parent.join(format!("{}-{}.{}", stem, i, ext))
        };
        if !candidate.exists() {
            return candidate;
        }
    }

    // Extremely unlikely — 10000 collisions
    parent.join(format!(
        "{}-{}.{}",
        stem,
        chrono::Utc::now().timestamp_millis(),
        ext
    ))
}

/// Move duplicate files into numbered subdirectories under duplicates/
/// Each duplicate group gets its own directory: duplicates/000/, duplicates/001/, etc.
/// The first file in each group is the "original" and is NOT moved here.
pub fn move_duplicates(groups: &[DuplicateGroup], output_dir: &Path) -> Result<(usize, usize)> {
    let dup_base = output_dir.join("duplicates");
    let mut moved = 0;
    let mut errors = 0;

    for (i, group) in groups.iter().enumerate() {
        let group_dir = dup_base.join(format!("{:03}", i));
        fs::create_dir_all(&group_dir)
            .with_context(|| format!("creating duplicate dir {}", group_dir.display()))?;

        // Write a manifest file for the verifier
        let manifest_path = group_dir.join("manifest.txt");
        let mut manifest = format!(
            "# Duplicate group {:03}\n# BLAKE3 hash: {}\n# File size: {} bytes\n# Original kept at: {}\n\n",
            i, group.hash, group.size,
            group.files[0].display()
        );

        // Skip the first file (kept as original), move the rest
        for dup_path in group.files.iter().skip(1) {
            let filename = dup_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let dest = resolve_collision(&group_dir.join(&filename));

            manifest.push_str(&format!("{}\n", dup_path.display()));

            let planned = PlannedMove {
                source: dup_path.clone(),
                destination: dest,
                date_source: DateSource::None,
                has_location: false,
            };

            match execute_move(&planned) {
                Ok(()) => moved += 1,
                Err(e) => {
                    error!(path = %dup_path.display(), error = %e, "failed to move duplicate");
                    errors += 1;
                }
            }
        }

        fs::write(&manifest_path, manifest)
            .with_context(|| format!("writing manifest {}", manifest_path.display()))?;
    }

    Ok((moved, errors))
}

/// Execute a planned move atomically
pub fn execute_move(planned: &PlannedMove) -> Result<()> {
    let dest_dir = planned
        .destination
        .parent()
        .context("destination has no parent directory")?;

    // Create target directory
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating directory {}", dest_dir.display()))?;

    let final_dest = resolve_collision(&planned.destination);

    // Check if source and destination are on the same filesystem
    // by attempting rename first (atomic, O(1) on same volume)
    match fs::rename(&planned.source, &final_dest) {
        Ok(()) => {
            info!(
                src = %planned.source.display(),
                dst = %final_dest.display(),
                "moved (rename)"
            );
            Ok(())
        }
        Err(_) => {
            // Cross-volume: copy, verify, delete
            cross_volume_move(&planned.source, &final_dest)
        }
    }
}

/// Safe cross-volume move: copy → verify → delete source
fn cross_volume_move(src: &Path, dst: &Path) -> Result<()> {
    let dst_dir = dst.parent().context("destination has no parent")?;

    // Copy to temp file in same directory as destination
    let temp_name = format!(".tmp-{}", chrono::Utc::now().timestamp_millis());
    let temp_path = dst_dir.join(temp_name);

    fs::copy(src, &temp_path).with_context(|| format!("copying {} to temp file", src.display()))?;

    // Verify the copy by comparing sizes
    let src_size = fs::metadata(src)
        .with_context(|| format!("reading source metadata: {}", src.display()))?
        .len();
    let tmp_size = fs::metadata(&temp_path)
        .with_context(|| format!("reading temp file metadata: {}", temp_path.display()))?
        .len();

    if src_size != tmp_size {
        // Clean up temp file and bail
        let _ = fs::remove_file(&temp_path);
        bail!(
            "copy verification failed for {}: source {} bytes, copy {} bytes",
            src.display(),
            src_size,
            tmp_size
        );
    }

    // Atomic rename within same volume (temp → final)
    fs::rename(&temp_path, dst).with_context(|| format!("renaming temp to {}", dst.display()))?;

    // Only now delete the source
    fs::remove_file(src).with_context(|| format!("removing source file {}", src.display()))?;

    info!(
        src = %src.display(),
        dst = %dst.display(),
        "moved (cross-volume copy+verify+delete)"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_date_directory() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap()
            .and_utc();
        assert_eq!(date_directory(&dt), PathBuf::from("2024/03/15"));
    }

    #[test]
    fn test_resolve_collision_no_conflict() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("photo.jpg");
        assert_eq!(resolve_collision(&path), path);
    }

    #[test]
    fn test_resolve_collision_with_conflict() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("photo.jpg");
        fs::write(&path, b"exists").unwrap();

        let resolved = resolve_collision(&path);
        assert_ne!(resolved, path);
        assert!(resolved.to_str().unwrap().contains("photo-1.jpg"));
    }

    #[test]
    fn test_resolve_collision_multiple() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("photo.jpg");
        fs::write(&path, b"exists").unwrap();
        fs::write(tmp.path().join("photo-1.jpg"), b"exists").unwrap();

        let resolved = resolve_collision(&path);
        assert!(resolved.to_str().unwrap().contains("photo-2.jpg"));
    }

    #[test]
    fn test_execute_move_same_volume() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.jpg");
        let dst_dir = tmp.path().join("2024/01/15");
        let dst = dst_dir.join("2024-01-15-103000.jpg");
        fs::write(&src, b"image data").unwrap();

        let planned = PlannedMove {
            source: src.clone(),
            destination: dst.clone(),
            date_source: DateSource::Exif,
            has_location: false,
        };

        execute_move(&planned).unwrap();
        assert!(!src.exists());
        assert!(dst.exists());
    }

    #[test]
    fn test_build_target_path_no_date() {
        let meta = FileMetadata {
            date: None,
            latitude: None,
            longitude: None,
            date_source: DateSource::None,
        };
        let geo = GeoLookup::new();
        let (dir, name) = build_target_path(&meta, "jpg", &geo);
        assert_eq!(dir, PathBuf::from("unsorted"));
        assert_eq!(name, "unknown.jpg");
    }
}
