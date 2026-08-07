//! ThreeM (`mmm`) — media organiser library.
//!
//! The binaries in `src/main.rs` and `src/bin/` are thin consumers of this
//! crate. Everything the CLI does lives here so integration tests under
//! `tests/` can drive the same code paths the binary does.

pub mod config;
pub mod error;
pub mod geocoder;
pub mod hasher;
pub mod metadata;
pub mod organiser;
pub mod reporter;
pub mod scanner;
