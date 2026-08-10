//! Claims the documentation makes that the crate can check for itself.
//!
//! `docs/research/coverage-report.md` and the phase notes both record the same
//! finding: every stale claim found in this repository was found by a person
//! reading prose against code, which is how a `rename()` that had not existed
//! for two phases survived in `TECHNICAL.md`. Nothing stops that recurring.
//!
//! This file does not fix that in general. It pins the handful of facts that
//! are *declared* in one place and then repeated in prose, where the repetition
//! is what goes stale:
//!
//! * The minimum supported Rust version, declared in `Cargo.toml` and repeated
//!   in `README.md` and `CONTRIBUTING.md`. A floor stated differently in two
//!   documents is worse than one stated nowhere: a contributor believes the
//!   wrong number and cannot tell which is authoritative.
//!   `CARGO_PKG_RUST_VERSION` is the declaration itself, so this asserts
//!   against the source rather than against a copy.
//! * The set of binaries the crate ships, declared as `[[bin]]` tables and
//!   repeated across three documents — which is the drift this suite was
//!   extended for, `mmm-fixtures` having shipped in 0.3.x while all three went
//!   on describing a two-binary tool.
//! * The disclosure contact, and what the bug template has to ask for.
//!
//! What it cannot pin is prose that *explains* rather than repeats — the
//! `awkward` trailer below is the one place it tries, and the compromise it
//! makes is written up at that test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// Shared with `tests/release.rs`, which checks the same binary list against the
// packaging script rather than against the documentation.
#[path = "common/repo.rs"]
mod repo;
use repo::{declared_binaries, read};

/// `Cargo.toml`'s `rust-version`, as cargo saw it at compile time.
const DECLARED_MSRV: &str = env!("CARGO_PKG_RUST_VERSION");

#[test]
fn the_readme_states_the_msrv_the_crate_declares() {
    let readme = read("README.md");
    assert!(
        readme.contains(DECLARED_MSRV),
        "README.md does not mention Rust {DECLARED_MSRV}, which is the \
         `rust-version` in code/Cargo.toml. Either the floor moved and the \
         README was not updated, or the README names a different one."
    );
}

#[test]
fn contributing_states_the_msrv_the_crate_declares() {
    let contributing = read("CONTRIBUTING.md");
    assert!(
        contributing.contains(DECLARED_MSRV),
        "CONTRIBUTING.md does not mention Rust {DECLARED_MSRV}, which is the \
         `rust-version` in code/Cargo.toml."
    );
}

/// The disclosure route has to exist and has to be reachable from the places a
/// reporter actually looks. A contact that only one document knows about is a
/// contact nobody uses.
#[test]
fn the_security_policy_carries_a_disclosure_contact() {
    let security = read("SECURITY.md");

    // Read out of SECURITY.md rather than written here, for the reason
    // `DECLARED_MSRV` is `env!("CARGO_PKG_RUST_VERSION")` rather than "1.87.0":
    // a literal in this file is a second copy of a fact, and the copy goes
    // stale the first time somebody changes the original. This test used to
    // hold that copy, and changing the contact address broke it — the test
    // reporting its own duplication rather than a defect in the documentation.
    let contacts = disclosure_addresses(&security);
    assert_eq!(
        contacts.len(),
        1,
        "SECURITY.md should name exactly one disclosure address, and names {}: \
         {contacts:?}. A reporter cannot choose between two, and none leaves \
         them nowhere to send anything.",
        contacts.len()
    );
    let contact = &contacts[0];

    for (name, body) in [
        ("README.md", read("README.md")),
        ("CONTRIBUTING.md", read("CONTRIBUTING.md")),
        (
            ".github/ISSUE_TEMPLATE/bug_report.md",
            read(".github/ISSUE_TEMPLATE/bug_report.md"),
        ),
    ] {
        assert!(
            body.contains("SECURITY.md"),
            "{name} does not point at SECURITY.md, so a reporter arriving there \
             has nowhere to be sent"
        );

        // If a document names an address at all, it has to name the one
        // SECURITY.md does. Nothing but SECURITY.md carries one today, and this
        // is what keeps that true: a second address added to the README later
        // is a reporter sending a vulnerability somewhere nobody reads.
        for found in disclosure_addresses(&body) {
            assert_eq!(
                &found, contact,
                "{name} names {found}, but SECURITY.md names {contact}. Two \
                 disclosure addresses in one repository means one of them is \
                 wrong and a reporter cannot tell which."
            );
        }
    }
}

/// Every email address a document names.
///
/// Deliberately crude — a whitespace split with the surrounding markdown
/// stripped — because the alternative is a regex crate carried by every build
/// of a photo organiser to read four documents in one test. It finds
/// `**security@t0lkim.dev**` and `<a@b.dev>` alike, which is the shape these
/// files actually use.
fn disclosure_addresses(body: &str) -> Vec<String> {
    let mut found: Vec<String> = body
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '.'))
        .filter(|word| {
            // An address, not a version number or a `@v7` action pin: one `@`,
            // something either side of it, and a dot in the domain.
            word.matches('@').count() == 1
                && word.split('@').next().is_some_and(|user| !user.is_empty())
                && word
                    .split('@')
                    .nth(1)
                    .is_some_and(|domain| domain.contains('.') && !domain.ends_with('.'))
        })
        .map(ToString::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The template is only useful if it asks for the four things that make a
/// report reproducible. Someone tidying it later should be told which of them
/// they removed, rather than finding out from an unreproducible bug.
#[test]
fn the_bug_template_asks_for_what_makes_a_report_reproducible() {
    let template = read(".github/ISSUE_TEMPLATE/bug_report.md");

    for required in [
        "mmm --version", // the version
        "Operating system",
        "The exact command",
        "journal", // whether the run left a journal
    ] {
        assert!(
            template.contains(required),
            "the bug report template no longer asks for {required:?}"
        );
    }

    // GitHub only treats the file as a template if it opens with front matter
    // naming it; without this it renders as an ordinary document nobody sees.
    assert!(
        template.starts_with("---\nname: Bug report\n"),
        "the template is missing the front matter GitHub needs to offer it"
    );
}

/// The documents a reader is sent to before they run anything.
const USER_FACING_DOCS: &[&str] = &["README.md", "docs/USER-GUIDE.md", "CONTRIBUTING.md"];

/// Any wording that tells a reader the `awkward` profile's breakage is intended
/// rather than a defect. Matched against a lowercased body, so a
/// sentence-initial "Deliberately" counts.
const INTENT_PHRASINGS: &[&str] = &[
    "deliberately malformed",
    "meant to be wrong",
    "correct result",
    "correct outcome",
];

/// A binary that ships and is documented nowhere is a binary nobody runs.
///
/// This is the drift that made the extension necessary rather than a
/// hypothetical: `mmm-fixtures` was added as a third `[[bin]]`, wired into the
/// packaging script — which `release.rs` next door checks — and installed onto
/// people's `PATH` by `cargo install`, while `README.md`, `docs/USER-GUIDE.md`
/// and `CONTRIBUTING.md` all went on describing a two-binary tool. Nothing
/// failed. The release was correct and the documentation was wrong, which is
/// the pairing no build error can distinguish from the reverse.
///
/// The list comes from `[[bin]]` rather than from a literal here, so adding a
/// fourth binary is what makes this cover it. One honest weakness: `mmm` is a
/// substring of `mmm-fixtures` and of the crate's own name, so its own
/// assertion is nearly free — the check has teeth for every binary whose name
/// is not a prefix of the others, which is the case that has actually gone
/// wrong.
#[test]
fn every_binary_the_crate_ships_is_named_in_the_user_facing_documentation() {
    let documents: Vec<(&str, String)> = USER_FACING_DOCS
        .iter()
        .map(|name| (*name, read(name)))
        .collect();

    for binary in declared_binaries() {
        for (name, body) in &documents {
            assert!(
                body.contains(&binary),
                "code/Cargo.toml ships a `{binary}` binary and {name} never names it. \
                 Either document it, or drop the [[bin]] entry — a binary on somebody's \
                 PATH that no document mentions is one they will never run."
            );
        }
    }
}

/// The `awkward` profile generates files that are *supposed* to be broken, and
/// the run over them is supposed to print warnings. Without being told that, a
/// reader gets a correct result and files a bug about it — so the sentence that
/// tells them is load-bearing, and until now nothing checked it was still
/// printed. The scenario suite began *executing* it when it started driving
/// both profiles; executing a line is not asserting on what it says.
///
/// Two halves, because the claim has two homes and they fail separately:
///
/// * The binary must print it for `awkward` **and must not print it for
///   `realistic`**. The negative is what makes this a test — a trailer printed
///   unconditionally would satisfy the positive while telling a user with a
///   well-formed library that their files are deliberately malformed.
/// * Each user-facing document that describes the profile must make the same
///   claim in prose.
///
/// The prose half is matched against a set of accepted phrasings rather than
/// one exact sentence, and that is a deliberate compromise. The three documents
/// word it three different ways on purpose — the trailer is terse because it is
/// a terminal line, the guide is expansive because it has room. Pinning the
/// exact wording of each would fail on a rewrite that improved it, which trains
/// people to update the test without reading it. Pinning "the claim is still
/// made somewhere in this document" fails on a deletion, which is the failure
/// that matters.
#[test]
fn the_awkward_profile_tells_the_reader_its_files_are_meant_to_be_broken() {
    let tmp = tempfile::tempdir().unwrap();

    let awkward = fixtures_stdout(&tmp.path().join("awkward"), "awkward");
    assert!(
        awkward.contains("deliberately malformed"),
        "mmm-fixtures --profile awkward no longer says its files are deliberately \
         malformed. A reader who is not told will report the warnings as a bug. \
         It printed:\n{awkward}"
    );
    assert!(
        awkward.contains("correct result"),
        "mmm-fixtures --profile awkward says the files are malformed but no longer \
         says the warnings are the correct outcome, which is the half that stops a \
         bug report. It printed:\n{awkward}"
    );

    let realistic = fixtures_stdout(&tmp.path().join("realistic"), "realistic");
    assert!(
        !realistic.contains("deliberately malformed"),
        "mmm-fixtures prints the awkward-profile warning for --profile realistic, \
         whose files are well-formed. It printed:\n{realistic}"
    );

    for name in USER_FACING_DOCS {
        let body = read(name).to_lowercase();
        if !body.contains("awkward") {
            // CONTRIBUTING.md has no reason to describe the profiles, and a
            // test that forced it to would be inventing a requirement.
            continue;
        }
        assert!(
            INTENT_PHRASINGS.iter().any(|phrase| body.contains(phrase)),
            "{name} describes the `awkward` profile without saying anywhere that its \
             broken files are deliberate. Say so in any wording; this test accepts \
             {INTENT_PHRASINGS:?}. A reader who is not told reports a correct run as \
             a bug."
        );
    }
}

/// Build a library with the shipped binary and hand back what it printed.
fn fixtures_stdout(dir: &std::path::Path, profile: &str) -> String {
    let output = assert_cmd::Command::cargo_bin("mmm-fixtures")
        .unwrap()
        .arg(dir)
        .arg("--profile")
        .arg(profile)
        // Pinned so the two runs differ only in profile: an unseeded run picks
        // from the wall clock, and a trailer that depended on which files
        // happened to be generated would fail here once in a while rather than
        // when it broke.
        .arg("--seed")
        .arg("1")
        .output()
        .expect("running mmm-fixtures");

    assert!(
        output.status.success(),
        "mmm-fixtures --profile {profile} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
