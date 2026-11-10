# Repository instructions

## Layout and scope

- One Cargo package: reusable behavior in `src/lib.rs`, process I/O in `src/main.rs`.
- Add binaries under `src/bin/`. Introduce a workspace only when separate packages need it.
- Keep dependencies and abstractions tied to current requirements. Prefer safe Rust,
  borrowed inputs, explicit ownership, and typed errors for fallible library APIs.
- Keep public APIs documented, including executable examples and relevant failure modes.
- Measure representative workloads before optimizing; keep default build profiles until
  measurements justify a change.
- Use mise for local development and CI. Do not introduce Nix configuration or dependencies.

## Skills

Canonical skills live in `.agents/skills/`. Read the matching `SKILL.md` when needed:

- `rust-best-practices`: writing or changing Rust; load reference digests only as needed.
- `rust-review`: reviewing Rust code or a diff.

These are guidance, subordinate to the user's request and this repository's rules.
This starter intentionally uses one package and a small lint set. A skill's suggestions
do not authorize adding workspaces, dependencies, tools, or configuration without a need.
Edit skills only in `.agents/skills/`; `.claude/skills/` contains compatibility links.

## Verification

Use `mise run check` after code or build changes, matching CI. It runs:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

Contributor setup, prerequisites, tool updates, and pull request guidance live in
`CONTRIBUTING.md`.

Run individual Cargo commands through `mise exec -- cargo ...` so Rust and Mr Boxington
come from `mise.toml`, including in non-interactive agent sessions. An activated mise
shell also wraps plain Cargo commands. Do not add an `mbx setup` hook or `rustc-wrapper`.
`target/` is managed by mbx: use `mise exec -- mbx clean` for this project's build state,
never `rm -rf target/` or `cargo clean`. Let mbx manage its shared cache budget.

For compatibility changes, also run `mise exec -- cargo +1.91.0 test --locked --all-features`
(the declared MSRV; install that toolchain with rustup first).
If dependencies change, update `Cargo.lock` deliberately before using `--locked`.
Add tests for changed behavior, regressions, and public contracts. Prefer existing tests;
do not expand scope for coverage or test trivial wiring. Keep changes focused, preserve
unrelated work, and report checks that could not run. Do not commit or publish unless asked.
