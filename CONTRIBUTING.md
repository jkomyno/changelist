# Contributing

## Set up development

Follow the [setup instructions in the README](README.md#run-it), then
run these commands from the repository root:

```sh
mise trust
mise install rust mr-boxington deno
mise run check
```

Use mise for both development and CI. Nix is not part of the toolchain.
`mise.toml` selects stable Rust, a specific Mr Boxington release, and the Deno
release that runs the TypeScript host tests and scripts; codemode itself does not use Deno.
`Cargo.toml` declares the minimum supported Rust version, currently 1.91, which the
embedded V8 dependencies require.

Every build compiles V8: the `v8` build script downloads a prebuilt `rusty_v8` static
library (tens of MiB) from GitHub releases on first use. Offline or mirrored builds
can set `RUSTY_V8_ARCHIVE` to a local file or URL, or `RUSTY_V8_MIRROR` to a mirror.

## Make a change

Keep each change focused on one problem. Put reusable behavior in `src/lib.rs`
and process I/O in `src/main.rs`. Add dependencies only when the change needs them.

Document public APIs with runnable examples and relevant error or panic
conditions. Add or update tests for changed behavior, bug regressions, or public
contracts. Prefer existing tests over duplicate coverage.

Use `#[cfg(test)] mod tests` for private internals, `tests/` for public API
integration tests, and rustdoc examples for usage. Avoid tests that only check
trivial wiring or implementation details.

## Run checks

Before submitting a code or build change, run:

```sh
mise run check
mise run typecheck
```

The task checks formatting, runs Clippy on all targets and features, runs tests
including doc tests, and builds documentation. Clippy and rustdoc warnings fail
the check. To fix formatting, run `mise exec -- cargo fmt --all`.
`typecheck` installs the locked development-only npm dependencies without install
scripts and checks delivered adapters against the pinned Pi and OpenCode APIs.
Node is managed by mise; it is not needed to use the installed Rust CLI.

For changes to the bridge or `integrations/`, also run the host checks, which
fetch Pi 1.4.0 and OpenCode 1.19.5 through mise and need no credentials:

```sh
mise run pi-check
mise run opencode-check
```

The offline Composio adapter tests run in `mise run check`. With `composio login`
and GitHub and Slack connected, `mise run composio-live-check` executes read-only
tools; report its result separately from offline checks.

For compatibility changes, also test the declared MSRV:

```sh
mise exec -- rustup toolchain install 1.91.0 --profile minimal
mise exec -- cargo +1.91.0 test --locked --all-features
```

The [CI workflow](.github/workflows/ci.yml) installs tools with mise and runs Cargo
through Mr Boxington. It also installs the exact CI commit and exercises its
binary, adapters, trace inspection, and native plan workflow from an empty
directory. Pi and OpenCode checks run on bridge, integration, build, and test
changes, and on manual workflow runs. These offline checks do not establish live
model, interactive UI, or live-provider behavior.

For installation changes, commit the complete bundle before running the real
source-install boundary check; the installer intentionally excludes uncommitted
files. Use an isolated prefix:

```sh
scripts/install.sh --revision "$(git rev-parse HEAD)" --prefix /tmp/codemode-install
mise exec -- scripts/install-check.sh /tmp/codemode-install/current
```

See [installation and compatibility](docs/installation.md) for upgrade, rollback,
engine selection, and compatibility (there is none to preserve before a release).
CI currently runs on Linux. Check platform-specific changes
on the affected platform and report what you tested.

## Manage dependencies and build storage

Use `mise exec -- cargo add <crate>` when adding a dependency. Include the
resulting `Cargo.toml` and `Cargo.lock` changes together. Keep `Cargo.lock` under
version control, including for library development. The `--locked` checks reject
unrecorded dependency changes.

mise's `mr_boxington = true` option wraps Cargo with mbx. Use `mise exec -- cargo
...` in non-interactive sessions, or activate mise in your shell. Editors should
inherit the same environment. Do not add `mbx setup` hooks or a `rustc-wrapper`.

mbx manages `target/` as a symlink and collects old build state automatically.
Inspect the cache with `mise exec -- mbx cache stats`. Clear this project's build
state with `mise exec -- mbx clean`. Do not use `cargo clean` or delete `target/`
directly. A remote compilation cache is not configured.

## Update tools and skills

Change tool selections in `mise.toml`, then run `mise install rust mr-boxington deno`
and `mise run check`. Stable Rust follows a moving channel. Pin an exact Rust
release there if your project needs a fixed compiler.

If the MSRV changes, update `Cargo.toml`, the CI job, and the documented commands
in README, AGENTS, and this file together. Test the new minimum toolchain.

Keep skill content in `.agents/skills/`. For a new skill, add a relative link in
`.claude/skills/`, following the existing links. Preserve the upstream license
and revision when updating vendored skills. The update procedure is recorded in
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

## Submit a pull request

Explain the problem, resulting behavior, and checks you ran. Include a reproducer
for a bug fix. Note any checks you could not run and any public API or MSRV changes.
Use small Conventional Commits, such as `fix: preserve empty input`.

Measure representative workloads before claiming a performance improvement.
Publishing remains disabled until the package metadata and release process are
configured for the project.
