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
use std::process::Command;

// Shared with `tests/docs.rs`, which checks the same binary list against the
// documentation rather than against the packaging script.
#[path = "common/repo.rs"]
mod repo;
use repo::{declared_binaries, read, repo_root};

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

/// The shell of one step's `run:` block in `.github/workflows/release.yml`,
/// found by the step's `name:`.
///
/// Hand-rolled rather than parsed as YAML: the crate has no YAML dependency,
/// and adding one to read two blocks out of one file would put a parser in the
/// dependency tree of a tool that moves photographs. It is exact about what it
/// takes — the block ends at the first non-blank line indented no further than
/// the `run:` that opened it — so a step gaining a sibling key cannot silently
/// swallow the next step's script.
fn workflow_step(step_name: &str) -> String {
    let workflow = read(".github/workflows/release.yml");
    let marker = format!("- name: {step_name}");

    let mut lines = workflow
        .lines()
        .skip_while(|line| !line.trim_start().starts_with(&marker))
        .peekable();
    assert!(
        lines.peek().is_some(),
        "release.yml has no step named {step_name:?} — it was renamed, and this \
         test would otherwise have nothing to check"
    );
    lines.next();

    let indent_of = |line: &str| line.len() - line.trim_start().len();
    let mut run_indent = None;
    let mut body: Vec<&str> = Vec::new();
    for line in lines {
        match run_indent {
            None => {
                let trimmed = line.trim_start();
                assert!(
                    !trimmed.starts_with("- name:"),
                    "the {step_name:?} step has no `run: |` block"
                );
                if trimmed == "run: |" {
                    run_indent = Some(indent_of(line));
                }
            }
            Some(indent) => {
                if !line.trim().is_empty() && indent_of(line) <= indent {
                    break;
                }
                body.push(line);
            }
        }
    }
    assert!(
        run_indent.is_some(),
        "the {step_name:?} step ran off the end of the file"
    );

    // Dedent by the shallowest non-blank line, so the extracted script is what
    // bash would be handed rather than something uniformly over-indented.
    let margin = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indent_of(line))
        .min()
        .unwrap_or(0);
    body.iter()
        .map(|line| line.get(margin..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The workflow's arch check and smoke run used to name `mmm` and nothing else,
/// so `mmm-dedup-verifier` and `mmm-fixtures` were built, archived and published
/// without ever being started. They now iterate a list the workflow reads out of
/// `package-release.sh` at run time — and this runs that step, rather than
/// reading it, because a derivation that silently produces an empty list would
/// leave every loop below it passing having examined nothing.
///
/// Held against `[[bin]]`, so the chain is complete: the crate declares the
/// binaries, [`the_release_ships_exactly_the_binaries_the_crate_declares`] pins
/// the packaging script to that, and this pins the workflow's checks to the same
/// line of the same script.
#[test]
fn the_workflow_derives_the_binary_list_it_checks() {
    let step = workflow_step("Read the binary list from the packaging script");
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join("github-env");
    fs::write(&env_file, "").unwrap();

    let out = Command::new("bash")
        .arg("-c")
        .arg(&step)
        .current_dir(repo_root())
        .env("GITHUB_ENV", &env_file)
        .output()
        .expect("bash is available on every platform CI runs on");
    assert!(
        out.status.success(),
        "the step that reads the binary list failed: {out:?}"
    );

    let exported = fs::read_to_string(&env_file).unwrap();
    let mut derived: Vec<String> = exported
        .lines()
        .find_map(|line| line.strip_prefix("RELEASE_BINARIES="))
        .expect("the step exported no RELEASE_BINARIES, so both loops would be empty")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut declared = declared_binaries();
    derived.sort();
    declared.sort();

    assert_eq!(
        derived, declared,
        "release.yml will arch-check and smoke-run {derived:?}, but the crate \
         declares {declared:?}"
    );
}

/// The list being right is worth nothing if the steps do not read it.
///
/// One honest limit: this pins the loop, which is what covers every binary's
/// `--version`. The piece of real work each binary then does is written out one
/// binary at a time — necessarily, since they do different things — so a fourth
/// binary would be started here and given nothing to do, and only the loop would
/// say so.
#[test]
fn the_workflows_checks_iterate_that_list_rather_than_their_own() {
    for step_name in [
        "Check the architecture is the one that was asked for",
        "Smoke-run the binaries",
    ] {
        let body = workflow_step(step_name);
        assert!(
            body.contains("for name in $RELEASE_BINARIES"),
            "the {step_name:?} step does not loop over $RELEASE_BINARIES, so it \
             covers whichever binaries somebody wrote into it rather than the \
             ones the release ships:\n{body}"
        );
    }
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
