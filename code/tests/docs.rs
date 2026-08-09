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

    // The address itself is not pinned. It used to be, and the literal here was
    // a second copy of a fact that lives in SECURITY.md — so changing the
    // contact broke this test, which is the test reporting its own duplication
    // rather than a defect. What must hold is that *an* address is there and
    // reachable, not which one somebody chose.
    let has_contact = security
        .split_whitespace()
        .any(|word| word.contains('@') && word.contains('.') && word.len() > 5);
    assert!(
        has_contact,
        "SECURITY.md has no disclosure contact in it — a reporter arriving \
         there has nowhere to send anything"
    );

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
    }
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
