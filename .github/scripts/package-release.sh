#!/usr/bin/env bash
#
# Archive an already-built release target as a .tar.gz with a SHA256 checksum.
#
#   .github/scripts/package-release.sh <target-triple> <version> [dist-dir]
#
# Reads the binaries named in BINARIES below out of
# code/target/<target>/release/ and writes
# <dist>/mmm-<version>-<target>.tar.gz plus a .sha256 beside it. The dist
# directory is relative to the current directory; the binaries and the documents
# are found relative to this script, so it can be run from anywhere.
#
# Like the changelog extractor next to it, this is a script rather than inline
# YAML so the packaging can be dry-run locally against a real build instead of
# being discovered to be wrong at tag time.

set -euo pipefail

target="${1:?usage: package-release.sh <target-triple> <version> [dist-dir]}"
version="${2:?usage: package-release.sh <target-triple> <version> [dist-dir]}"
dist="${3:-dist}"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
bin_dir="$repo_root/code/target/$target/release"
name="mmm-$version-$target"
stage="$dist/$name"

# The binaries a release ships. This is a declaration on one line, and the only
# place in the file that names them, because `code/tests/release.rs` reads this
# line and compares it against the `[[bin]]` targets in `code/Cargo.toml` — set
# against set, so a third binary added to the crate cannot be left out of the
# tarball in silence, and one deleted from the crate cannot linger here.
BINARIES="mmm mmm-dedup-verifier mmm-fixtures"

# Linux runners have sha256sum, macOS has shasum. Both print `<digest>  <file>`,
# which is what `sha256sum -c` and `shasum -a 256 -c` each expect to read back.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$@"
    else
        shasum -a 256 "$@"
    fi
}

# `[profile.release]` in code/Cargo.toml sets `strip = true`, so there is no
# strip step here — there is a check that it happened. A stripped binary keeps
# only its undefined (imported) symbols: measured on this crate, the release
# `mmm` has 1 non-undefined symbol against 44,822 in the debug build, so the
# two are not close enough for a threshold to be a judgement call. On Linux
# `nm` of a stripped ELF reports no symbols at all and exits non-zero, which
# reads here as a count of zero.
#
# The check exists because `strip = true` is one line in a profile: delete it
# and every release from then on ships tens of megabytes of debug symbols with
# nothing to say so.
assert_stripped() {
    local bin="$1" symbols
    symbols=$(nm -- "$bin" 2>/dev/null | grep -c -v ' U ' || true)
    symbols="${symbols:-0}"
    if [ "$symbols" -gt 100 ]; then
        echo "::error::$bin carries $symbols symbols — [profile.release] strip = true is not in effect" >&2
        exit 1
    fi
    echo "  $(basename "$bin"): stripped ($symbols non-undefined symbols)"
}

if [ ! -d "$bin_dir" ]; then
    echo "package-release: nothing built for $target — no $bin_dir" >&2
    exit 1
fi

rm -rf "$stage"
mkdir -p "$stage"

# A top-level directory inside the archive, so unpacking it in a downloads
# folder leaves one directory rather than five loose files.
for bin in $BINARIES; do
    if [ ! -x "$bin_dir/$bin" ]; then
        echo "package-release: $bin_dir/$bin is missing or not executable" >&2
        exit 1
    fi
    assert_stripped "$bin_dir/$bin"
    cp "$bin_dir/$bin" "$stage/$bin"
done

for doc in README.md LICENSE CHANGELOG.md; do
    cp "$repo_root/$doc" "$stage/$doc"
done

tar -czf "$dist/$name.tar.gz" -C "$dist" "$name"
rm -rf "$stage"

# Written from inside the dist directory so the file it names is a bare
# filename: `sha256sum -c mmm-0.2.0-<target>.tar.gz.sha256` then works for
# somebody who downloaded both into the same place.
(cd "$dist" && sha256 "$name.tar.gz" >"$name.tar.gz.sha256")

echo "packaged $dist/$name.tar.gz"
cat "$dist/$name.tar.gz.sha256"
