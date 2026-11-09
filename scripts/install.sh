#!/bin/sh
# Install one committed source snapshot, including its matching host adapters.
set -eu

usage() {
  echo 'Usage: scripts/install.sh --revision <full-commit-sha> --prefix <directory> [--backend deno|embedded-v8] [--source <local-repository>]'
}

source_repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
revision=
prefix=
backend=deno
while [ "$#" -gt 0 ]; do
  case "$1" in
    --revision|--prefix|--backend|--source)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      case "$1" in
        --revision) revision=$2 ;;
        --prefix) prefix=$2 ;;
        --backend) backend=$2 ;;
        --source) source_repo=$2 ;;
      esac
      shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
[ -n "$prefix" ] && [ "${#revision}" -eq 40 ] || { usage >&2; exit 2; }
case "$revision" in *[!0-9a-f]*) echo 'revision must be a full lowercase commit SHA' >&2; exit 2 ;; esac
case "$backend" in deno|embedded-v8) ;; *) usage >&2; exit 2 ;; esac
case "$(uname -s)" in Linux|Darwin) ;; *) echo 'source installation supports Linux and macOS shell tools' >&2; exit 2 ;; esac
resolved=$(git -C "$source_repo" rev-parse --verify "$revision^{commit}")
[ "$resolved" = "$revision" ] || { echo 'revision must identify an existing commit in the local repository' >&2; exit 2; }
mkdir -p "$prefix"
prefix=$(CDPATH='' cd -- "$prefix" && pwd -P)
if [ -e "$prefix/current" ] && [ ! -L "$prefix/current" ]; then
  echo "$prefix/current must be a symlink or absent" >&2
  exit 2
fi
mkdir "$prefix/.install-lock" 2>/dev/null || {
  echo "another installation owns $prefix/.install-lock; remove it only after confirming that installer has stopped" >&2
  exit 1
}
snapshot=
stage=
activation_dir=
cleanup() {
  [ -z "$snapshot" ] || rm -rf -- "$snapshot"
  [ -z "$stage" ] || rm -rf -- "$stage"
  [ -z "$activation_dir" ] || rm -rf -- "$activation_dir"
  rmdir "$prefix/.install-lock"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
mkdir -p "$prefix/versions"
version="$revision-$backend"
destination="$prefix/versions/$version"
validate_bundle() {
  bundle=$1
  [ -d "$bundle" ] && [ ! -L "$bundle" ] || return 1
  # Keep these required public entrypoints and their imports together on reuse.
  while IFS= read -r asset; do
    [ -f "$bundle/$asset" ] && [ ! -L "$bundle/$asset" ] || {
      echo "missing installed asset: $bundle/$asset" >&2
      return 1
    }
  done <<'ASSETS'
REVISION
BUILD_BACKEND
BUILD_COMPILER
bin/codemode
bin/codemode-fixture
integrations/shared/bridge-client.ts
integrations/README.md
integrations/pi/codemode-bridge.ts
integrations/pi/pi-tools.d.ts
integrations/pi/README.md
integrations/opencode/codemode-bridge.ts
integrations/opencode/README.md
integrations/composio/host.ts
integrations/composio/plan-host.ts
integrations/composio/composio-provider.ts
integrations/composio/composio-plan.ts
integrations/composio/composio-tools.d.ts
integrations/composio/README.md
integrations/composio/demo/fake-composio.ts
integrations/composio/demo/fake-labels.ts
types/codemode.d.ts
examples/compose.ts
examples/failure.ts
examples/recovery/plan.json
docs/installation.md
docs/integrations.md
docs/recovery.md
scripts/install.sh
scripts/install-check.sh
scripts/install-smoke.ts
README.md
CONTRIBUTING.md
LICENSE
THIRD-PARTY-NOTICES.md
Cargo.toml
Cargo.lock
mise.toml
package.json
package-lock.json
tsconfig.json
ASSETS
  [ "$(cat "$bundle/REVISION")" = "$revision" ] &&
    [ "$(cat "$bundle/BUILD_BACKEND")" = "$backend" ] &&
    [ -x "$bundle/bin/codemode" ] && [ -x "$bundle/bin/codemode-fixture" ] &&
    [ -x "$bundle/scripts/install.sh" ] && [ -x "$bundle/scripts/install-check.sh" ]
}
if [ -e "$destination" ] || [ -L "$destination" ]; then
  validate_bundle "$destination" || {
    echo "refusing to reuse an incomplete installation: $destination" >&2
    exit 1
  }
else
  command -v mise >/dev/null || { echo 'mise must be on PATH' >&2; exit 1; }
  snapshot=$(mktemp -d "${TMPDIR:-/tmp}/codemode-source.XXXXXX")
  git -C "$source_repo" archive --format=tar --output="$snapshot/source.tar" "$revision"
  mkdir "$snapshot/source"
  tar -xf "$snapshot/source.tar" -C "$snapshot/source"
  stage=$(mktemp -d "$prefix/versions/.stage.XXXXXX")
  (
    cd "$snapshot/source"
    # Trust only this archived commit for this process; do not alter mise trust state.
    export MISE_TRUSTED_CONFIG_PATHS="$snapshot/source"
    unset CARGO_TARGET_DIR
    if [ "$backend" = embedded-v8 ]; then
      mise exec -- cargo build --release --locked --features embedded-v8 --bin codemode --bin codemode-fixture
    else
      mise exec -- cargo build --release --locked --bin codemode --bin codemode-fixture
    fi
    mkdir "$stage/bin"
    cp target/release/codemode target/release/codemode-fixture "$stage/bin/"
    cp -R integrations types examples docs scripts "$stage/"
    cp README.md CONTRIBUTING.md LICENSE THIRD-PARTY-NOTICES.md Cargo.toml Cargo.lock mise.toml package.json package-lock.json tsconfig.json "$stage/"
    mise exec -- rustc --version > "$stage/BUILD_COMPILER"
  )
  printf '%s\n' "$revision" > "$stage/REVISION"
  printf '%s\n' "$backend" > "$stage/BUILD_BACKEND"
  validate_bundle "$stage" || { echo 'refusing to activate an incomplete installation' >&2; exit 1; }
  mv "$stage" "$destination"
  stage=
fi
activation_dir=$(mktemp -d "$prefix/.activate.XXXXXX")
activation="$activation_dir/current"
ln -s "versions/$version" "$activation"
# Both variants replace the symlink itself atomically, including a directory target.
case "$(uname -s)" in
  Darwin) mv -fh "$activation" "$prefix/current" ;;
  Linux) mv -fT "$activation" "$prefix/current" ;;
esac
printf 'Installed %s\nBinary: %s/current/bin/codemode\nAdapters: %s/current/integrations\n' "$version" "$prefix" "$prefix"
