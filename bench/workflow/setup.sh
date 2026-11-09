#!/bin/sh
# Install the pinned comparison packages and build codemode for the workflow benchmark.
#
#   mise exec -- bench/workflow/setup.sh
#
# Each package tree is installed from its committed lockfile into its own directory under
# bench/workflow/.cache/pkgs/ with install scripts disabled; the benchmark's own scripts never
# live there and import package entry points by absolute path. npm verifies every tarball
# against the lockfile's integrity; this script then checks the top-level packages against
# pins.json, so a lockfile edit cannot silently change what is measured.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cache="$here/.cache/pkgs"
mkdir -p "$cache"

for name in pi-durable opencode; do
  dir="$cache/$name"
  rm -rf "$dir"
  mkdir -p "$dir"
  cp "$here/lock/$name/package.json" "$here/lock/$name/package-lock.json" "$dir/"
  (cd "$dir" && npm ci --ignore-scripts --no-audit --no-fund --prefer-offline --loglevel=error)
done

node "$here/lib/verify-pins.ts" "$here/pins.json" "$cache"

(cd "$root" && cargo build --release --locked --bin codemode)
