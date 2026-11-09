#!/bin/sh
# Exercise the installed artifact from an empty directory, without repository paths.
set -eu
if [ "$#" -lt 1 ] || [ "$#" -gt 3 ]; then
  echo 'Usage: scripts/install-check.sh <installation-directory> [deno|embedded-v8] [empty-work-directory]' >&2
  exit 2
fi
installation=$(CDPATH='' cd -- "$1" && pwd -P)
backend=${2:-deno}
case "$backend" in deno|embedded-v8) ;; *) echo 'unknown backend' >&2; exit 2 ;; esac
deno=$(command -v deno)
work=${3:-$(mktemp -d "${TMPDIR:-/tmp}/codemode-install-check.XXXXXX")}
mkdir -p "$work"
[ -z "$(ls -A "$work")" ] || { echo 'smoke work directory must be empty' >&2; exit 2; }
work=$(CDPATH='' cd -- "$work" && pwd -P)
cd "$work"
"$deno" run --no-config --allow-run --allow-read --allow-write --allow-env \
  "$installation/scripts/install-smoke.ts" "$installation" "$backend" "$deno"
printf 'Installed workflow passed; trace, journal, and fixture evidence: %s\n' "$work"
