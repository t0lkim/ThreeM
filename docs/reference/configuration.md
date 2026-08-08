---
type: reference
title: Configuration
created: 2026-08-08
tags:
  - config
  - cli
  - reference
related:
  - '[[adr-005-configuration-precedence]]'
  - '[[USER-GUIDE]]'
  - '[[CHANGELOG]]'
---

# Configuration

`mmm` reads its settings from four layers. Lowest priority first:

| # | Layer | Where | Skipped by |
|---|---|---|---|
| 0 | Built-in defaults | Compiled in — the **Default** column below | nothing |
| 1 | User config | `$XDG_CONFIG_HOME/mmm/config.toml` when that variable is set to an absolute path, otherwise the platform config directory — `~/.config/mmm/config.toml` on Linux, `~/Library/Application Support/mmm/config.toml` on macOS | `--no-config`, `--config` |
| 2 | Project config | The nearest `mmm.toml` or `.mmm.toml`, found by walking up from the working directory to the filesystem root | `--no-config`, `--config` |
| 3 | Environment | `MMM_`-prefixed variables | nothing |
| 4 | Command line | Flags, this invocation only | nothing |

A higher layer overrides a lower one **key by key**. A layer that says nothing about a key leaves the layer below it standing, so a config file only has to name what it changes. Defaults are applied last, to the keys no layer claimed.

Two flags change which files are read:

- `--config <PATH>` reads that one file **instead of** searching. It does not add to discovery — the user config is not inherited underneath it — and a path that does not exist is an error, never a fall back to the defaults.
- `--no-config` reads no config file at all. `MMM_` variables still apply: skipping files is a statement about *files*.
- Passing both is refused; one names a file to read and the other says to read none.

The reasoning behind this order, and behind the two rules that constrain it, is in [`adr-005-configuration-precedence`](../decisions/adr-005-configuration-precedence.md).

## Starting a config

```bash
mmm config init            # write the per-user config
mmm config init --project  # write mmm.toml in the working directory
```

Both write every key at its built-in default, commented out, with the precedence rule at the top. Neither overwrites an existing file unless `--force` is passed.

## Keys

Every key is available in the user config, the project config and the environment. The **CLI flag** column is where the four layers stop being interchangeable: a key with no flag can only be set in a file or the environment.

Nesting in the **Key** column is the TOML table — `extensions.image` is written under an `[extensions]` header.

| Key | Type | Default | CLI flag | Environment variable |
|---|---|---|---|---|
| `output_dir` | path | *none* — the run writes into its first input directory | `-o`, `--output` | `MMM_OUTPUT_DIR` |
| `chunk_size` | integer | `100` | `-c`, `--chunk-size` | `MMM_CHUNK_SIZE` |
| `no_prompt` | boolean | `false` | `--no-prompt[=BOOL]` | `MMM_NO_PROMPT` |
| `verbose` | integer — `0` warnings, `1` info, `2` debug, `3` and above trace | `0` | `-v`, repeated (raises only — see below) | `MMM_VERBOSE` |
| `journal_dir` | path | *none* — journals go under `<output>/.mmm/journal` | `--journal-dir` | `MMM_JOURNAL_DIR` |
| `date_directory_format` | strftime pattern | `"%Y-%m-%d"` | — | `MMM_DATE_DIRECTORY_FORMAT` |
| `filename_format` | token pattern | `"{date}-{time}{location}.{ext}"` | — | `MMM_FILENAME_FORMAT` |
| `include_location` | boolean | `true` | — | `MMM_INCLUDE_LOCATION` |
| `duplicates_dir` | relative path | `"duplicates"` | — | `MMM_DUPLICATES_DIR` |
| `unsorted_dir` | relative path | `"unsorted"` | — | `MMM_UNSORTED_DIR` |
| `skip_patterns` | list of globs | `[]` | — | `MMM_SKIP_PATTERNS` |
| `default_timezone` | fixed offset or IANA zone name | *none* — the machine's own timezone is used, and the run says so | `--timezone` | `MMM_DEFAULT_TIMEZONE` |
| `require_exif` | boolean | `false` | `--require-exif[=BOOL]` | `MMM_REQUIRE_EXIF` |
| `filesystem_date_warning_percent` | integer 0–100 | `20` | — | `MMM_FILESYSTEM_DATE_WARNING_PERCENT` |
| `extensions.image` | list of strings | the 21 built-in image extensions | — | `MMM_EXTENSIONS_IMAGE` |
| `extensions.video` | list of strings | the 11 built-in video extensions | — | `MMM_EXTENSIONS_VIDEO` |

`mmm config show` prints the built-in extension lists in full, so `mmm config show > mmm.toml` gives you them to edit rather than to retype.

In the environment, a list is comma-separated and surrounding spaces are trimmed — `MMM_SKIP_PATTERNS='*.tmp, .thumbnails'`. An empty value is the empty list, which is how a shell says "scan everything". Booleans are `true`/`false`/`1`/`0` and nothing else: a deliberately short list, so that no `MMM_NO_PROMPT=maybe` reads as false to one reader and true to another.

### What the values mean

- **`date_directory_format`** is a strftime pattern for the dated directory. The default is **one directory per day** — `2024-03-15/`, not `2024/03/15/`. `"%Y/%m/%d"` restores the nested tree and `"%Y/%Y-%m"` files by year and month; a `/` in the pattern is a directory separator, everything else is part of a name. It is refused if it is an absolute path, contains `..`, is not a valid strftime pattern, or renders to nothing.
- **`filename_format`** is a token pattern for the filename. The tokens are `{date}` (`YYYY-MM-DD`), `{time}` (`HHMMSS`), `{location}`, `{ext}` and `{original_stem}`. `{location}` carries its own leading separator and expands to nothing when a file has no coordinates, which is why the default has no hyphen in front of it. It is refused if it contains a path separator, begins with a dot, omits `{ext}`, or uses a token that does not exist.
- **`include_location = false`** drops the place name from every filename *and* skips the geocoding lookup, rather than performing it and discarding the result.
- **`duplicates_dir`** and **`unsorted_dir`** are relative to the output tree and may be nested — `duplicates_dir = "_review/copies"`. Absolute paths and `..` are refused: either would file photographs outside the tree the run was pointed at.
- **`skip_patterns`** excludes paths from the scan. A pattern with **no `/`** matches a path component's own name, so `"*.tmp"` skips those files anywhere and `".thumbnails"` skips that directory wherever it appears. A pattern **containing `/`** matches the path relative to the scan root, so `"raw/**"` skips one subtree rather than every `raw/` in the library. `*` stops at a separator, `**` crosses one, and a matching directory is pruned rather than walked. The run reports `N entries excluded by skip_patterns` so a pattern quietly swallowing a library is visible.
- **`default_timezone`** decides which wall clock a photo with no recorded offset is read against — a fixed offset (`"+08:00"`, `"-05:30"`) or an IANA zone name (`"Asia/Singapore"`), refused if it is neither. A file that carries its own `OffsetTimeOriginal` tag is unaffected: the file's own record always wins. It does **not** change which day an EXIF-dated photograph is filed under — a wall clock is filed under exactly the digits the camera wrote, on any machine — but it does decide the recorded instant, and it does move filesystem-dated and UTC-stamped video files.
- **`require_exif = true`** refuses to file anything under a date it did not record itself. A file dated from the filesystem goes to the unsorted directory **keeping its own filename**, unlike the undated files there, which are all `unknown.<ext>` — the point of the setting is that you would rather sort those by hand, and a directory of `unknown-1.cr2` would make that impossible. Settable in a file where `commit` is not, because it can only ever make a run more careful; `--require-exif=false` answers it from the command line.
- **`filesystem_date_warning_percent`** is the share of *dated* files that may take their date from the filesystem before the run's summary says so out loud. Files with no date at all are not counted either way — they went to the unsorted directory, which is the run already saying so. `0` warns about every single fallback; `100` never warns. A value above 100 is refused rather than accepted as a threshold nothing can cross.
- **`extensions.image` / `extensions.video`** decide what counts as media, compared case-insensitively. A list **replaces** the built-in one — `image = ["dng", "jpg"]` scans those two and nothing else — which is also how to make the tool stop picking up a format it currently does.

### What cannot be set here

Three flags exist only on the command line, and writing one in a config file or the environment is an **error** rather than a silent no-op:

| Flag | Why it is command-line only |
|---|---|
| `--commit` | Moving files is opt-in at the command line so that no file — not one written months ago, not one that arrived with a copied project directory — can make a run destructive. |
| `--no-journal` | A run without a journal cannot be undone, so it has to be asked for deliberately. |
| `--i-know-what-im-doing` | Acknowledging an unsafe combination *is* the acknowledgement, and a file cannot give it on your behalf. |

```
$ mmm config validate bad.toml
Error: bad.toml:1:1: `commit` cannot be set here — moving files is opt-in at the command line
so that no file — not one written months ago, not one that arrived with a copied project
directory — can make a run destructive. Pass --commit on the command line instead.
```

## Worked precedence example

A user config that sets two keys, and a project config in a directory above the one the command runs in:

```toml
# ~/.config/mmm/config.toml
chunk_size = 10
no_prompt = true
```

```toml
# ~/work/mmm.toml
chunk_size = 20
```

Run from `~/work/nested/`, each stage adding the next layer up. `# from:` is `mmm config show`'s own annotation:

```
$ mmm config show                                  # user config only
chunk_size = 10  # from: user config (~/.config/mmm/config.toml)
no_prompt = true  # from: user config (~/.config/mmm/config.toml)

$ mmm config show                                  # project config found by walking up
chunk_size = 20  # from: project config (~/work/mmm.toml)
no_prompt = true  # from: user config (~/.config/mmm/config.toml)

$ MMM_CHUNK_SIZE=30 mmm config show
chunk_size = 30  # from: environment
no_prompt = true  # from: user config (~/.config/mmm/config.toml)

$ MMM_CHUNK_SIZE=30 mmm --chunk-size 40 config show
chunk_size = 40  # from: command line
no_prompt = true  # from: user config (~/.config/mmm/config.toml)
```

`no_prompt` never moves, and that is the point: the project config, the environment variable and the flag each spoke about `chunk_size` alone, so the user config's other opinion stood the whole way up.

`mmm config path` shows the walk that found the project file, including the directories passed over:

```
$ mmm config path

Searched, in order:

  found      user config (~/.config/mmm/config.toml)
  not found  project config (~/work/nested/mmm.toml)
  not found  project config (~/work/nested/.mmm.toml)
  found      project config (~/work/mmm.toml)

2 files were read, lowest priority first.
```

The walk stops at the first hit, so what it lists is what actually happened. Within one directory `mmm.toml` is checked before `.mmm.toml`, and finding one stops the search rather than merging the two.

## Errors

A config that cannot be read is never ignored. Every failure names the file and the position in it, and the run stops before it scans anything.

```
mmm.toml:1:1: unknown field `chunck_size`, expected one of `output_dir`, `chunk_size`, `no_prompt`, …
mmm.toml:1:14: invalid type: string "many", expected usize
mmm.toml:1:25: `date_directory_format` must be relative to the output directory, and "/%Y" is an
  absolute path — it would file photographs at the root of the filesystem
mmm.toml:1:17: `skip_patterns` entry "[unclosed" is not a valid glob: error parsing glob
  '[unclosed': unclosed character class; missing ']'
MMM_CHUNCK_SIZE: `chunck_size` is not a setting — see `mmm config show` for the keys that are
no config file at nope.toml — --config names the file to read, and carrying on with the defaults
  would silently do something other than what was asked
```

A mistyped key, a value of the wrong type, malformed TOML, a format or glob that will not compile, an unrecognised `MMM_` variable and a `--config` path that is not there all fail this way. The line and column are the position in the file, so the reader is looking at the right line rather than searching for it.

`mmm config validate [PATH]` asks these questions without running anything. With a path it reads **that file and nothing else**, so a broken project config cannot stop the command you reached for to diagnose broken configs. With no path it reports on the files this run would read:

```
$ mmm config validate

  ok  user config (~/.config/mmm/config.toml)
  ok  project config (~/work/mmm.toml)

2 config files parsed.
```

## Gotchas

- **A list replaces, it does not append.** A layer setting `skip_patterns` or `extensions.image` supersedes the whole list below it. That is what makes a project config able to *narrow* a list, or say "actually, scan everything" with `skip_patterns = []`.
- **`[extensions]` merges field by field, though.** A layer naming only `video` leaves the layer below's `image` list standing. Merging the table wholesale would mean adding one video extension silently discarded twenty image ones.
- **`verbose` only goes up from the command line.** A count of zero is indistinguishable from not passing `-v`, so it enters as "said nothing" and a configured `verbose = 2` stands. The way to turn a configured verbosity back down is `--no-config`.
- **`--no-prompt=false` exists for the same reason.** A bare switch can only ever say yes, so a `no_prompt = true` in a file would otherwise be unanswerable from the command line. The value must be attached with `=`, so `mmm --no-prompt ~/Photos` still reads the path as a directory.
- **`journal_dir` moves both sides at once.** It relocates the journals a run writes *and* the ones `mmm undo` and `mmm journal list` read, so the two cannot end up looking in different places.
- **A broken config stops `mmm undo` too.** Every subcommand loads the configuration first. Carrying on with the defaults would mean undo searching for journals wherever the *default* says and reporting "no runs recorded" for a library that has them; an error naming the file and line is the better failure, and `--no-config` is the way past it.
- **`mmm config show` output is a config file.** `mmm config show > mmm.toml` pins the current settings — the `# from:` annotations are trailing comments, and the two keys with no stored default are printed commented out with what the run does instead.
- **An organise flag before `config` is honoured.** `mmm --chunk-size 7 config show` reports `# from: command line`, because the layer that wins most often is the one an explanation most needs to cover. `--commit`, `--no-journal` and `--i-know-what-im-doing` are still refused there — `config` cannot act on them.
