#!/usr/bin/env bash
# Regenerate the dataset dump, compute expected outputs with the TypeScript oracle and
# the independent Python check, and fail if either disagrees with bench/expected.json.
set -euo pipefail

BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUN="$(mise which bun 2>/dev/null || command -v bun)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

"$BUN" "$BENCH/dump-datasets.ts" "$TMP/datasets.json"
"$BUN" "$BENCH/oracle.ts" > "$TMP/oracle.json"
if ! cmp -s "$TMP/oracle.json" "$BENCH/expected.json"; then
  echo "verify-expected: bench/oracle.ts output differs from bench/expected.json" >&2
  diff "$BENCH/expected.json" "$TMP/oracle.json" >&2 || true
  exit 1
fi
echo "verify-expected: oracle matches bench/expected.json"
python3 -I "$BENCH/check_expected.py" "$TMP/datasets.json" "$BENCH/expected.json"
