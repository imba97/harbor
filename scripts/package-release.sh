#!/usr/bin/env bash
#
# Packages one release archive for harbor.
#
#     scripts/package-release.sh <target-triple> [out-dir]
#
# and it writes
#
#     dist/harbor-<target>.tar.gz
#
# The archive holds exactly two entries, both at the top level: the `cargo-harbor` binary and
# `LICENSE`. The layout is an interface, not a preference -- `cargo binstall harbor` unpacks
# the archive and looks for the binary at its root, which is why
# `[package.metadata.binstall] bin-dir` in Cargo.toml has to spell that out; and MIT requires
# the licence notice to travel with a copy of the binary anyway.
#
# The name is an interface too, and a subtle one: `harbor-<target>` is the "versionless"
# filename binstall tries by default, and it carries the **crate** name rather than the
# binary's (`cargo-harbor`, which Cargo's subcommand convention forces). The binary name only
# has to match `{ bin }` inside the archive.
#
# `.tar.gz` is one of the extensions binstall accepts for its `tgz` format, so no `pkg-fmt`
# or `pkg-url` override is needed.
#
# This script is what CI runs, so a release can be reproduced locally with one command.

set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  echo "usage: $0 <target-triple> [out-dir]" >&2
  exit 2
fi

target="$1"
out_dir="${2:-dist}"
root="$(cd "$(dirname "$0")/.." && pwd)"

# `--locked`: a release is built from the committed lockfile, so the same tag yields the
# same dependency set no matter who builds it.
cargo build --release --locked --manifest-path "$root/Cargo.toml" --target "$target"

binary="$root/target/$target/release/cargo-harbor"
if [ ! -x "$binary" ]; then
  echo "no executable at $binary" >&2
  exit 1
fi

mkdir -p "$out_dir"
archive="$out_dir/harbor-$target.tar.gz"

# Staged in a temporary directory so the archive carries no path components of its own:
# `tar -C <dir> cargo-harbor LICENSE` stores them as `cargo-harbor` and `LICENSE`.
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
cp "$binary" "$staging/cargo-harbor"
cp "$root/LICENSE" "$staging/LICENSE"

tar -czf "$archive" -C "$staging" cargo-harbor LICENSE

echo "$archive"
