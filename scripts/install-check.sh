#!/bin/sh
# Exercise the installed artifact from an empty directory, without repository paths.
set -eu
if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo 'Usage: scripts/install-check.sh <installation-directory> [empty-work-directory]' >&2
  exit 2
fi
installation=$(CDPATH='' cd -- "$1" && pwd -P)
# Deno runs the TypeScript smoke host; the installed binary needs no Deno.
deno=$(command -v deno)
work=${2:-$(mktemp -d "${TMPDIR:-/tmp}/codemode-install-check.XXXXXX")}
mkdir -p "$work"
[ -z "$(ls -A "$work")" ] || { echo 'smoke work directory must be empty' >&2; exit 2; }
work=$(CDPATH='' cd -- "$work" && pwd -P)
cd "$work"
"$deno" run --no-config --allow-run --allow-read --allow-write --allow-env \
  "$installation/scripts/install-smoke.ts" "$installation"
printf 'Installed workflow passed; trace, journal, and plan evidence: %s\n' "$work"
