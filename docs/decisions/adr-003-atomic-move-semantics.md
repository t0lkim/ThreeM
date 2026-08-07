---
type: decision
title: Atomic move semantics
created: 2026-08-08
tags:
  - safety
  - filesystem
  - atomicity
related:
  - '[[adr-001-dry-run-by-default]]'
  - '[[CHANGELOG]]'
  - '[[TECHNICAL]]'
---

# ADR-003: Atomic move semantics

**Status:** Accepted
**Date:** 2026-08-08

## Problem

`mmm` moves irreplaceable files. Every move it performs is, from the user's point of view, the only copy of a photograph changing address, and the tool holds no journal that could put one back. The move primitive therefore has to be right in a way ordinary application code does not, because the failure mode is not a bad exit code — it is a missing photograph the user will not notice for a year.

Before this phase the primitive was `fs::rename`, guarded by an `exists()` check, with a copy-and-delete fallback verified by file size. Each of those three parts loses files:

**`fs::rename` overwrites.** POSIX `rename(2)` replaces the destination silently and unconditionally, and stable `std` exposes no flag asking it not to. The guard was `resolve_collision`, which called `Path::exists()` and appended `-1` if the answer was yes. That is wrong twice over. It is wrong under concurrency — the answer is stale the instant it is returned, and anything that claims the name in the window between the check and the rename is destroyed. And it is wrong with no concurrency at all: `Path::exists()` **follows symlinks**, so a *dangling* symlink at the destination reads as "nothing here", and the rename overwrites it. That second case is the deterministic form of the same defect. No race, no second thread, no timing — just a check asking the wrong question, reproducible on demand, which is what made it testable.

**Every rename failure was read as "different volume."** The code caught any `Err` from `rename` and fell through to copy-and-delete. `EACCES` on a read-only destination directory, `ENOENT` on a source deleted since planning, `EROFS`, `ENOSPC` — all of them became a copy attempt, which then failed for the same underlying reason but reported itself as a temp-file error. The operator asked to move `holiday.jpg` into `output/2024/01/15` and was told `copying …/holiday.jpg to temp file: Permission denied`. The real problem was named nowhere in the message.

**The copy was verified by size.** `cross_volume_move` compared `metadata().len()` on the source and the temp file, and if they matched it called `fs::remove_file` on the source. A copy that came off a failing drive, through a bad cable, or through a filesystem that silently substituted a block has exactly the right length and the wrong contents. Size equality is not content equality, and the gap between them is where the only copy of the file gets deleted.

## Decision

Three contracts, each stated as a property the code cannot violate rather than a rule it is expected to follow.

### 1. A move never overwrites. The destination refuses; the caller renames.

The same-volume primitive is `link_and_unlink`: `fs::hard_link(src, dst)`, then `fs::remove_file(src)`.

`link(2)` fails `EEXIST` if **anything** occupies `dst` — a regular file, a directory, a live symlink, a dangling symlink. That is precisely the question `Path::exists()` gets wrong, asked of the kernel at the moment of the operation instead of ahead of it. There is no window, because there is no separate check.

The collision logic is restructured around this. `resolve_collision(&Path) -> PathBuf` is gone; `collision_candidate(&Path, attempt) -> PathBuf` replaces it and touches the filesystem not at all — attempt 0 is the path, attempt *n* is `stem-n.ext`. `execute_move` walks candidates `0..MAX_COLLISION_ATTEMPTS` (10 000) and lets `move_no_clobber` be the sole authority on whether a name is free. It answers by failing. Exhausting the range is an error naming both paths, never a fallback that overwrites.

`link` + `unlink` is two syscalls where `rename` is one, and it is not atomic the way rename is: there is a window in which both names point at the same inode. The window contains no state in which data is missing, which is the property that matters — and the failure path closes it deliberately. If the link lands but the source will not unlink, the *new link is removed* rather than leaving two names for one file, which the dedup pass would otherwise later report as a duplicate of itself.

### 2. A cross-volume move deletes the source only after the destination's bytes are proved identical by digest.

`copy_verify_delete(src, dst, copy)`: copy the source to a temp file beside the destination, hash the landed file, compare against the digest of what was read, promote the temp file into place, and only then remove the source. On mismatch the temp file goes and the source stays, and the error states **both digests** — the operator can tell a corrupted copy from a missing file.

Details that are load-bearing rather than incidental:

- The copy is `File::create_new`, not `fs::copy`. `fs::copy` truncates whatever it finds, so the copy step itself would be a second overwrite path.
- `create_new` does not carry the source's mode across the way `fs::copy` does, so permissions are set explicitly. Without that a read-only original quietly comes out writable.
- The copy is `sync_all`ed before it is verified and before the source is removed. The caller is about to delete the only other copy, and a file still sitting in the page cache is not yet a copy.
- The hash is `hasher::hash_reader`, the same primitive dedup uses. Two implementations of "is this the same file" are two chances to disagree, and the one place they would disagree is immediately before `remove_file` on somebody's only photograph.
- Temp-file cleanup is a **drop guard**, not one `let _ = remove_file` per early return. There are six ways out of this function; scattering the cleanup means the next early return added is the one that forgets it, and the symptom is `.tmp-1748…` files accumulating in a photo library, indistinguishable from the photos except by name. The guard is disarmed only *after* a successful promotion — after, not before, because on the `reserve_and_rename` fallback the temp path is free again and a later move could already have claimed it.

The `copy` step is a parameter. That is what makes the defect testable at all: real corruption needs a failing drive, a bad cable, or a filesystem that substitutes a block, and none of those can be arranged in a test — but all of them do their damage in exactly one place, between reading the source and writing the copy. The injected step is honest about what it read and dishonest only about what it wrote, which is the shape of the real thing.

### 3. Only two conditions justify copying. Everything else stops.

A failed `link` is classified before it is acted on, by `classify_link_failure(&io::Error) -> LinkFailure` — a pure function over the error, which is what makes the routing testable without a second mounted volume.

| Condition | Classification | Consequence |
|---|---|---|
| `EEXIST` / `ErrorKind::AlreadyExists` | `DestinationTaken` | Try the next candidate name. |
| `EXDEV` / `ErrorKind::CrossesDevices` | `DifferentVolume` | The copy path. |
| `EPERM`, `ENOTSUP`/`EOPNOTSUPP`, `ErrorKind::Unsupported` | `LinksUnsupported` | The copy path — the filesystem has no hard links to give. |
| `EACCES`, `ENOENT`, `EROFS`, `ENOSPC`, anything else | `Fatal` | Propagated with `moving {src} to {dst}` context. **No copy attempted.** |

**`EPERM` and `EACCES` are the reason the raw errno is consulted at all.** Both arrive as `ErrorKind::PermissionDenied`, and here they mean opposite things: `link` answers `EPERM` when the filesystem has no hard links, and `EACCES` when the caller may not write to the directory. One is a legitimate copy; the other is a hard stop that must reach the operator naming the destination they actually asked about. `ENOTSUP` has no `ErrorKind` mapping at all and arrives as `Uncategorized`.

## Platform differences

The contract is uniform; the mechanism reaching it is not.

**`renameat2(RENAME_NOREPLACE)` and `renamex_np(RENAME_EXCL)` would be the one-syscall answer and are not used.** Linux ≥3.15 and macOS both expose a no-replace rename, but neither is reachable from stable `std`, and reaching them means adding `libc` and two `cfg`-split call sites for a primitive whose whole job is to be obviously correct. `link` + `unlink` needs no new dependency and answers the same question. If `std` ever stabilises a no-clobber rename this is the one place that changes.

**Errno numbers are `#[cfg(unix)]` and hand-declared.** A small documented `errno` module carries `EPERM` and the `ENOTSUP`/`EOPNOTSUPP` pair rather than pulling `libc` in for six integers. macOS and Linux agree on `EPERM = 1`; they differ on `ENOTSUP` (macOS 45, and 102 for `EOPNOTSUPP`; Linux 95 for both), which is `cfg`-split. `EXDEV` is deliberately *absent* from that module — `ErrorKind::CrossesDevices` is stable on 1.92 and std already maps the errno to it, so a raw check would be a second spelling of the same test.

**Filesystems without hard links get a different promotion step.** exFAT and FAT32 — which is what most SD cards and many external drives are formatted as — support no hard links at all, so `link` can never succeed there even when both paths are on the same volume. That is why `LinksUnsupported` routes to the copy path, and why the *promotion* at the end of that path does not require a link either. `promote_into_place` tries `link_and_unlink` and falls back to `reserve_and_rename`, which claims the destination with `O_CREAT | O_EXCL` and then renames the temp file over the placeholder it now owns. `create_new` asks the same question `link` answers with `EEXIST` — including refusing a dangling symlink, since `O_CREAT | O_EXCL` fails on a symlink whether or not its target exists — in a form FAT32 can answer, and it is atomic against another writer claiming the name first.

That `rename` is **the only overwrite left in the module**, and the thing it overwrites is our own zero-byte placeholder. If the rename fails the placeholder is removed again, because an empty file where a photo should be is worse than nothing.

**Windows is not covered by this ADR's evidence.** `fs::hard_link` works on NTFS and the `ErrorKind` arms above are platform-independent, so the code compiles and the contract should hold — but the `#[cfg(unix)]` errno fallback does not apply, so a Windows filesystem returning a `PermissionDenied` that means "no hard links here" would classify as `Fatal` and refuse the move rather than copying. That is the safe direction to be wrong in (a refusal, not a lost file), and it is untested: CI runs `ubuntu-latest` and `macos-latest` only. Adding Windows to the matrix is the prerequisite for claiming anything stronger.

**A cross-volume move is not equivalent to a same-volume one under interruption**, which is why `MoveKind` records which happened. A same-volume move creates a directory entry and drops another and cannot half-happen to the file's contents; a cross-volume move reads and rewrites every byte. Callers, the duplicate manifest today and a journal later, want to know which one moved a given photo.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Keep `fs::rename`, tighten the `exists()` check | There is no tightening available. The check is stale by construction, and it is *also* wrong with no race at all, on dangling symlinks. A better-timed wrong question is still the wrong question. |
| Add `libc` and use `renameat2` / `renamex_np` | One syscall instead of two, at the cost of a dependency, two `cfg`-split unsafe call sites, and a fallback path for kernels and filesystems that reject the flag — which is `link` + `unlink` anyway. The window `link` opens holds no state in which data is missing. |
| Verify the copy by size plus mtime | Neither is a function of the contents. A block-substituting filesystem preserves both. The question being asked is "are these the same bytes", and the only honest answer to it is a digest. |
| Verify the copy by re-reading and comparing bytes directly | Equivalent in strength, worse in shape: it needs both files open at once and cannot reuse the hashing code dedup already runs. Streaming the source once while hashing and writing is one pass, and it produces the digest the error message quotes. |
| Hash only for cross-volume moves above some size threshold | The threshold is a guess about which photos matter. Hashing is one pass over data that is being read and written anyway. |
| Trust the copy and keep the source until a later verification pass | There is no later pass, and adding one means holding two copies of a library through an interruption that would leave the state ambiguous. Verify before deleting, or do not delete. |

## Consequences

- **Two syscalls per same-volume move instead of one**, plus the classification on failure. Not measurable against the EXIF parse and the BLAKE3 hashing that dominate a run.
- **A cross-volume move now hashes twice** — once streaming the source as it is copied, once over the landed file. The first is free (the bytes are in hand), the second is a full re-read of the destination. That is the price of the guarantee and it is paid only on the copy path.
- **Some moves that previously "succeeded" now fail**, and correctly: a destination whose directory is read-only, a source deleted since planning, a destination filesystem with no space. Each returns an error naming both paths instead of a temp-file message about a copy the operator never asked for.
- **`execute_move` returns `MoveOutcome { kind, destination }`.** The planned destination is not always the one a file reaches, and a record that cannot name the file's actual path cannot be used to put it back. `move_duplicates` records the reached path; a future journal reads the same field.
- **The library surface grew** — `copy_verify_delete`, `MoveError`, `MoveKind` and `MoveOutcome` are public because the tests that hold this contract drive them directly. A seam that only production code can reach is a seam no test can hold.
- **The contract is pinned by `tests/failure_paths.rs` and `tests/path_properties.rs`**, both written before the fixes they cover. Every one of the five original regression tests failed or refused to compile against the code as it stood, and each failure is recorded in the phase document rather than described. The property suite additionally asserts that a contested destination never overwrites its occupant for arbitrary generated collision sets — the generalisation of the two hand-written cases.
- **This is not an undo log.** The contract says a move never destroys something it did not create, and that a copy is proved before its source is deleted. It says nothing about reversing a completed run, which remains the gap [`adr-001`](adr-001-dry-run-by-default.md) addresses by making preview the default.
