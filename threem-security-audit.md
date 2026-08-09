# ThreeM (mmm) v0.3.0 — Deep Security Analysis

> **Target:** `t0lkim-FOSS/ThreeM` | **Date:** 2026-08-09 | **Scope:** Full source (45,232 lines) | **Language:** Rust (safe, no `unsafe` blocks)

## Overall Rating: LOW RISK

ThreeM has an exceptionally strong security posture for a v0.3.0 tool that moves other people's photo libraries. The codebase is pure safe Rust with zero `unsafe` blocks, enforced panic-safety via Clippy denials, comprehensive path-traversal prevention, crash-safe intent-before-move journaling, and a safe-by-default CLI that requires explicit `--commit` to touch files. No critical or high-severity findings. The issues found are low-severity hardening opportunities and one dependency advisory that doesn't reach exploitable code paths in this project.

| Critical / High | Medium | Low | Informational | Claims Verified | Claims Refuted |
|:---:|:---:|:---:|:---:|:---:|:---:|
| 0 | 2 | 4 | 5 | 14 | 0 |

---

## Attack Surface Assessment

ThreeM is a local CLI tool with no network operations. Its attack surface is entirely filesystem and parser-based.

| Surface | Description |
|---------|-------------|
| **EXIF Metadata Parsing** | nom-exif 1.5.2 parses camera EXIF data. Guarded by a 2-byte minimum file size check (prevents known panic at `jpeg.rs:110`). Fuzzed in CI. |
| **XMP Sidecar Parsing** | quick-xml 0.41 pull parser, namespace-aware. Deliberately refuses unprefixed names. Malformed XML warns and stops, keeping partial results. Fuzzed. |
| **TOML Config Files** | serde with `deny_unknown_fields`. Destructive keys (`commit`, `no_journal`, `i_know_what_im_doing`) explicitly refused with explanatory messages. |
| **Filesystem Operations** | `link(2)+unlink` no-clobber moves, `File::create_new()` for temps, BLAKE3 cross-volume verification, RAII cleanup via `TempFileGuard`. |
| **Journal Deserialisation** | JSONL with schema versioning. Truncated final lines tolerated; mid-file corruption refused. `sync_data()` per entry. Fuzzed. |
| **Path Construction** | `sanitise_for_filename()` strips separators + null bytes. Format patterns validated at construction: absolute paths, `..`, null bytes all rejected. |

---

## Findings

### F001 — Medium · anyhow 1.0.102 — RUSTSEC-2026-0190 unsoundness advisory

**Location:** `Cargo.toml:31` · `anyhow = "1"`

`anyhow` 1.0.102 has an unsoundness advisory in `Error::downcast_mut()`. ThreeM does NOT call `downcast_mut()` anywhere, so the unsound code path is unreachable. However, this is a direct dependency with a known advisory, and `cargo audit` flags it on every CI run.

**Impact:** No exploitable path exists in ThreeM's usage. The risk is reputational — users running `cargo audit` see a warning.

> **Fix:** Bump `anyhow` to the patched version once available. Pin `anyhow = "1.0.103"` or later in `Cargo.toml`.

### F002 — Medium · No size cap on EXIF/XMP parser input

**Location:** `metadata.rs` · `xmp.rs`

Neither the EXIF parser (nom-exif) nor the XMP reader (quick-xml) has an explicit input size cap imposed by ThreeM. A crafted multi-gigabyte EXIF segment or XMP sidecar would be loaded entirely into memory. While nom-exif reads via its own internal buffering, the XMP reader processes the entire file.

**Impact:** Denial of service via memory exhaustion. Requires an attacker to place a crafted file in the target library. SECURITY.md acknowledges DoS from large inputs as out of scope, but a multi-GB EXIF block inside a small JPEG is a more targeted vector than a large library.

> **Fix:** Cap XMP sidecar reads at a reasonable limit (e.g. 10 MB — no legitimate XMP sidecar approaches this). For EXIF, nom-exif handles its own reads, so the guard is the 2-byte minimum; consider refusing files with EXIF segments reported as >10 MB by the container parser before handing to nom-exif.

### F003 — Low · Predictable temp file naming in cross-volume moves

**Location:** `organiser.rs` · `copy_verify_delete()`

Cross-volume move temp files use a predictable name based on the destination path with a `.mmm.partial` suffix. While `File::create_new()` (O_CREAT|O_EXCL) prevents a symlink-at-temp-location attack, the predictability means an attacker with write access to the output directory could pre-create the temp file to cause a move failure (denial of service).

**Impact:** Move failure for specific files. SECURITY.md explicitly marks "filesystem races an attacker with write access" as out of scope. The `create_new` flag prevents data loss — the failure is clean.

> **Harden (optional):** Add a random suffix to the temp filename (e.g. `.mmm.partial.{random}`). Low priority given the out-of-scope classification and the `create_new` guard.

### F004 — Low · Nightly toolchain pin for fuzzing may miss sanitiser improvements

**Location:** `.github/workflows/ci.yml:237`

The fuzzing CI job is pinned to `nightly-2026-05-01` because current nightly breaks nom-exif 1.5.2 compilation. This means fuzzing hasn't picked up any AddressSanitizer improvements, new LLVM sanitiser checks, or libFuzzer features since May 2026.

**Impact:** Reduced fuzzing effectiveness over time. The pin is documented and justified, and the corpus-seeded regression testing still works.

> **Fix:** Monitor nom-exif for a release that compiles on current nightly. File an issue upstream if the breakage persists. Consider adding a scheduled CI job that attempts fuzzing on latest nightly and reports (non-blocking) whether it succeeds.

### F005 — Low · Dedup verifier manifest parsing trusts file content for path resolution

**Location:** `bin/mmm_dedup_verifier.rs:267-301`

The dedup verifier parses `manifest.txt` files using prefix stripping (`# Original kept at:`, `# Original moved to:`) to resolve the original file's path. A hand-crafted manifest could point the verifier at an arbitrary file path. However, the verifier is read-only — it only hashes files, never modifies them.

**Impact:** Information disclosure — an attacker who controls a manifest could cause the verifier to read and hash an arbitrary file. The hash is printed to stdout. No data modification is possible.

> **Harden (optional):** Validate that resolved original paths are within the expected output tree. Note that this is a secondary binary, not the main tool, and requires the attacker to already control the `duplicates/` directory.

### F006 — Low · No config file permission checks

**Location:** `settings.rs:973-987`

Config files (`~/.config/mmm/config.toml`, project `mmm.toml`/`.mmm.toml`) are read without checking file ownership or permissions. A world-writable config file in a shared environment could be modified by another user to alter behaviour (e.g. changing `skip_patterns` to skip files, or `duplicates_dir` to redirect duplicates).

**Impact:** Behaviour modification in shared-filesystem environments. Config files cannot enable `commit` mode (by design), so the worst case is altered preview output or changed file routing when the user explicitly commits. Standard for CLI tools — `git`, `rg`, `fd` all behave the same way.

> **Consider:** Log a warning when a config file is group- or world-writable. Low priority — consistent with ecosystem norms.

### F007 — Info · number_prefix 0.4.0 unmaintained (transitive via indicatif)

**Location:** `Cargo.lock` · indicatif → number_prefix

RUSTSEC-2025-0119 marks `number_prefix` as unmaintained. This is a transitive dependency through `indicatif` (progress bars). No security advisory exists — only an unmaintained status.

> **Monitor:** Wait for `indicatif` to update. No action required.

### F008 — Info · Release binaries are stripped — no debug symbol leakage

**Location:** `.github/scripts/package-release.sh:56-65`

The release packaging script asserts that binaries are stripped (`nm` check for ≤100 non-undefined symbols). `[profile.release] strip = true` is enforced by CI, preventing accidental debug symbol leakage in releases. This is a positive finding.

> **None needed.** The enforcement mechanism is correct.

### F009 — Info · CI uses minimal permissions and pinned actions

**Location:** `.github/workflows/ci.yml:17-18` · `release.yml:42-44`

CI workflows use `permissions: contents: read` globally, with `contents: write` only on the publish job. Actions are pinned to major versions (`@v7`, `@v4`, `@v2`). The release creates a draft (not public) that requires manual publish. Concurrency groups cancel in-progress runs. `--locked` prevents silent dependency updates in CI.

> **Harden (optional):** Pin actions to full commit SHAs instead of version tags to prevent tag-hijack supply chain attacks (e.g. `actions/checkout@<sha>`). This is an ecosystem-wide best practice, not a ThreeM-specific weakness.

### F010 — Info · No advisory lock against concurrent runs on the same directory

**Location:** `journal.rs` · `organiser.rs`

Two concurrent `mmm organise --commit` processes targeting the same output tree are not prevented. Journal creation uses `O_CREAT|O_EXCL` with a unique run ID, so journal file collision is near-impossible. The `link(2)` no-clobber provides per-file atomicity. However, interleaved journals could produce confusing undo behaviour.

**Impact:** Cosmetic — no data loss (per-file atomics are sound), but undo from one run's journal may reference files the other run already moved. Concurrent batch organise of the same library is an unlikely user scenario.

> **Consider:** Document that concurrent runs on the same output tree are unsupported. Optionally add an advisory lockfile in `.mmm/` for a clear early error.

### F011 — Info · Manifest.txt does not escape control characters in filenames

**Location:** `organiser.rs` · `GroupManifest::record_move()`

The human-readable `manifest.txt` writes source paths via `writeln!()` with no escaping. A file named with embedded newlines could inject misleading lines. The machine-readable JSONL journal properly escapes all strings via `serde_json`, and undo operates from the journal, never the manifest.

**Impact:** Cosmetic only — misleading manifest for human readers. No impact on tool correctness, undo, or verification (all operate from the JSONL journal).

> **None required.** Optionally replace control characters when rendering manifest paths.

---

## Security Claims Verification

Each documented security property was independently verified by reading the source code.

| Claim | Verdict | Evidence |
|-------|:-------:|---------|
| Dry-run is default; `--commit` required to move files | **Confirmed** | `config.rs`: `is_dry_run()` returns `!self.commit`; `commit` defaults `false`. `settings.rs`: `commit` cannot be set from config files — `COMMAND_LINE_ONLY_KEYS` array + `deny_unknown_fields` + explicit refusal message. |
| Intent recorded before move; `sync_data()` per entry | **Confirmed** | `journal.rs:250-270`: `append()` calls `write_all()` + `sync_data()` before returning. `organiser.rs`: `recorded_move()` appends `MoveIntent` BEFORE attempting `execute_move()`. Journal created with `create_new(true)` — no overwrite possible. |
| Same-volume moves use `link(2)+unlink`, never `rename` | **Confirmed** | `organiser.rs`: `move_no_clobber()` calls `link_and_unlink()`. `fs::rename` is explicitly mentioned as rejected in the doc comment ("silently overwrites"). Cross-volume fallback uses `copy_verify_delete()` with BLAKE3 hash comparison. |
| Symlinks are not followed during scanning | **Confirmed** | `scanner.rs`: `WalkDir::new(dir).follow_links(false)`. `undo.rs:317`: `fs::symlink_metadata()` (not `metadata()`) used for verification — a symlink at the destination is detected as a replacement, not followed. |
| Path traversal via metadata is prevented | **Confirmed** | `naming.rs`: `sanitise_for_filename()` replaces `/`, `\`, `\0`, and all non-alphanumeric chars (except `-`, `_`). Leading `.` → `_` prefix. `DateDirectoryFormat::new()` and `FilenameFormat::new()` both call `reject_escaping()` which refuses absolute paths, `..`, and null bytes. Test: `../../etc/passwd` → `______etc_passwd`. |
| EXIF parser panic on <2-byte files is guarded | **Confirmed** | `metadata.rs`: `MIN_PARSEABLE_BYTES = 2`; `is_too_short_to_parse()` checks file length BEFORE any parser call. Falls back to filesystem timestamp. Regression tests verify zero-byte and one-byte files are handled gracefully. |
| GPS coordinates reject NaN/Inf/out-of-range | **Confirmed** | `metadata.rs`: `parse_iso6709()` checks `is_nan()`, `is_infinite()`, latitude ±90, longitude ±180. Returns `None` on any failure. Found by the fuzzer. |
| XMP parsing is namespace-aware; unprefixed names ignored | **Confirmed** | `xmp.rs`: `NsReader` matches properties by namespace URI (e.g. `http://ns.adobe.com/exif/1.0/`), not just prefix. `property_of()` deliberately returns `None` for unprefixed names — prevents reading dates from arbitrary XML elements. |
| Dedup never deletes originals | **Confirmed** | `organiser.rs` + `hasher.rs`: the first file in each duplicate group (shallowest, then lexicographic) stays in place. Others move to `duplicates/NNN/` with a manifest. Independent keyed-BLAKE3 verifier confirms matches before any manual deletion. |
| Cross-volume moves verify content with BLAKE3 | **Confirmed** | `organiser.rs`: `copy_verify_delete()` copies to temp, hashes both source and temp with `hash_reader()`, compares digests. Source only unlinked AFTER digest match AND successful `promote_into_place()`. Digest mismatch returns an error; source preserved. |
| No `unsafe` blocks in the codebase | **Confirmed** | `rg 'unsafe' src/` returns only doc-comment and string-literal occurrences ("unsafe combination", "unsafe run"). Zero `unsafe {}` blocks. Pure safe Rust. |
| Panic safety: `unwrap`/`expect` denied in non-test code | **Confirmed** | `Cargo.toml`: `unwrap_used = "deny"`, `expect_used = "deny"`. Every `#[cfg(test)]` module carries a local `#[allow]`. CI runs `clippy -- -D warnings`, making this a hard gate. |
| Fuzz targets cover all untrusted-input parsers | **Confirmed** | `fuzz.rs` exposes four entry points: `parse_wall_clock` (EXIF dates), `parse_iso6709` (GPS), `xmp_date` (XMP sidecars), `journal_header_line` + `journal_entry_line` (journal deserialisation). CI runs 60s per target with AddressSanitizer, corpus-seeded, on both Linux and macOS. |
| `--no-journal --commit` requires `--i-know-what-im-doing` | **Confirmed** | `config.rs`: `validate()` checks the combination and returns an error with an explanatory message. All three flags are in `COMMAND_LINE_ONLY_KEYS` — cannot be set from config or environment. |

---

## Threat Model

### In-Scope Threats (per SECURITY.md)

- **Crafted media files causing parser crashes:** Mitigated by fuzz testing (4 targets, AddressSanitizer, CI-enforced), the 2-byte minimum guard, and GPS coordinate validation. Residual risk: unbounded allocation from oversized EXIF segments (F002).
- **Path traversal escaping the output directory:** Mitigated by `sanitise_for_filename()`, format pattern validation at construction time, and the `one_component()` guard. No bypass found.
- **Data loss from overwrites:** Mitigated by `link(2)+unlink` no-clobber, `File::create_new()` for temps, BLAKE3 verification on cross-volume, intent-before-move journaling. No bypass found.
- **Journal corruption preventing undo:** Mitigated by per-entry `sync_data()`, truncated-tail tolerance, mid-file-corruption refusal, schema versioning. Fuzz-tested.

### Out-of-Scope (acknowledged in SECURITY.md)

- DoS from large libraries — batch tool; expected.
- Filesystem races with write access — attacker with write access doesn't need `mmm`.
- `--no-journal --commit --i-know-what-im-doing` — behaves as documented, requires three flags.
- Dependency vulns with no path from `mmm` inputs — report upstream.

---

## Dependency Inventory

| Dependency | Version | Purpose | Status |
|------------|---------|---------|--------|
| `anyhow` | 1.0.102 | Error handling | **Advisory** RUSTSEC-2026-0190 (F001) |
| `blake3` | 1.8.4 | Content hashing | Clean |
| `chrono` | 0.4.44 | Date/time | Clean |
| `chrono-tz` | 0.10.4 | IANA timezones | Clean |
| `clap` | 4.6.0 | CLI parsing | Clean |
| `directories` | 6.0.0 | Config paths | Clean |
| `globset` | 0.4.19 | Skip patterns | Clean |
| `indicatif` | 0.17.11 | Progress bars | Clean (transitive: number_prefix unmaintained) |
| `nom-exif` | 1.5.2 | EXIF parsing | Clean |
| `quick-xml` | 0.41.0 | XMP parsing | Clean |
| `rayon` | 1.12.0 | Parallel hashing | Clean |
| `reverse_geocoder` | 4.1.1 | GPS → place names | Clean |
| `serde` | 1.0.228 | Serialisation | Clean |
| `toml` | 1.1.4 | Config parsing | Clean |
| `walkdir` | 2.5.0 | Directory walking | Clean |

---

## Hardening Already in Place

Strengths worth preserving — these are load-bearing safety properties.

- **Safe-by-default CLI:** dry-run default, `--commit` required, `--no-journal` requires acknowledgement flag — config files CANNOT override these.
- **Crash-safe journaling:** intent-before-move with per-entry `sync_data()`; truncated tails tolerated; schema versioned; undo is itself undoable.
- **No-clobber filesystem ops:** `link(2)+unlink` on same volume; `File::create_new()` everywhere; BLAKE3 content verification on cross-volume; RAII temp file cleanup.
- **Path sanitisation defence-in-depth:** format patterns validated at construction (not use); every token value sanitised; `one_component()` final guard; hostile-extension test in the suite.
- **Panic safety:** Clippy `deny` on `unwrap_used`/`expect_used`; CI enforces with `-D warnings`; graceful fallbacks throughout.
- **Comprehensive CI:** cross-platform (Linux + macOS), three timezone variants, MSRV testing, fuzzing with AddressSanitizer, coverage floors (organiser.rs 92%+, journal.rs 96%+), release binary architecture verification and smoke testing, stripped-symbols assertion.
- **Undo verification:** `symlink_metadata()` (not `metadata()`) in undo — detects symlinks at destinations; size + optional BLAKE3 hash check before any restore; unverifiable files refused, not guessed.
- **Independent dedup verifier:** separate binary using keyed BLAKE3 (not the main cascade's unkeyed mode) with full-file hashing (not partial). Different buffer size (256 KB vs 128 KB). A second opinion by design.

---

## Recommendations Summary

1. **Bump `anyhow`** to a version past RUSTSEC-2026-0190 (F001). One-line `Cargo.toml` edit.
2. **Cap XMP sidecar file size** before reading (F002). A 10 MB ceiling covers any legitimate sidecar.
3. **Pin CI actions to commit SHAs** (F009). Hardens against tag-hijack supply chain attacks.
4. **Monitor nom-exif nightly compat** (F004). Unpin fuzzing when the upstream fix lands.
5. Everything else is optional hardening — the existing posture is strong.

---

*Audit performed by deep source-code analysis of all 45,232 lines across 18 source files, 2 CI workflows, 2 shell scripts, and the full dependency tree. Cross-vendor verification via GPT-5.5 Forge audit mode confirmed all findings and contributed F010–F011. No code was modified during this audit.*
