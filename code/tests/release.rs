//! Claims the release automation makes, checked on every push rather than once
//! per release.
//!
//! `.github/workflows/release.yml` runs on a `v*` tag and nothing else, so the
//! two scripts it calls are executed exactly once per release — at the moment
//! it is least convenient to find out one of them is wrong. Everything here is
//! cheap enough to run in the ordinary suite, which means a change that breaks
//! the packaging fails on the pull request that made it instead of on the tag.
//!
//! Same reasoning as `docs.rs` next door: these pin facts that are stated in
//! two places and would otherwise drift in silence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The repository root: one level up from the crate, which is `code/`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory always has a parent")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()))
}

/// Every `name` under a `[[bin]]` table in `Cargo.toml`.
fn declared_binaries() -> Vec<String> {
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

/// The `BINARIES="…"` declaration in the packaging script.
///
/// Parsed off that one line rather than searched for anywhere in the file. The
/// first version of this test asked whether the script *contained* each binary
/// name and passed against a script that had stopped copying one of them — the
/// name was still in a comment at the top. A test that reads the declaration
/// can only be satisfied by the declaration.
fn packaged_binaries() -> Vec<String> {
    let script = read(".github/scripts/package-release.sh");
    let line = script
        .lines()
        .find(|line| line.starts_with("BINARIES="))
        .expect("package-release.sh no longer declares BINARIES on its own line");

    line.trim_start_matches("BINARIES=")
        .trim_matches('"')
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Adding a third `[[bin]]` to `Cargo.toml` would otherwise produce a release
/// that quietly ships two of them, and nothing about the tarball would say a
/// binary was missing. Asserted as set against set, so the reverse — a binary
/// left in the script after being removed from the crate, which fails the
/// release at tag time — is caught here too.
#[test]
fn the_release_ships_exactly_the_binaries_the_crate_declares() {
    let mut declared = declared_binaries();
    let mut packaged = packaged_binaries();
    declared.sort();
    packaged.sort();

    assert_eq!(
        declared, packaged,
        "code/Cargo.toml declares {declared:?} but \
         .github/scripts/package-release.sh ships {packaged:?}"
    );
}

/// A workflow pointing at a script that has been renamed fails at tag time, on
/// a tag that has already been pushed.
#[test]
fn the_release_workflow_calls_the_scripts_that_exist() {
    let workflow = read(".github/workflows/release.yml");
    for script in [
        ".github/scripts/changelog-section.sh",
        ".github/scripts/package-release.sh",
    ] {
        assert!(
            workflow.contains(script),
            "release.yml no longer calls {script}"
        );
        assert!(
            repo_root().join(script).is_file(),
            "{script} is called by release.yml but does not exist"
        );
    }
}

/// Run the changelog extractor against a throwaway changelog.
fn extract(version: &str, changelog: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("CHANGELOG.md");
    fs::write(&path, changelog).unwrap();

    Command::new("bash")
        .arg(repo_root().join(".github/scripts/changelog-section.sh"))
        .arg(version)
        .arg(&path)
        .output()
        .expect("bash is available on every platform CI runs on")
}

/// A changelog shaped the way this one will be once a version is cut: an open
/// `Unreleased` section above the released ones, and link-reference definitions
/// below all of them.
const SAMPLE: &str = "\
# Changelog

## [Unreleased]

- something not yet released

## [0.2.0] — 2026-08-09

### Breaking

- dry-run by default

## [0.1.0] — 2026-04-18

- the first one

[Unreleased]: https://github.com/t0lkim/ThreeM/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/t0lkim/ThreeM/releases/tag/v0.2.0
";

#[test]
fn the_extractor_takes_one_section_and_stops_at_the_next() {
    let out = extract("0.2.0", SAMPLE);
    assert!(out.status.success(), "extraction failed: {out:?}");
    let body = String::from_utf8(out.stdout).unwrap();

    assert_eq!(
        body, "### Breaking\n\n- dry-run by default\n",
        "the body is not exactly the 0.2.0 section: the heading should be gone, \
         the following section should not be included, and the blank padding \
         around it should be trimmed"
    );
}

/// The link references sit after the last section, so the final release would
/// otherwise sweep them into its body.
#[test]
fn the_extractor_drops_the_link_reference_definitions() {
    let out = extract("0.1.0", SAMPLE);
    assert!(out.status.success());
    let body = String::from_utf8(out.stdout).unwrap();
    assert_eq!(body, "- the first one\n");
}

/// The reason this runs before anything is built: a tag pushed against a
/// changelog nobody updated must fail, not publish an empty release body.
#[test]
fn the_extractor_refuses_a_version_with_no_section() {
    let out = extract("9.9.9", SAMPLE);
    assert!(
        !out.status.success(),
        "a missing section was accepted, so a release would be published with \
         an empty body"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("9.9.9"),
        "the error does not name the version it looked for: {stderr}"
    );
}

/// `0.2` is not `0.2.0`, and a heading is matched on the text between the
/// brackets rather than on the line containing it.
#[test]
fn the_extractor_does_not_match_a_partial_version() {
    let out = extract("0.2", SAMPLE);
    assert!(
        !out.status.success(),
        "`0.2` matched the `## [0.2.0]` heading, so a mistyped tag would \
         publish the wrong release notes"
    );
}
