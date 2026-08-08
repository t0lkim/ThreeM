//! `ThreeM` (`mmm`) — media organiser library.
//!
//! The binaries in `src/main.rs` and `src/bin/` are thin consumers of this
//! crate. Everything the CLI does lives here so integration tests under
//! `tests/` can drive the same code paths the binary does.

/// The directory `mmm` keeps its own metadata in, at the root of an output
/// tree: run journals today, anything else the tool needs to remember later.
///
/// It lives here rather than in [`config`] or [`journal`] because two modules
/// need to agree on it for opposite reasons — [`config`] writes into it, and
/// [`scanner`] must refuse to look inside it. A tool that organised its own
/// journal into a dated photo directory would eat the record of what it had
/// just done.
pub const METADATA_DIR_NAME: &str = ".mmm";

pub mod config;
pub mod error;
pub mod geocoder;
pub mod hasher;
pub mod journal;
pub mod metadata;
pub mod naming;
pub mod organiser;
pub mod reporter;
pub mod scanner;
pub mod settings;
pub mod undo;
