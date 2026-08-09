//! Claims the documentation makes that the crate can check for itself.
//!
//! `docs/research/coverage-report.md` and the phase notes both record the same
//! finding: every stale claim found in this repository was found by a person
//! reading prose against code, which is how a `rename()` that had not existed
//! for two phases survived in `TECHNICAL.md`. Nothing stops that recurring.
//!
//! This file does not fix that in general — it pins the one fact that this
//! phase newly wrote down in three places at once. The minimum supported Rust
//! version is declared in `Cargo.toml` and then *repeated* in `README.md` and
//! `CONTRIBUTING.md`, and a floor stated differently in two documents is worse
//! than one stated nowhere: a contributor believes the wrong number and cannot
//! tell which is authoritative. `CARGO_PKG_RUST_VERSION` is the declaration
//! itself, so this asserts against the source rather than against a copy.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

/// `Cargo.toml`'s `rust-version`, as cargo saw it at compile time.
const DECLARED_MSRV: &str = env!("CARGO_PKG_RUST_VERSION");

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
