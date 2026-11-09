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
`mise.toml` selects stable Rust, a specific Mr Boxington release, and the
prototype's Deno execution backend.
`Cargo.toml` declares the default build's minimum supported Rust version, currently
1.85. The optional `embedded-v8` feature uses newer Deno embedding dependencies;
it is tested on Rust 1.99.1, with no complete minimum claimed for that feature.

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
mise run embedded-check
```

The task checks formatting, runs Clippy on all targets and features, runs tests
including doc tests, and builds documentation. Clippy and rustdoc warnings fail
the check. `embedded-check` executes the real embedded-worker and backend tests;
CI runs it after `check`. To fix formatting, run `mise exec -- cargo fmt --all`.

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
mise exec -- rustup toolchain install 1.85.0 --profile minimal
mise exec -- cargo +1.85.0 test --locked
```

Keep that MSRV run on default features. Use the current mise compiler for
`--features embedded-v8` or `--all-features`; enabling the optional engine does
not change the compatibility promise of the default library and native plans.

Both [CI jobs](.github/workflows/ci.yml) install tools with mise and run Cargo
through Mr Boxington. CI currently runs on Linux. Check platform-specific changes
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
