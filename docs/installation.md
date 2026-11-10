# Versioned source installation

Install a reviewed Git commit as one bundle: `codemode`, the
`codemode-composio` launcher, matching TypeScript host adapters, declarations,
user docs, the installed-workflow check, and license notices. Fixtures, mock
tools, test harnesses, and demos are not installed. No registry package or
release binary is published.

## Prerequisites and install

Use Git, mise, and the Rust compiler/linker prerequisites for your platform
(including a C/C++ toolchain for bundled SQLite). The minimum supported Rust is
1.91; mise selects stable Rust. Every build compiles V8: the `v8` build script
downloads a prebuilt `rusty_v8` static library from GitHub releases on first
use. For offline or mirrored builds, set `RUSTY_V8_ARCHIVE` to a local file or
URL, or `RUSTY_V8_MIRROR` to a mirror. The release binary is about 75 MiB
(78,132,960 bytes on macOS arm64).

The installer uses Linux or macOS shell utilities. CI targets Linux; the
Composio workflow was verified on macOS. This is not a claim that every
architecture or platform has been verified.

Choose a full, reviewed 40-character commit SHA. The installer requires that
commit to exist locally, never fetches a moving branch, and ignores uncommitted
files. Build inputs and adapters come from the same `git archive` snapshot.

```sh
REVISION=replace-with-the-reviewed-40-character-commit-sha
git clone https://github.com/jkomyno/codemode.git
cd codemode
git checkout --detach "$REVISION"
mise trust
mise install rust mr-boxington deno
scripts/install.sh --revision "$REVISION" --prefix "$HOME/.local/share/codemode"
```

Deno is needed only by the install check below; the installed binary does not
use it. The script builds `codemode` with
`mise exec -- cargo build --release --locked --bin codemode`, through the
snapshot's mise configuration and Mr Boxington. It trusts that snapshot only for
the build process; it does not change persistent mise trust settings. Rust's
stable tool selection can advance, so the installation records the actual
compiler in `BUILD_COMPILER`; the commit and Cargo dependency graph are pinned,
but this is not a byte-for-byte reproducible-build claim. The only installation
destination is the required explicit `--prefix`; the script does not edit shell
profiles, host settings, or credentials.

The bundle is staged before activation:

```text
<prefix>/versions/<commit>/
  bin/codemode  bin/codemode-composio
  integrations/{shared,pi,opencode,composio}/
  types/  docs/  scripts/
  REVISION  BUILD_COMPILER
  README.md  LICENSE  THIRD-PARTY-NOTICES.md
<prefix>/current -> versions/<commit>
```

`current` changes atomically after a successful build and copy. Required binary,
launcher, adapter, declaration, documentation, and license files are checked
before activation, including when reusing an existing version. Failed builds or
incomplete bundles leave the previous activation intact. Existing versions are
preserved; concurrent installations under one prefix are refused. After an
abrupt kill, confirm the installer has stopped before removing a leftover
`.install-lock` directory.

## Use from any directory

Resolve `current` once for each session so the binary and adapters stay on the
same revision even if another installation activates an upgrade:

```sh
INSTALL=$(cd "$HOME/.local/share/codemode/current" && pwd -P)
cd /tmp
"$INSTALL/bin/codemode" --help
```

For Composio, the installed `bin/codemode-composio` launcher runs the bundled
GitHub host under `composio run` with the matching binary. The
[GitHub triage guide](composio-github.md) walks through the whole workflow:

```sh
"$INSTALL/bin/codemode-composio" investigate --account ca_... --repo owner/repo \
  --program "$INSTALL/integrations/composio/examples/triage.ts" --out plan.json
```

The launcher points `TMPDIR` at a private directory removed on exit, because the
Composio CLI writes each call's arguments, comment bodies included, to plaintext
files there. Composio CLI 0.6.0 temporarily copies its entry script next to the
original, so the installed `integrations/composio/` directory must remain
writable by its owner.

For Pi:

```sh
pi --extension "$INSTALL/integrations/pi/codemode-bridge.ts" \
  --codemode-bin "$INSTALL/bin/codemode" --codemode-tools read,grep,find,ls
```

For OpenCode, import the absolute path to
`$INSTALL/integrations/opencode/codemode-bridge.ts` in your plugin file and pass
`bin: "$INSTALL/bin/codemode"` as an actual expanded path string, together with
your host tools. OpenCode can only call those supplied tools; it exposes no
plugin API for invoking its built-ins.

Host-specific options are described in the installed
[Composio](../integrations/composio/README.md), [Pi](../integrations/pi/README.md),
and [OpenCode](../integrations/opencode/README.md) guides. Add `current/bin` to
your PATH if desired; the installer leaves that choice to you.

## Data the workflow leaves behind

A plan journal (`<journal>`, with `-wal`, `-shm`, and `.lock` beside it) keeps
the approved plan for its lifetime, because recovery needs it: every argument,
including full comment bodies, plus the account and destination scopes. It
also keeps approver and reviewer names and notes, each cut to 1 KiB. Deleting
the journal files removes all of it; delete them only when no command is using
the journal. See [what the journal stores](recovery.md#3-the-journal) and the
guide's [storage section](composio-github.md#what-is-stored-and-where).

## Verify, upgrade, and roll back

From the source checkout, with Deno on PATH:

```sh
mise exec -- scripts/install-check.sh "$INSTALL"
```

The check creates an empty working directory and uses only the installed bundle.
It runs a real bridge tool through the delivered shared adapter, saves and
inspects its trace, and executes the installed OpenCode adapter with a synthetic
permission context. It checks and approves a plan digest, runs and resumes it
through the installed plan bridge with the check itself as host and provider,
confirms the host executed exactly one effect, and inspects the journal and the
session's trace. Finally it runs `codemode-composio` with a stand-in `composio`
command and confirms the launcher passed the bundled host and binary and used a
private `TMPDIR` that it removed. The printed directory keeps the evidence. Pass
a second argument to choose an empty work directory. The installed
`scripts/install-check.sh` can also be called directly with Deno on PATH; no
source checkout is required.

This smoke check establishes artifact paths and adapter behavior with
synthetic host input. It does not call Composio or GitHub. The separate pinned
Pi 1.4.0 and OpenCode 1.19.5 offline checks exercise real host processes.
None of these establishes live models, interactive approval dialogs, live
provider writes, other host versions, power-loss durability, or that a person
can follow the workflow; the [operator trial](operator-trial.md) is for that.
See [host verification limits](integrations.md).

To upgrade, fetch the chosen commit explicitly into your local repository, review
it, then run the installer again with its full SHA and the same prefix. To roll
back, run the installer with a previously installed SHA; it reuses that bundle
and atomically reactivates it. `--source <repository>` names the local source
repository from another directory. Running hosts keep their resolved version
path; restart them to adopt an upgrade.

## Compatibility

The project is unreleased and makes no compatibility promises: removed commands,
options, formats, and protocols are deleted outright, and a binary refuses
journal, approval, and bridge-protocol versions other than its own. Finish or
abandon any open journal with the binary that wrote it before upgrading, and
keep that version installed until you have. Use the binary and adapters from one
installed commit together.
