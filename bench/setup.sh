#!/usr/bin/env bash
# Fetch and verify the pinned upstream engines into bench/.cache, then build our
# release binary. Pins live in bench/engines/pins.json. Downloaded code is treated as
# untrusted data: it is placed under bench/.cache, dependency install scripts are
# disabled, and nothing in the cache is executed by this script.
set -euo pipefail

BENCH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$BENCH/.." && pwd)"
CACHE="$BENCH/.cache"
PINS="$BENCH/engines/pins.json"

pin() { python3 -I -c 'import json,sys; v=json.load(open(sys.argv[1]))
for k in sys.argv[2].split("."): v=v[int(k)] if isinstance(v,list) else v[k]
print(v)' "$PINS" "$1"; }

need() { command -v "$1" >/dev/null || { echo "setup: $1 is required" >&2; exit 1; }; }
need curl; need openssl; need tar; need git; need python3; need mise

BUN="$(mise which bun 2>/dev/null || command -v bun || true)"
[ -n "$BUN" ] || { echo "setup: bun $(pin runtimes.bun) is required" >&2; exit 1; }
BUN_VERSION="$("$BUN" --version)"
if [ "$BUN_VERSION" != "$(pin runtimes.bun)" ]; then
  echo "setup: warning: bun $BUN_VERSION differs from the pinned $(pin runtimes.bun)" >&2
fi

mkdir -p "$CACHE"

# --- Pi: registry tarballs verified against their npm sha512 integrity ---------
PI="$CACHE/pi"
mkdir -p "$PI/tarballs"
for i in 0 1; do
  name="$(pin pi.tarballs.$i.name)"
  url="$(pin pi.tarballs.$i.url)"
  integrity="$(pin pi.tarballs.$i.integrity)"
  dir="$PI/$(pin pi.tarballs.$i.install_dir)"
  file="$PI/tarballs/$(basename "$url")"
  if [ ! -f "$file" ]; then
    curl -fsSL "$url" -o "$file.part"
    mv "$file.part" "$file"
  fi
  actual="sha512-$(openssl dgst -sha512 -binary "$file" | openssl base64 -A)"
  if [ "$actual" != "$integrity" ]; then
    echo "setup: integrity mismatch for $name: $actual" >&2
    rm -f "$file"
    exit 1
  fi
  rm -rf "$dir"
  mkdir -p "$dir"
  tar -xzf "$file" -C "$dir" --strip-components 1
  echo "setup: $name verified ($integrity)"
done

# --- OpenCode: packages/codemode at the pinned commit, plus pinned dependencies --
OC="$CACHE/opencode"
REPO="$OC/repo"
COMMIT="$(pin opencode.git_commit)"
if [ ! -d "$REPO/.git" ] || [ "$(git -C "$REPO" rev-parse HEAD 2>/dev/null)" != "$COMMIT" ]; then
  rm -rf "$REPO"
  mkdir -p "$REPO"
  git -C "$REPO" init -q
  git -C "$REPO" remote add origin "$(pin opencode.repository)"
  git -C "$REPO" config core.hooksPath /dev/null
  git -C "$REPO" sparse-checkout set --no-cone "/$(pin opencode.sparse_path)/"
  git -C "$REPO" fetch -q --depth 1 --filter=blob:none origin "refs/tags/$(pin opencode.git_tag)"
  fetched="$(git -C "$REPO" rev-parse 'FETCH_HEAD^{commit}')"
  if [ "$fetched" != "$COMMIT" ]; then
    echo "setup: tag $(pin opencode.git_tag) resolves to $fetched, expected $COMMIT" >&2
    exit 1
  fi
  git -C "$REPO" -c advice.detachedHead=false checkout -q "$COMMIT"
fi
[ "$(git -C "$REPO" rev-parse HEAD)" = "$COMMIT" ] || { echo "setup: OpenCode checkout is not $COMMIT" >&2; exit 1; }
git -C "$REPO" diff --quiet || { echo "setup: OpenCode checkout has local changes" >&2; exit 1; }
echo "setup: OpenCode $(pin opencode.git_tag) at $COMMIT"

cp "$BENCH/engines/opencode-deps/package.json" "$BENCH/engines/opencode-deps/bun.lock" "$OC/"
(cd "$OC" && "$BUN" install --frozen-lockfile --ignore-scripts --no-progress >/dev/null)
python3 -I - "$OC" <<'EOF'
import json, sys
root = sys.argv[1]
want = json.load(open(f"{root}/package.json"))["dependencies"]
for name, version in want.items():
    got = json.load(open(f"{root}/node_modules/{name}/package.json"))["version"]
    if got != version:
        sys.exit(f"setup: {name} {got} installed, {version} pinned")
print("setup: OpenCode dependencies", ", ".join(f"{k}@{v}" for k, v in want.items()))
EOF

# --- codemode: our release binary ---------------------------------------------
(cd "$ROOT" && mise exec -- cargo build --release --locked --bin codemode)
echo "setup: built $ROOT/target/release/codemode"
echo "setup: deno $(mise which deno)"
