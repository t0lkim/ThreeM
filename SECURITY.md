# Security Policy

## Reporting a vulnerability

Email **security@t0lkim.dev** with `ThreeM Security` in the subject. Please do not
open a public issue for anything in scope below — the issue tracker is public,
and `mmm` is pointed at people's photograph libraries.

Include, as far as you have them: the version (`mmm --version`), the input that
triggers it, the exact command, and what you observed. A crashing file can be
attached; if you would rather not send the file itself, its BLAKE3 digest and
the first few hundred bytes are usually enough to reconstruct one.

**What to expect.** ThreeM is maintained by one person, without funding and
without a bounty programme, so the honest commitment is a small one: an
acknowledgement within seven days, and an assessment of whether the report is
in scope within thirty. If a report is valid I will fix it, credit you in the
changelog unless you ask me not to, and say in the release notes what an
existing user should do. If it is out of scope I will say so and why rather
than leaving it unanswered.

Please give me thirty days before publishing. If I have gone quiet past that,
publish — an unmaintained tool that people still run is itself the risk, and
silence is not a reason to keep users uninformed.

## Supported versions

Pre-1.0. Only the latest release gets fixes; there are no maintained branches
behind it. Upgrade before reporting if you can, and say so if you cannot.

## Scope

`mmm` is a local command-line tool. It opens no sockets, phones nothing home,
and its reverse geocoding runs against a dataset compiled into the binary — so
there is no server, no authentication and no network attack surface to report
against. What it does do is read files written by somebody else and move files
that matter, and that is where the interesting failures are:

**In scope**

- A crafted media file, XMP sidecar or journal line that causes a panic, an
  out-of-bounds read, an unbounded allocation or a hang. The four parsers this
  covers are fuzzed ([`docs/research/fuzzing.md`](docs/research/fuzzing.md)) —
  a report that gets past those targets is a good report.
- Anything that makes `mmm` write outside its output directory: a path
  traversal out of a filename derived from metadata, a symlink followed where
  it should not have been, a destination escaping via `..`.
- Anything that loses or overwrites data the tool promised to preserve —
  a destination overwritten rather than suffixed, an original deleted before
  its copy was verified, a move that no journal line records.
- A journal that cannot be replayed correctly, or an `mmm undo` that restores
  the wrong content, given a file the tool itself wrote.
- Weakened verification: the dedup verifier agreeing with a manifest it should
  have rejected, or a content check that a modified file passes.

**Out of scope**

- Denial of service by feeding the tool an enormous library. It is a batch
  organiser; a big input takes a long time.
- Filesystem races an attacker with write access to the same directories could
  win. Anyone who can rewrite your photographs mid-run does not need a bug in
  `mmm`.
- `--no-journal --commit --i-know-what-im-doing` behaving as documented. That
  combination exists to discard the safety net and requires three flags to
  reach, one of which is a sentence.
- Vulnerabilities in a dependency with no path from `mmm`'s inputs to them.
  Report those upstream; tell me if `mmm` reaches them and I will pull the fix
  through.

## Hardening already in place

Stated so a report can start from what is known rather than rediscover it:

- Every run is a preview until `--commit`; a preview creates, moves and deletes
  nothing.
- Every committing move is written to the journal and flushed to disk *before*
  the file is touched, so an interrupted run is still reversible.
- Same-volume moves are `link()` + `unlink()`, which refuses an occupied
  destination rather than overwriting it; cross-volume moves copy, compare
  BLAKE3 digests of what was read and what landed, and only then unlink the
  source.
- Deduplication never deletes: one member of each group stays put, the copies
  move to `duplicates/` with a manifest recording where each came from.
- The four untrusted-input parsers are fuzzed under AddressSanitizer in CI on
  every push, seeded from a checked-in corpus of regression inputs.
- The destructive modules carry line-coverage floors enforced by CI, and have
  been mutation-tested — with the surviving mutants, and the reason each was
  accepted, written down in
  [`docs/research/mutation-testing.md`](docs/research/mutation-testing.md).

None of that is a claim that the tool is safe from anything in the in-scope
list. It is what has been tried, so you know where the ground has been covered.
