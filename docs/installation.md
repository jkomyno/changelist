# Versioned source installation

Install a reviewed Git commit as one bundle: `codemode`, the offline
`codemode-fixture`, matching TypeScript adapters, declarations, examples, docs,
dependency locks, and license notices. No registry package or release binary is
published by this workflow. The default build uses Deno for programs; native
approved plans need no JavaScript engine.

## Prerequisites and install

Use Git, mise, and the Rust compiler/linker prerequisites for your platform
(including a C/C++ toolchain for bundled SQLite). The installer uses Linux or
macOS shell utilities. CI targets Linux; local checks must be reported with their
actual platform. This is not a claim that every architecture or platform has
been verified.

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

The script builds both executables with `mise exec -- cargo build --release
--locked`, through the snapshot's mise configuration and Mr Boxington. It trusts
that snapshot only for the build process; it does not change persistent mise
trust settings. Rust's stable tool selection can advance, so the installation
records the actual compiler in `BUILD_COMPILER`; the commit and Cargo dependency
graph are pinned, but this is not a byte-for-byte reproducible-build claim.
mise, Rust, and Cargo use their normal tool/download/build caches. The only
installation destination is the required explicit `--prefix`; the script does
not edit shell profiles, host settings, or credentials.

The bundle is staged before activation:

```text
<prefix>/versions/<commit>-deno/
  bin/codemode
  bin/codemode-fixture
  integrations/{shared,pi,opencode,composio}/
  types/  examples/  docs/  scripts/
  REVISION  BUILD_BACKEND  BUILD_COMPILER
  Cargo.lock  package-lock.json  LICENSE  THIRD-PARTY-NOTICES.md
<prefix>/current -> versions/<commit>-deno
```

`current` changes atomically after a successful build and copy. Required binary,
adapter, declaration, example, documentation, and license files are checked
before activation, including when reusing an existing version. Failed builds or
incomplete bundles leave the previous activation intact. Existing versions are preserved; concurrent
installations under one prefix are refused. After an abrupt kill, confirm the
installer has stopped before removing a leftover `.install-lock` directory.

For the optional engine, install the same revision with
`--backend embedded-v8`. This adds the Cargo feature to the build; program
execution still defaults to Deno until you explicitly select `embedded-v8`.
The default build supports Rust 1.85. Embedding is tested on Rust 1.99.1 and
requires a newer compiler; its complete minimum is unverified. See
[embedded execution](embedded-evaluation.md).

## Use from any directory

Resolve `current` once for each host session so its binary and adapter stay on
the same revision even if another installation activates an upgrade:

```sh
INSTALL=$(cd "$HOME/.local/share/codemode/current" && pwd -P)
cd /tmp
"$INSTALL/bin/codemode" run --code "$INSTALL/examples/compose.ts" \
  --deno "$(command -v deno)" --trace "$PWD/codemode-trace.json"
"$INSTALL/bin/codemode" inspect "$PWD/codemode-trace.json"

pi --extension "$INSTALL/integrations/pi/codemode-bridge.ts" \
  --codemode-bin "$INSTALL/bin/codemode" --codemode-tools read,grep,find,ls
```

Pi's `--codemode-backend embedded-v8`, OpenCode's `backend: "embedded-v8"`,
and Composio's `--backend embedded-v8` select the optional engine.
The chosen binary must have been built with the feature. Deno remains the
default, and unsupported engine names are refused. Native plans ignore this
program-engine choice.

For OpenCode, import the absolute path to
`$INSTALL/integrations/opencode/codemode-bridge.ts` in your plugin file and pass
`bin: "$INSTALL/bin/codemode"` as an actual expanded path string, together with
your host tools. OpenCode can only call those supplied tools. It exposes no
plugin API for invoking its built-ins. For Composio, use the installed
`integrations/composio/host.ts` entrypoint with `--bin` set to the
matching binary:

```sh
composio run --file "$INSTALL/integrations/composio/host.ts" -- \
  --program "$PWD/my-program.ts" --tools SLUG_A,SLUG_B \
  --bin "$INSTALL/bin/codemode" --deno "$(command -v deno)"
```

Composio CLI 0.6.0 temporarily copies its entrypoint next to the original, so
that installed integration directory must remain writable by its owner.
Account setup and host-specific options are described in the
installed [Pi](../integrations/pi/README.md),
[OpenCode](../integrations/opencode/README.md), and
[Composio](../integrations/composio/README.md) guides. Add `current/bin` to your
PATH if desired; the installer leaves that choice to you.

## Verify, upgrade, and roll back

From the source checkout, with Deno on PATH:

```sh
mise exec -- scripts/install-check.sh "$INSTALL" deno
# For a feature-enabled installation:
mise exec -- scripts/install-check.sh "$INSTALL" embedded-v8
```

The check creates an empty working directory and uses only the installed bundle.
It imports the delivered shared adapter, runs a real bridge tool, saves and
inspects its trace, and executes the installed OpenCode adapter with a synthetic
permission context. It then initializes the installed offline fixture, checks
and approves a plan digest, runs and resumes it with a nonexistent engine path,
and checks the fixture ledger for exactly one effect. The printed directory
retains traces, journal, and provider evidence. Pass a third argument to select
an empty work directory. The installed `scripts/install-check.sh` can also be
called directly with Deno on PATH; no source checkout is required for the check.
Deno runs the host harness even when the guest engine is embedded V8.

This smoke check establishes artifact paths and adapter behavior with synthetic
host input. The separate pinned Pi 1.4.0 and OpenCode 1.19.5 offline CI checks
exercise real host processes. Neither establishes live models, interactive
approval dialogs, live provider mutations, other host versions, or power-loss
durability. See [host verification limits](integrations.md).

To upgrade, fetch the chosen commit explicitly into your local repository, review
it, then run the installer again with its full SHA and the same prefix. To roll
back, run the installer with a previous installed SHA and its original backend;
it reuses that bundle and atomically reactivates it. `--source <repository>` lets
you name the local source repository from another directory. Existing running
hosts should keep their resolved version path; restart them to adopt an upgrade.
Keep the prior version available until its journals and integrations are checked.

## Compatibility and deprecation

The supported distribution unit is the matching binary-plus-adapter commit.
The Rust crate remains unpublished and its public API is not stabilized. Do not
assume arbitrary revisions or adapters from different commits are compatible.

| Surface | Current format and policy |
| --- | --- |
| Saved execution traces | Schema 3 records the execution backend, including native calls. Older records can omit backend fields and are interpreted as historical Deno execution. Preserve original traces; consumers must inspect `schema_version` before interpreting unfamiliar fields. |
| Program stdio bridge | Protocol 2; Rust also accepts protocol 1 starts for legacy hosts. Provider milestone facts require protocol 2. The delivered shared adapter sends version 2. |
| Plan stdio bridge | Protocol 3; other start versions are refused. Scope, authorization, reconciliation, and journal evidence belong to this separate contract. |
| Plan JSON | Format 1. Approval binds the canonical digest, provider contracts, and budgets. A presentation change must not change that digest. |
| SQLite journal | Format 1. Opening another format fails closed; there is no automatic migration or downgrade. Preserve the journal and its matching binary before upgrading. |
| Host API checks | Pi 1.4.0 and OpenCode 1.19.5, pinned independently of the bundle revision. Offline checks and TypeScript checking establish distinct evidence. |

Additive fields must retain conservative defaults for historical evidence;
breaking serialized changes require a format/protocol version change and a
documented migration or explicit refusal. Never reinterpret an unknown external
effect as success during an upgrade. A future journal migration must preserve
approval bindings and recorded evidence and operate on a backup first.

Before removing a documented CLI option, TypeScript export, or accepted protocol,
announce the deprecation and replacement in the docs, retain it for at least one
tagged minor version, and add a migration note with the removal. This source
installation does not itself declare a stable public Rust API or publish a
release. Review exported helpers against actual consumers before stabilization.
