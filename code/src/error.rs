use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum MediaError {
    #[error("IO error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("metadata extraction failed for {path}: {reason}")]
    Metadata { path: PathBuf, reason: String },

    #[error("hash computation failed for {path}: {source}")]
    Hash {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("move failed from {src} to {dst}: {reason}")]
    Move {
        src: PathBuf,
        dst: PathBuf,
        reason: String,
    },
}
