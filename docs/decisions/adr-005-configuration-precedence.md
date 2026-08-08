---
type: decision
title: Configuration precedence
created: 2026-08-08
tags:
  - config
  - cli
  - safety
  - precedence
related:
  - '[[adr-001-dry-run-by-default]]'
  - '[[configuration]]'
  - '[[CHANGELOG]]'
---

# ADR-005: Layered configuration, strict keys, and no `commit` in a file

**Status:** Accepted
**Date:** 2026-08-08

## Problem

Every run meant retyping the same flags. A person with one library and one set of preferences — a chunk size, an output tree, a date layout — had no way to write them down, so each invocation restated them and each restatement was a chance to get one wrong.

A config file answers that, and it introduces three questions that have to be settled before it is written rather than discovered afterwards:

1. **Which layer wins?** Once there is more than one place a value can come from, "why did it do that?" becomes a real question. A tool whose answer is "read the source" has replaced typing flags with something worse.
2. **What happens to a key nobody recognises?** A config file is written once and read for months. `chunck_size` is one keystroke away from `chunk_size`.
3. **What may a file say at all?** [`adr-001`](adr-001-dry-run-by-default.md) made previewing the default and moving files opt-in at the command line. A file that could set `commit` would undo that decision for every invocation on the machine, silently, and the person it happened to would not be the person who wrote the file.

## Decision

**Five layers in a fixed order, `deny_unknown_fields` on every layer, and the three safety flags absent from the settings type entirely.**

### Layer order

Lowest priority first: built-in defaults, user config, project config, `MMM_` environment variables, command line.

The ordering principle is **specificity to this invocation**. The defaults know nothing about the user; the user config is a standing preference; a project config is a statement about one tree, and the *nearest* one wins because a config in a subdirectory is a statement about that subdirectory and one several levels up that outranked it could never be overridden; the environment belongs to this shell; the flag belongs to this command. Each layer knows more about what is happening right now than the one below it.

Layers combine **key by key**, never wholesale. A layer that sets one key does not blank the eleven it said nothing about, which is what lets a config file name only what it changes. That in turn requires a type in which "said nothing" and "said the default" are different values — hence a `PartialSettings` of `Option` fields distinct from the resolved `Settings` — because a layer that could not tell silence from agreement would have every file overwrite every file below it with values nobody wrote, and the *lowest*-priority layer would effectively win.

Defaults are applied **last**, to the fields still unclaimed after every layer has spoken, rather than seeded as a first layer. The merge would be identical either way. What seeding would destroy is the property `mmm config show` runs on: after the fold, a field that is still unset is one no layer claimed, so "where did this value come from?" is answerable by walking the layer list backwards for the first layer with an opinion. Source annotations fall out of the merge instead of needing a parallel mechanism that could disagree with it.

The order lives in the sequence the caller passes layers in, and nowhere else. A priority number on a layer would be a second place for the ordering to live and a second place for it to be wrong.

### Strict keys

`deny_unknown_fields` on every deserialised layer, and the same rule applied by hand to the `MMM_` variables.

A mistyped key that is quietly ignored is indistinguishable, from the outside, from a setting that does not work: the user changes the value, sees no difference, and concludes the feature is broken. Refusing the file and naming the key costs one error message and saves that entire investigation. The same reasoning drives the rest of the loading rules — a malformed file, an unreadable one, a `--config` path that does not exist, a format string that will not compile and a glob that will not parse are all hard errors naming the file, the line and the column, never a silent fall back to the defaults. A config that was ignored and a config that was obeyed produce different trees from the same command.

Validation hangs off deserialisation rather than off a pass over the resolved settings. Two consequences follow, and the second is why. First, *when*: a broken pattern is a broken config file whether or not a higher layer would have overridden it. Second, *where*: an error raised inside the deserialiser carries the TOML span, so the message can be `mmm.toml:2:25` and the reader is looking at the right line. A check after the fold could only say that some layer, somewhere, was wrong.

### `commit` is not a setting

`commit`, `no_journal` and `i_know_what_im_doing` are absent from the settings type, refused in a file, and refused in the environment.

All three exist to make a destructive or irreversible run harder to ask for, and they have that property **only while they must be typed**. A run must not become destructive because of a file somebody wrote months ago, one that arrived with a copied project directory, or one inherited from `$HOME` by a script that only meant to preview. `deny_unknown_fields` does the refusing mechanically; a named reason is substituted for its message, because serde's answer to `commit = true` is a list of the twelve fields that *are* settings — everything except the one thing the reader needs to know.

The omission carries a comment on the struct saying all of this, so nobody later reads it as an oversight and "completes" the type.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **Ignore unknown keys, warn on stderr** | The warning is on the wrong side of the problem: it appears in a scrollback nobody reads on a run that then proceeds to do the wrong thing. It also cannot be made to work for `commit = true`, which must be a refusal and not a note. |
| **Merge lists across layers instead of replacing** | A `skip_patterns` or `extensions.image` that accumulated could only ever be *widened*. There would be no way to narrow a list from a project file, and no way to say "actually, scan everything". Replacement is reversible from a higher layer; accumulation is not. |
| **Merge the `[extensions]` table wholesale, like every other value** | Adding one video extension would silently discard twenty image ones, and the person who did it would find out when a scan came back empty. The table merges field by field for that reason and no other. |
| **Seed defaults as the lowest layer** | Identical merge result, and it erases the distinction between "the default" and "somebody set it to the default value" — which is exactly the question `mmm config show` exists to answer. |
| **A priority number on each layer** | A second place for the ordering to live, and a second place for it to be wrong. The sequence the layers are passed in already is the rule. |
| **Allow `commit` in a project config only** | The narrower version has the same shape: a `mmm.toml` travels with a directory, arrives by `git clone` or by copying a tree from a colleague, and would then make a bare `mmm .` destructive for whoever ran it next. The property being protected is that the person standing there typed it. |
| **A `--yes`-style config key that only *enables* prompting-free commits** | Same thing with a different name. Any file-settable key that shortens the distance to a move re-opens what [`adr-001`](adr-001-dry-run-by-default.md) closed. |
| **A single global config, no project layer** | Loses the case the feature is most useful for: one machine organising several libraries with different layouts. A per-tree file is how a library carries its own conventions. |
| **JSON or YAML instead of TOML** | JSON has no comments, which rules it out for a file whose starter template is mostly explanation. YAML's implicit typing (`no` as a boolean, a version number as a float) is a class of surprise nobody needs in a file that decides where photographs are filed. |
| **Environment above the command line** | Backwards. A variable exported in a shell profile is standing state; a flag is this invocation. A tool where the flag could not win would have no way to override anything for one run. |

## Consequences

- **Every subcommand loads the configuration, `mmm undo` included.** A broken config stops an undo until it is fixed or `--no-config` is passed. That is deliberate: `journal_dir` selects where undo *reads* journals as well as where organise writes them, so running on the defaults instead would search the wrong place and report "no runs recorded" for a library that has them.
- **Tracing initialises after the layers are read**, because `verbose` is itself a setting and a subscriber built from the flag alone could not be told about a configured one afterwards. The cost is that a config error is reported through the process's exit rather than through the log — which is where an error naming a file and a line belongs anyway.
- **`--no-prompt` needed an explicit `=false` form.** A bare switch can only ever say `true`, so a `no_prompt = true` in a user config would have been unanswerable from the command line, and a flag that resolves the same whether or not it was passed is not a precedence rule.
- **`verbose` has the same hole and is not fixed.** A count of zero is genuinely "nothing said", so a configured verbosity cannot be turned back *down* except with `--no-config`. The flag's help says so.
- **`--chunk-size` lost its clap default.** A default baked into the flag arrives as an opinion of the *command line* — the highest-priority layer there is — and would have silently outranked every `chunk_size` any config file could set, which looks exactly like the setting not working.
- **`--config` replaces discovery rather than adding to it.** A file named on the command line is the answer to "what settings is this run using?", and one that still inherited from `$HOME` would not be. `--no-config` skips files but not the environment, since the environment belongs to this invocation the way a flag does.
- **Discovery inputs are arguments, not process reads.** The working directory, the user config path and the environment are passed in, with one function reading the real process. The precedence rules are therefore testable against a temporary directory with no `set_var` anywhere — which matters because Rust test binaries are threaded, and one test's `$XDG_CONFIG_HOME` would otherwise be every concurrent test's.
- **Containment is validated at the layer, not argued about.** `unsorted_dir = "/etc"` and `date_directory_format = "../%Y"` are one line of config each and would file photographs outside the tree the run was pointed at. Both are refused where they are written, and the Phase 02 containment invariants are quantified over every format string and directory name the loader accepts rather than over the ones shipped by default.
- **`mmm config show` is the explanation.** Its output is a valid config file with a `# from:` annotation on every value, so "why did it do that?" is answerable without reading source — which was the point of fixing the precedence in the first place. Full key reference in [`configuration`](../reference/configuration.md).
