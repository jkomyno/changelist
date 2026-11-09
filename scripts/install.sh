#!/bin/sh
# Install one committed source snapshot, including its matching host adapters.
set -eu

usage() {
  echo 'Usage: scripts/install.sh --revision <full-commit-sha> --prefix <directory> [--source <local-repository>]'
}

source_repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
revision=
prefix=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --revision|--prefix|--source)
      [ "$#" -ge 2 ] || { usage >&2; exit 2; }
      case "$1" in
        --revision) revision=$2 ;;
        --prefix) prefix=$2 ;;
        --source) source_repo=$2 ;;
      esac
      shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
[ -n "$prefix" ] && [ "${#revision}" -eq 40 ] || { usage >&2; exit 2; }
case "$revision" in *[!0-9a-f]*) echo 'revision must be a full lowercase commit SHA' >&2; exit 2 ;; esac
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
version="$revision"
destination="$prefix/versions/$version"
# The installed files: the production binary, its adapters, declarations, user
# docs, and the installed-workflow check. Fixtures, mock tools, test harnesses,
# demos, and checkout-only scripts are never installed.
ASSETS='bin/codemode
integrations/README.md
integrations/shared/bridge-client.ts
integrations/pi/README.md
integrations/pi/codemode-bridge.ts
integrations/pi/pi-tools.d.ts
integrations/opencode/README.md
integrations/opencode/codemode-bridge.ts
integrations/composio/README.md
integrations/composio/host.ts
integrations/composio/plan-host.ts
integrations/composio/composio-plan-host.ts
integrations/composio/composio-provider.ts
integrations/composio/composio-plan.ts
integrations/composio/composio-tools.d.ts
types/codemode.d.ts
docs/installation.md
docs/integrations.md
docs/recovery.md
scripts/install-check.sh
scripts/install-smoke.ts
README.md
LICENSE
THIRD-PARTY-NOTICES.md'
validate_bundle() {
  bundle=$1
  [ -d "$bundle" ] && [ ! -L "$bundle" ] || return 1
  # Keep these required public entrypoints and their imports together on reuse.
  for asset in REVISION BUILD_COMPILER $ASSETS; do
    [ -f "$bundle/$asset" ] && [ ! -L "$bundle/$asset" ] || {
      echo "missing installed asset: $bundle/$asset" >&2
      return 1
    }
  done
  [ "$(cat "$bundle/REVISION")" = "$revision" ] &&
    [ -x "$bundle/bin/codemode" ] && [ -x "$bundle/scripts/install-check.sh" ]
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
    mise exec -- cargo build --release --locked --bin codemode
    mkdir "$stage/bin"
    cp target/release/codemode "$stage/bin/"
    for asset in $ASSETS; do
      case "$asset" in
        bin/*) ;;
        *)
          mkdir -p "$stage/$(dirname -- "$asset")"
          cp -p "$asset" "$stage/$asset" ;;
      esac
    done
    mise exec -- rustc --version > "$stage/BUILD_COMPILER"
  )
  printf '%s\n' "$revision" > "$stage/REVISION"
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
