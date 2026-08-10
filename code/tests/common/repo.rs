//! Reading the repository from a test, for the two suites that need to.
//!
//! `tests/docs.rs` and `tests/release.rs` both assert that a fact declared in
//! one place is stated the same way in another, and both therefore have to read
//! files that live *above* the cargo package — `README.md`, `CONTRIBUTING.md`,
//! `.github/`. They also both need the list of binaries the crate declares, and
//! that list is the thing they are checking two different consumers against:
//! the packaging script in one case, the documentation in the other.
//!
//! It lives here rather than twice over because a second copy of
//! [`declared_binaries`] is a second parser to fix the day `Cargo.toml`'s
//! formatting changes — and, worse, a second answer to "which binaries does
//! this crate ship?", which is the exact question both suites exist to keep
//! from having two answers.
//!
//! Not `common/mod.rs`: that is the fixture re-export surface every integration
//! suite pulls in, and none of the other eight want these. Both callers include
//! this file directly with `#[path = "common/repo.rs"] mod repo;`.
//!
//! Both callers are gated on the `repository-tests` feature, for the reason
//! recorded in `Cargo.toml` and `.cargo/mutants.toml`: under `cargo-mutants`
//! the package is copied into a temp directory and there is no repository above
//! it, so every function here would fail on a missing file rather than on a
//! broken claim.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

/// The repository root: one level up from the crate, which is `code/`.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory always has a parent")
        .to_path_buf()
}

/// Read a file named relative to the repository root.
pub fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()))
}

/// Every `name` under a `[[bin]]` table in `Cargo.toml`.
///
/// Parsed out of the manifest rather than listed here, so that adding a binary
/// is what makes the callers' assertions cover it. A literal list in a test is
/// a third place to remember, and it would go stale in exactly the way the
/// callers exist to prevent.
pub fn declared_binaries() -> Vec<String> {
    let manifest = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("the crate's own manifest is readable");

    let mut names = Vec::new();
    let mut in_bin = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_bin = line == "[[bin]]";
            continue;
        }
        if in_bin {
            if let Some(value) = line.strip_prefix("name = ") {
                names.push(value.trim_matches('"').to_string());
            }
        }
    }
    assert!(
        !names.is_empty(),
        "no [[bin]] targets found in Cargo.toml — this test is parsing it wrongly"
    );
    names
}
