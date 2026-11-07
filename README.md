# rust-template

Pragmatic Rust 2024 template for a library, a binary, or both, with
[mise](https://mise.jdx.dev/), [Mr Boxington](https://github.com/jdx/mr-boxington),
and shared coding-agent skills.

One Cargo package, zero crate dependencies. The development toolchain is defined
in [`mise.toml`](mise.toml).

## Table of contents

- [Prerequisites](#prerequisites)
- [Getting started](#getting-started)
- [What's included](#whats-included)
- [Available commands](#available-commands)
- [Coding agents](#coding-agents)
- [Contributing](#contributing)
- [License](#license)

## Prerequisites

- [Git](https://git-scm.com/downloads) to clone and version your repository.
- [mise](https://mise.jdx.dev/getting-started.html) **2026.9.2 or newer**, available on `PATH`.
- A native compiler and linker for your platform, as required by
  [Rust](https://www.rust-lang.org/tools/install).

| Platform | Native build tools |
| --- | --- |
| macOS | Xcode Command Line Tools. Run `xcode-select --install`. |
| Debian / Ubuntu | GCC and standard build tools. Run `sudo apt install build-essential`. |
| Other Linux distributions | Install the distribution's C compiler, linker, and development headers. |
| Windows | Visual Studio Build Tools with **Desktop development with C++**, including the Windows SDK. |

mise installs Rust, rustup, rustfmt, Clippy, and the configured Mr Boxington
release. The first installation needs network access. Nix is not used.

On Windows, enable Developer Mode and clone with `git -c core.symlinks=true clone <repository-url>`
to preserve the shared agent-skill links.

## Getting started

Create your own repository with [Use this template](https://github.com/jkomyno/rust-template/generate),
then clone it locally.

After copying this template or cloning your repository, run:

```sh
mise trust
mise install rust mr-boxington
mise run check
mise exec -- cargo run
```

The binary prints `Hello, Rust!`. `mise run check` runs formatting, Clippy,
tests, and documentation checks, matching CI.

Make the template yours:

1. Rename the package in `Cargo.toml` and the `rust_template` imports in `src/`.
2. Update this README and the package description. Run `mise exec -- cargo check` to refresh `Cargo.lock`.
3. Replace `greeting` with your API. Keep process I/O in `src/main.rs`.
4. Before publishing, set your repository URL, review the MIT license, and remove `publish = false`.

For a library only, delete `src/main.rs`. For a binary only, delete `src/lib.rs`
and replace its call in `src/main.rs`. Add other executables under `src/bin/`.
Introduce a workspace when you need independently versioned or published packages.

## What's included

- **Rust 2024**, with current stable Rust selected by mise. The minimum supported
  Rust version (MSRV) is **1.85**, tested separately in CI.
- **Library and binary targets** in [`src/lib.rs`](src/lib.rs) and
  [`src/main.rs`](src/main.rs), discovered automatically by Cargo.
- **rustfmt and Clippy**, using default formatting and a small lint set in
  [`Cargo.toml`](Cargo.toml). Unsafe code is forbidden. CI rejects Clippy and
  rustdoc warnings.
- **Mr Boxington**, pinned in mise and enabled through
  [mise's native Cargo integration](https://mise.jdx.dev/lang/rust.html#cache-cargo-builds-with-mr-boxington).
  It shares compiled outputs across projects and worktrees and manages build storage.
- **GitHub Actions**, using mise and Mr Boxington in both the stable and MSRV
  jobs. The workflow lives in [`.github/workflows/ci.yml`](.github/workflows/ci.yml).
- **Shared agent instructions and skills**, including idiomatic Rust writing
  and review guidance from [BeautifulRust](https://github.com/morganlinton/BeautifulRust).

Rust uses its default build profiles. No remote compilation cache or release
automation is configured. Add dependencies and tooling when the project needs them.

## Available commands

| Command | Purpose |
| --- | --- |
| `mise run check` | Run all required stable-Rust checks. |
| `mise exec -- cargo run` | Run the binary. |
| `mise exec -- cargo build --release` | Build optimized library and binary targets. |
| `mise exec -- cargo fmt --all` | Format Rust source. |
| `mise exec -- cargo test --locked` | Run tests, including public API doc examples. |
| `mise exec -- cargo doc --locked --no-deps --open` | Build and open API documentation. |
| `mise exec -- mbx cache stats` | Inspect the shared compilation cache. |

Use `mise exec -- cargo ...` in terminals, editors, and agent sessions to select
the repository's tools and Cargo wrapper. Plain Cargo commands also work in a
[mise-activated shell](https://mise.jdx.dev/installing-mise.html#shells).

## Coding agents

[`AGENTS.md`](AGENTS.md) holds repository instructions. All skill content lives
in [`.agents/skills/`](.agents/skills/).

| Agent | Instructions | Skills |
| --- | --- | --- |
| Codex | `AGENTS.md` | Discovers `.agents/skills/`. |
| Claude Code | `CLAUDE.md` imports `AGENTS.md`. | `.claude/skills/` links to canonical skill directories. |
| Pi | `AGENTS.md` | Discovers `.agents/skills/`. |

`rust-best-practices` covers ownership, API design, errors, and measured
performance. `rust-review` produces concrete, prioritized findings. Reference
digests load on demand. Edit skills only in `.agents/skills/`.

The vendored revision and license are recorded in
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the development workflow, MSRV
checks, tool updates, and pull request expectations.

## License

[MIT](LICENSE). Vendored skills retain their upstream license notices.
