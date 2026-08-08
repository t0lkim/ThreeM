//! Integration suite for the configuration layers, driven through the real
//! `mmm` binary.
//!
//! ## Why the binary and not the library
//!
//! `settings.rs` already unit-tests the fold, the discovery walk and the
//! environment parser against pure functions, and it does so by passing
//! [`LoadOptions`] in rather than reading the process. That is the right shape
//! for those tests and it is precisely why it cannot answer the question this
//! suite asks: whether the *process* wires the same rules up. `--config` has to
//! reach `LoadOptions`, the working directory has to be where the project walk
//! starts, `XDG_CONFIG_HOME` has to reach `user_config_path`, and a
//! `ConfigError` has to leave through `main`'s `Result` with a non-zero status.
//! None of that is exercised by a test that constructs `LoadOptions` itself.
//!
//! [`LoadOptions`]: mmm::settings::LoadOptions
//!
//! ## The environment is an input, so the suite controls all of it
//!
//! Every command below runs with `XDG_CONFIG_HOME` pointed inside a `TempDir`
//! and with every inherited `MMM_` variable removed. Without the first, the
//! developer's own `~/.config/mmm/config.toml` would be a silent layer under
//! every assertion; without the second, an `MMM_CHUNK_SIZE` exported in the
//! shell that ran `cargo test` would outrank the files these tests write. Both
//! failures would be intermittent and would look like a bug in the tool.
//!
//! Note that nothing here calls `std::env::set_var`: the environment is set on
//! the child process. Rust test binaries are threaded, and one test's variable
//! is every concurrent test's variable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

use mmm::settings::DEFAULT_CHUNK_SIZE;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary world with somewhere to put each config layer.
///
/// Two directories, deliberately siblings: `home/` stands in for the platform
/// config directory (via `XDG_CONFIG_HOME`) and `project/` for the tree a run
/// is executed from. Keeping them apart means a test that writes a user config
/// cannot accidentally satisfy an assertion about the project one.
struct ConfigWorld {
    root: TempDir,
}

impl ConfigWorld {
    fn new() -> Self {
        let root = TempDir::new().expect("creating the config TempDir");
        fs::create_dir_all(root.path().join("home")).expect("creating the fixture config home");
        fs::create_dir_all(root.path().join("project")).expect("creating the fixture project dir");
        Self { root }
    }

    /// What the child sees as `XDG_CONFIG_HOME`.
    fn config_home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// Where the per-user config lives, whether or not it has been written.
    ///
    /// Deliberately not canonicalised: `user_config_path` joins the variable it
    /// was given without resolving it, so this is the exact string
    /// `mmm config show` prints.
    fn user_config_path(&self) -> PathBuf {
        self.config_home().join("mmm").join("config.toml")
    }

    fn write_user_config(&self, text: &str) -> PathBuf {
        let path = self.user_config_path();
        fs::create_dir_all(path.parent().unwrap()).expect("creating the user config dir");
        fs::write(&path, text).expect("writing the user config");
        path
    }

    /// The directory commands run from, unless a test says otherwise.
    fn project_dir(&self) -> PathBuf {
        self.root.path().join("project")
    }

    /// Where `mmm.toml` sits for the project layer, as the binary reports it.
    ///
    /// Canonicalised, because the project walk starts from the child's working
    /// directory and `getcwd` has already resolved every symlink in it — on
    /// macOS a `TempDir` under `/var` is reported under `/private/var`.
    fn project_config_path(&self) -> PathBuf {
        real(&self.project_dir()).join("mmm.toml")
    }

    fn write_project_config(&self, text: &str) -> PathBuf {
        let path = self.project_dir().join("mmm.toml");
        fs::write(&path, text).expect("writing the project config");
        self.project_config_path()
    }

    /// Create `rel` below the project directory and return it.
    fn subdir(&self, rel: &str) -> PathBuf {
        let path = self.project_dir().join(rel);
        fs::create_dir_all(&path).expect("creating a fixture subdirectory");
        path
    }

    /// A command that runs from the project directory.
    fn mmm(&self) -> Command {
        self.mmm_in(&self.project_dir())
    }

    /// A command that runs from `dir`, with the environment fully controlled.
    fn mmm_in(&self, dir: &Path) -> Command {
        let mut cmd = Command::cargo_bin("mmm").expect("locating the mmm binary");
        cmd.current_dir(dir)
            .env("XDG_CONFIG_HOME", self.config_home());
        // Whatever the developer or CI exported is not part of this test.
        for (key, _) in std::env::vars() {
            if key.starts_with("MMM_") {
                cmd.env_remove(key);
            }
        }
        cmd
    }
}

/// `path` with symlinks resolved, or `path` itself if it cannot be resolved.
fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Assert the process exited 0, printing both streams if it did not.
fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        stdout_of(out),
        stderr_of(out),
    );
}

/// Assert the process failed, and hand back its stderr for inspection.
///
/// A config that cannot be understood has to stop the run with a non-zero
/// status, not merely print something: the caller that notices is a script.
fn assert_failed(out: &std::process::Output, what: &str) -> String {
    assert!(
        !out.status.success(),
        "{what} succeeded, and should not have\n--- stdout ---\n{}\n--- stderr ---\n{}",
        stdout_of(out),
        stderr_of(out),
    );
    stderr_of(out)
}

/// The `mmm config show` line for `key`, whole, so a failure prints the
/// annotation as well as the value.
///
/// Panics when the key is absent, which is the correct outcome: `config show`
/// listing every setting is the property `settings_report`'s catalogue test
/// pins, and a key going missing here should not read as a value mismatch.
fn show_line(stdout: &str, key: &str) -> String {
    let prefix = format!("{key} = ");
    stdout
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("`config show` printed no `{key}` line:\n{stdout}"))
        .to_string()
}

/// Assert that `config show` reports `key = value`, decided by `source`.
///
/// Both halves in one assertion because either alone is satisfiable by the
/// wrong thing: the right value can come from a layer that was not supposed to
/// win, and the right source annotation on the wrong value would mean the fold
/// and the explanation disagree.
fn assert_shows(stdout: &str, key: &str, value: &str, source: &str) {
    let line = show_line(stdout, key);
    assert_eq!(
        line,
        format!("{key} = {value}  # from: {source}"),
        "`config show` disagreed about {key}\n--- full output ---\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

/// The whole precedence rule, one layer at a time, on one key.
///
/// Staged rather than split into four tests because each stage's claim is
/// *overridden by*, not *read from*: a test that only ever saw a project config
/// would pass whether or not the user config beneath it was outranked or simply
/// never read. Every stage leaves the layers below it in place and asserts a
/// different answer.
#[test]
fn each_layer_overrides_the_one_below_it() {
    let world = ConfigWorld::new();

    // --- built-in defaults, with nothing written anywhere ---
    let out = world.mmm().args(["config", "show"]).output().unwrap();
    assert_ok(&out, "config show with no config anywhere");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        &DEFAULT_CHUNK_SIZE.to_string(),
        "built-in defaults",
    );

    // --- user config beats the defaults ---
    let user = world.write_user_config("chunk_size = 10\n");
    let out = world.mmm().args(["config", "show"]).output().unwrap();
    assert_ok(&out, "config show with a user config");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        "10",
        &format!("user config ({})", user.display()),
    );

    // --- project config beats the user config ---
    let project = world.write_project_config("chunk_size = 20\n");
    let out = world.mmm().args(["config", "show"]).output().unwrap();
    assert_ok(&out, "config show with a project config");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        "20",
        &format!("project config ({})", project.display()),
    );

    // --- the environment beats both files ---
    let out = world
        .mmm()
        .env("MMM_CHUNK_SIZE", "30")
        .args(["config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show with MMM_CHUNK_SIZE set");
    assert_shows(&stdout_of(&out), "chunk_size", "30", "environment");

    // --- the command line beats everything ---
    let out = world
        .mmm()
        .env("MMM_CHUNK_SIZE", "30")
        .args(["--chunk-size", "40", "config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show with --chunk-size");
    assert_shows(&stdout_of(&out), "chunk_size", "40", "command line");
}

// ---------------------------------------------------------------------------
// What a broken config does
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_key_names_the_key() {
    let world = ConfigWorld::new();
    let project = world.write_project_config("chunck_size = 20\n");

    let out = world.mmm().args(["config", "show"]).output().unwrap();
    let stderr = assert_failed(&out, "config show over a config with a typo'd key");

    assert!(
        stderr.contains("chunck_size"),
        "the refusal did not name the key that is wrong:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("{}:1:1", project.display())),
        "the refusal did not name the file and position:\n{stderr}"
    );
}

#[test]
fn malformed_toml_names_the_file_and_the_position() {
    let world = ConfigWorld::new();
    // Line 1 parses; line 2 is a bare word where a value belongs, so the
    // reported position has to be the second line rather than the first.
    let project = world.write_project_config("chunk_size = 10\nno_prompt = yes\n");

    let out = world.mmm().args(["config", "show"]).output().unwrap();
    let stderr = assert_failed(&out, "config show over malformed TOML");

    assert!(
        stderr.contains(&format!("{}:2:13", project.display())),
        "the refusal did not name the file and the line and column of the problem:\n{stderr}"
    );
}

#[test]
fn a_config_file_may_not_turn_commit_on() {
    let world = ConfigWorld::new();
    let project = world.write_project_config("commit = true\n");

    let out = world.mmm().args(["config", "show"]).output().unwrap();
    let stderr = assert_failed(&out, "config show over a config setting commit");

    // The message a reader can act on, rather than the eleven-field list
    // `deny_unknown_fields` would otherwise produce.
    assert!(
        stderr.contains("`commit` cannot be set here"),
        "the refusal did not say that commit is not a setting:\n{stderr}"
    );
    assert!(
        stderr.contains("Pass --commit on the command line instead"),
        "the refusal did not say where commit does belong:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("{}:1:1", project.display())),
        "the refusal did not name the file and position:\n{stderr}"
    );
}

/// A file that cannot be understood must stop `organise` before it scans.
///
/// The refusal above is asserted through `config show`, which moves nothing.
/// This is the case the rule exists for: the alternative to stopping is
/// carrying on with the defaults, which for a library whose config sets an
/// output directory means organising it somewhere nobody asked for.
#[test]
fn a_broken_config_stops_an_organise_run_before_it_scans() {
    let world = ConfigWorld::new();
    world.write_project_config("chunck_size = 20\n");
    let library = world.subdir("library");

    let out = world.mmm().arg(&library).output().unwrap();
    let stderr = assert_failed(&out, "an organise run under a broken config");

    assert!(
        stderr.contains("chunck_size"),
        "the refusal did not name the key that is wrong:\n{stderr}"
    );
    assert!(
        !stdout_of(&out).contains("Scanning directories"),
        "the run scanned before reading its configuration:\n{}",
        stdout_of(&out)
    );
}

// ---------------------------------------------------------------------------
// Choosing which files are read
// ---------------------------------------------------------------------------

#[test]
fn no_config_ignores_both_discovered_files() {
    let world = ConfigWorld::new();
    world.write_user_config("chunk_size = 10\n");
    world.write_project_config("chunk_size = 20\n");

    let out = world
        .mmm()
        .args(["--no-config", "config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show with --no-config");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        &DEFAULT_CHUNK_SIZE.to_string(),
        "built-in defaults",
    );
}

/// `--no-config` is a statement about files, and the environment is not one.
///
/// Asserted alongside the test above because "ignores the config" and "ignores
/// the config *files*" are different rules, and only the second is the one
/// implemented — a reader who assumed the first would use `--no-config` to
/// escape an `MMM_` variable and be quietly wrong.
#[test]
fn no_config_leaves_the_environment_standing() {
    let world = ConfigWorld::new();
    world.write_user_config("chunk_size = 10\n");
    world.write_project_config("chunk_size = 20\n");

    let out = world
        .mmm()
        .env("MMM_CHUNK_SIZE", "30")
        .args(["--no-config", "config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show with --no-config and MMM_CHUNK_SIZE");
    assert_shows(&stdout_of(&out), "chunk_size", "30", "environment");
}

#[test]
fn an_explicit_config_that_is_not_there_is_an_error_not_a_fallback() {
    let world = ConfigWorld::new();
    // A discoverable config exists, so falling back would be *visible* rather
    // than merely defaulting — and would still be the wrong answer.
    world.write_project_config("chunk_size = 20\n");
    let missing = world.project_dir().join("nowhere.toml");

    let out = world
        .mmm()
        .arg("--config")
        .arg(&missing)
        .args(["config", "show"])
        .output()
        .unwrap();
    let stderr = assert_failed(&out, "config show with --config naming a missing file");

    assert!(
        stderr.contains(&format!("no config file at {}", missing.display())),
        "the refusal did not name the file that is not there:\n{stderr}"
    );
    assert!(
        !stdout_of(&out).contains("chunk_size ="),
        "the run resolved settings despite the named config being absent:\n{}",
        stdout_of(&out)
    );
}

/// `--config` replaces discovery rather than adding to it.
///
/// Paired with the test above: together they say that the flag names *the*
/// configuration, so a missing file cannot be papered over and a present one
/// cannot be quietly topped up from `$HOME`.
#[test]
fn an_explicit_config_replaces_the_discovered_ones() {
    let world = ConfigWorld::new();
    world.write_user_config("chunk_size = 10\nno_prompt = true\n");
    world.write_project_config("chunk_size = 20\n");

    let explicit = world.project_dir().join("elsewhere.toml");
    fs::write(&explicit, "chunk_size = 50\n").unwrap();

    let out = world
        .mmm()
        .arg("--config")
        .arg(&explicit)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show with --config");
    let stdout = stdout_of(&out);

    assert_shows(
        &stdout,
        "chunk_size",
        "50",
        &format!("explicit config ({})", explicit.display()),
    );
    // The user config's `no_prompt` is not inherited: it was not read at all.
    assert_shows(&stdout, "no_prompt", "false", "built-in defaults");
}

#[test]
fn project_discovery_walks_up_from_a_nested_subdirectory() {
    let world = ConfigWorld::new();
    let project = world.write_project_config("chunk_size = 20\n");
    let nested = world.subdir("one/two/three");

    let out = world
        .mmm_in(&nested)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show from a nested subdirectory");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        "20",
        &format!("project config ({})", project.display()),
    );
}

/// The walk stops at the first hit, so the nearest config wins.
///
/// The rule that makes nesting usable at all: a config in a subdirectory is a
/// statement about that subdirectory, and one several levels up that outranked
/// it could never be overridden.
#[test]
fn the_nearest_project_config_wins() {
    let world = ConfigWorld::new();
    world.write_project_config("chunk_size = 20\n");
    let nested = world.subdir("one/two");
    fs::write(nested.join("mmm.toml"), "chunk_size = 21\n").unwrap();

    let out = world
        .mmm_in(&nested)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert_ok(&out, "config show beside a nearer project config");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        "21",
        &format!(
            "project config ({})",
            real(&nested).join("mmm.toml").display()
        ),
    );
}

// ---------------------------------------------------------------------------
// `config init`
// ---------------------------------------------------------------------------

#[test]
fn config_init_refuses_to_overwrite_without_force() {
    let world = ConfigWorld::new();
    // Valid TOML, so the ambient load succeeds and the refusal under test is
    // the one `init` makes rather than one the loader made first.
    let existing = world.write_project_config("chunk_size = 20\n");

    let out = world
        .mmm()
        .args(["config", "init", "--project"])
        .output()
        .unwrap();
    let stderr = assert_failed(&out, "config init over an existing file");

    assert!(
        stderr.contains(&format!("{} already exists", existing.display()))
            && stderr.contains("--force"),
        "the refusal did not name the file and the way past it:\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        "chunk_size = 20\n",
        "the refused `config init` wrote to the file anyway"
    );
}

#[test]
fn config_init_force_overwrites_and_writes_a_config_that_parses() {
    let world = ConfigWorld::new();
    let existing = world.write_project_config("chunk_size = 20\n");

    let out = world
        .mmm()
        .args(["config", "init", "--project", "--force"])
        .output()
        .unwrap();
    assert_ok(&out, "config init --force");

    let written = fs::read_to_string(&existing).unwrap();
    assert_ne!(
        written, "chunk_size = 20\n",
        "`config init --force` left the old file in place"
    );

    // The starter file has to be readable by the tool that wrote it — every key
    // commented out means the run that follows resolves to the defaults.
    let out = world.mmm().args(["config", "show"]).output().unwrap();
    assert_ok(&out, "config show over the starter config");
    assert_shows(
        &stdout_of(&out),
        "chunk_size",
        &DEFAULT_CHUNK_SIZE.to_string(),
        "built-in defaults",
    );
}

#[test]
fn config_init_writes_the_user_config_by_default() {
    let world = ConfigWorld::new();

    let out = world.mmm().args(["config", "init"]).output().unwrap();
    assert_ok(&out, "config init with no target");

    assert!(
        world.user_config_path().is_file(),
        "`config init` did not write {}",
        world.user_config_path().display()
    );
    assert!(
        !world.project_dir().join("mmm.toml").exists(),
        "`config init` wrote a project config when it was asked for a user one"
    );
}
