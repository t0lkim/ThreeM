---
name: Bug report
about: Something mmm did that it should not have, or did not do that it should
title: ''
labels: bug
assignees: ''
---

<!--
If files went missing, stop running mmm against that library before filing.
`mmm journal list <output>` is read-only and names the runs; the journal is the
record that lets a run be put back.

Security issues do not go here — see SECURITY.md.
-->

## What happened

<!-- What you expected, and what happened instead. -->

## Version

<!-- Paste the output of `mmm --version`. If you built from source, the commit too. -->

```
$ mmm --version

```

## Operating system

<!-- e.g. macOS 15.3 (Apple silicon), Ubuntu 24.04 (x86_64). The filesystem
matters as well if you know it — APFS, ext4, exFAT, an SMB or NFS share. -->

## The exact command

<!-- Verbatim, including every flag, and say whether `--commit` was one of them.
Paths can be redacted, but keep the shape: how many inputs, and whether -o
pointed somewhere else. A run without --commit modifies nothing, which is often
the fastest thing to establish. -->

```sh
$ 
```

## Is there a journal file?

<!-- Tick one. mmm prints the journal path as a run starts and again in the
closing summary; by default it is <output>/.mmm/journal/<run_id>.jsonl.
`mmm journal list <output>` lists the runs it can see. -->

- [ ] Yes — `mmm journal list <output>` shows the run
- [ ] No — the run was a preview (no `--commit`), so nothing was journalled
- [ ] No — `--no-journal` was used
- [ ] I do not know

<!-- If a journal exists, `mmm journal show <run_id>` output is the single most
useful thing you can attach. Redact paths if you need to, but keep the entries
paired: one intent line and its outcome line. -->

## Output

<!-- The terminal output, and the run summary if it got that far. Re-running
with -vv gives considerably more. Anything mmm said about skipped files matters
even when it does not look related. -->

```
```

## Anything else

<!-- Roughly how many files, whether any are on a different volume from the
output, whether sidecars or duplicates are involved, whether a config file or
any MMM_ environment variables are in play (`mmm config show` names the layer
that decided every setting). -->
