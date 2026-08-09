#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a test that unwraps is a test that fails loudly; `tests/` is a separate \
              crate, so the library's in-module allow does not reach here"
)]

//! The generator's claims, held against what `mmm` actually does.
//!
//! `mmm-fixtures` ships an `EXPECTED.md` telling a user where every generated
//! file should end up. That document is the entire reason the generator is
//! worth shipping — a synthetic library nobody can check their result against
//! builds no confidence at all — and a document nothing verifies is a document
//! that goes stale the first time the organiser changes.
//!
//! So this suite generates a library, organises it, and asserts the outcome
//! matches the generator's own predictions, file by file. If the two ever
//! disagree the suite fails, which is the only arrangement under which
//! `EXPECTED.md` can be trusted.
//!
//! The mapping from a landed file back to the one that was generated is the
//! provenance marker each synthesised file carries — not the filename, which is
//! precisely the thing the organiser rewrites.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use tempfile::TempDir;

use mmm::fixtures::{file_contents_by_marker, MediaTree};
use mmm::generate::{expected_markdown, generate, Expect, Plan, Profile};

/// UTC throughout: every dated fixture the generator lays down writes an
/// explicit `+00:00`, so the run's own zone cannot move a file — but pinning it
/// keeps the filesystem-dated fallbacks from drifting across a midnight
/// boundary on a machine east of Greenwich.
const ARGS: &[&str] = &["--timezone", "UTC", "--no-prompt", "--commit"];

fn organise(input: &Path, output: &Path) -> std::process::Output {
    Command::cargo_bin("mmm")
        .expect("mmm builds")
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(ARGS)
        .output()
        .expect("running mmm")
}

/// Generate into a real directory, organise it, and return the plan alongside a
/// marker → landed-paths map of the result.
fn generate_and_organise(
    profile: Profile,
    seed: u64,
) -> (Plan, BTreeMap<String, Vec<String>>, TempDir, TempDir) {
    let src = TempDir::new().expect("temp dir");
    let dst = TempDir::new().expect("temp dir");

    let tree = MediaTree::at(src.path()).expect("preparing the source directory");
    let (tree, plan) = generate(tree, profile, seed);
    drop(tree);

    let out = organise(src.path(), dst.path());
    assert!(
        out.status.success(),
        "organising a generated library must succeed, malformed files included.\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let landed = file_contents_by_marker(dst.path());
    (plan, landed, src, dst)
}

/// The claim that matters: every file the generator says lands under a named
/// date directory is there, in the organised tree, identified by its own bytes.
#[test]
fn every_definite_expectation_is_met() {
    for (profile, seed) in [
        (Profile::Minimal, 1),
        (Profile::Realistic, 2),
        (Profile::Awkward, 3),
    ] {
        let (plan, landed, _src, _dst) = generate_and_organise(profile, seed);

        let mut checked = 0;
        for entry in &plan.entries {
            let (Expect::Filed(directory) | Expect::FiledFromFilesystem(directory)) = &entry.expect
            else {
                continue;
            };

            let Some(paths) = landed.get(&entry.rel) else {
                panic!(
                    "{profile}/seed {seed}: EXPECTED.md claims `{}` lands under `{directory}/`, \
                     and no file in the organised tree carries its bytes at all",
                    entry.rel
                );
            };

            assert!(
                paths
                    .iter()
                    .any(|p| p.starts_with(&format!("{directory}/"))),
                "{profile}/seed {seed}: EXPECTED.md claims `{}` lands under `{directory}/`; it \
                 is at {paths:?}.\n\nOne of the two is wrong — either the organiser changed or \
                 the generator's prediction did — and shipping the document without settling \
                 which would hand users a lie.",
                entry.rel,
            );
            checked += 1;
        }

        assert!(
            checked >= 10,
            "{profile}/seed {seed}: only {checked} definite expectations were checked, which is \
             too few for this test to be evidence of anything"
        );
    }
}

/// Duplicates: one member of each group survives in the dated tree, the rest
/// are moved under `duplicates/`, and nothing is deleted.
#[test]
fn duplicate_groups_keep_exactly_one_original_in_the_dated_tree() {
    let (plan, landed, _src, _dst) = generate_and_organise(Profile::Realistic, 4);

    // A `duplicate_of` copy carries the marker of the file it was copied from,
    // so a whole group shares one key — which is what makes the group
    // observable at all after the organiser has renamed every member.
    let originals: Vec<&str> = plan
        .entries
        .iter()
        .filter(|e| matches!(e.expect, Expect::DuplicateOf(_)))
        .filter_map(|e| match &e.expect {
            Expect::DuplicateOf(o) => Some(o.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !originals.is_empty(),
        "the realistic profile generates duplicate groups"
    );

    for original in originals {
        let paths = landed
            .get(original)
            .unwrap_or_else(|| panic!("duplicate group `{original}` vanished entirely"));

        let (in_duplicates, filed): (Vec<_>, Vec<_>) =
            paths.iter().partition(|p| p.starts_with("duplicates/"));

        assert_eq!(
            filed.len(),
            1,
            "exactly one member of `{original}` must survive in the dated tree — found \
             {filed:?}. More than one means deduplication did nothing; none means the only \
             remaining copies are in a directory the user is about to delete."
        );
        assert!(
            !in_duplicates.is_empty(),
            "`{original}` has copies, so some of them belong in duplicates/: {paths:?}"
        );
    }
}

/// Files that are not media are left exactly where they were. A tool that moves
/// these has rearranged somebody's disk.
#[test]
fn nothing_the_plan_calls_untouched_is_moved() {
    let src = TempDir::new().expect("temp dir");
    let dst = TempDir::new().expect("temp dir");

    let tree = MediaTree::at(src.path()).expect("preparing the source directory");
    let (tree, plan) = generate(tree, Profile::Awkward, 5);
    drop(tree);

    assert!(organise(src.path(), dst.path()).status.success());

    for entry in &plan.entries {
        if entry.expect != Expect::Untouched {
            continue;
        }
        assert!(
            src.path().join(&entry.rel).exists(),
            "`{}` is not a media file and EXPECTED.md says it is left alone — it is gone",
            entry.rel
        );
    }
}

/// A user who reports a bug can only hand over a seed. If the seed does not
/// reproduce the library, the report is worthless.
#[test]
fn one_seed_reproduces_one_library_byte_for_byte() {
    let build = |seed: u64| {
        let dir = TempDir::new().expect("temp dir");
        let tree = MediaTree::at(dir.path()).expect("preparing the directory");
        let (tree, plan) = generate(tree, Profile::Realistic, seed);
        drop(tree);

        let mut files: Vec<(String, String)> = Vec::new();
        for entry in walkdir::WalkDir::new(dir.path())
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let rel = entry
                .path()
                .strip_prefix(dir.path())
                .expect("under the root")
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(entry.path()).expect("reading a file just written");
            files.push((rel, blake3::hash(&bytes).to_hex().to_string()));
        }
        (files, plan)
    };

    let (a_files, a_plan) = build(1234);
    let (b_files, b_plan) = build(1234);

    assert_eq!(
        a_files, b_files,
        "the same seed must produce the same bytes in the same places"
    );
    assert_eq!(
        expected_markdown(&a_plan),
        expected_markdown(&b_plan),
        "and the same statement about them"
    );

    let (c_files, _) = build(5678);
    assert_ne!(
        a_files, c_files,
        "a different seed must produce a different library, or --seed means nothing"
    );
}

/// The awkward profile exists to be survived. A run over it must complete, not
/// abort — the zero-byte file in it is the exact shape that panicked the tool
/// before v0.3.0.
#[test]
fn the_awkward_profile_does_not_stop_the_run() {
    let (plan, landed, _src, _dst) = generate_and_organise(Profile::Awkward, 6);

    let malformed: Vec<&str> = plan
        .entries
        .iter()
        .filter(|e| e.malformed)
        .map(|e| e.rel.as_str())
        .collect();
    assert!(
        malformed.len() >= 8,
        "the awkward profile is supposed to be full of broken files; found {}",
        malformed.len()
    );

    // The run survived (asserted inside the helper). Now check it did not
    // survive by quietly dropping the good files that share the library.
    let well_formed = plan
        .entries
        .iter()
        .filter(|e| !e.malformed && matches!(e.expect, Expect::Filed(_)))
        .count();
    let accounted = plan
        .entries
        .iter()
        .filter(|e| !e.malformed && matches!(e.expect, Expect::Filed(_)))
        .filter(|e| landed.contains_key(&e.rel))
        .count();
    assert_eq!(
        accounted, well_formed,
        "a malformed file must cost one file, not the run: {accounted} of {well_formed} \
         well-formed photographs reached the output"
    );
}

/// The binary is what a user runs, and it refuses a directory that already has
/// something in it. That refusal is the only thing standing between a slip of
/// the keyboard and several hundred synthetic photographs scattered through
/// somebody's real library.
#[test]
fn the_binary_refuses_a_non_empty_directory_without_force() {
    let dir = TempDir::new().expect("temp dir");
    std::fs::write(dir.path().join("holiday.jpg"), b"a real photograph").expect("writing");

    let refused = Command::cargo_bin("mmm-fixtures")
        .expect("mmm-fixtures builds")
        .arg(dir.path())
        .arg("--seed")
        .arg("1")
        .output()
        .expect("running mmm-fixtures");

    assert!(
        !refused.status.success(),
        "a non-empty directory is refused"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("not empty") && stderr.contains("--force"),
        "the refusal must say what is wrong and how to proceed anyway; it said:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("reading").count(),
        1,
        "a refused run writes nothing"
    );

    let forced = Command::cargo_bin("mmm-fixtures")
        .expect("mmm-fixtures builds")
        .arg(dir.path())
        .arg("--seed")
        .arg("1")
        .arg("--profile")
        .arg("minimal")
        .arg("--force")
        .output()
        .expect("running mmm-fixtures");
    assert!(
        forced.status.success(),
        "--force is the way to say you meant it"
    );
    assert!(
        dir.path().join("holiday.jpg").exists(),
        "--force adds files; it does not clear the directory"
    );
}

/// The binary writes the document, prints the seed, and names the profile.
#[test]
fn the_binary_writes_expected_md_and_prints_the_seed() {
    let dir = TempDir::new().expect("temp dir");
    let out = Command::cargo_bin("mmm-fixtures")
        .expect("mmm-fixtures builds")
        .arg(dir.path())
        .arg("--profile")
        .arg("minimal")
        .arg("--seed")
        .arg("4242")
        .output()
        .expect("running mmm-fixtures");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("4242"),
        "the seed is the whole reproduction and must be printed: {stdout}"
    );
    assert!(stdout.contains("minimal"), "the profile is named: {stdout}");

    let doc = std::fs::read_to_string(dir.path().join("EXPECTED.md")).expect("EXPECTED.md exists");
    assert!(doc.contains("seed **4242**"));
    assert!(
        doc.contains("mmm undo"),
        "a document that tells someone how to run a file-moving tool must tell them how to \
         reverse it"
    );
}

/// An unknown profile is refused by name, listing the ones that exist.
#[test]
fn the_binary_refuses_an_unknown_profile() {
    let dir = TempDir::new().expect("temp dir");
    let out = Command::cargo_bin("mmm-fixtures")
        .expect("mmm-fixtures builds")
        .arg(dir.path())
        .arg("--profile")
        .arg("photographs")
        .output()
        .expect("running mmm-fixtures");

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("photographs"), "name what was rejected");
    for name in Profile::ALL {
        assert!(stderr.contains(name), "list what would work: {stderr}");
    }
}
