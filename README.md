# codemode

A local Rust runtime for agent-written TypeScript/JavaScript tool workflows.
Designed for use from **Pi, OpenCode, Codex, and the Composio CLI**, with one
provider-independent core and host-specific adapters.

Rust owns three things: **run-level control**, **rolling tool scheduling**, and
**execution evidence**. A replaceable backend executes the guest program;
a trusted provider executes explicitly authorized tools outside it.

The current prototype uses Deno 2.9.8 as its guest backend. `codemode run` uses an
offline mock provider; `codemode bridge` lets a parent host execute the tools.
A native [Pi 1.4.0 integration](integrations/pi/README.md) uses the bridge to run
Pi's own tools with Pi's permission gates. An [OpenCode 1.19.5 plugin](integrations/opencode/README.md)
gates every call through OpenCode's permission service and abort, but runs only
host tools passed to it: OpenCode exposes no plugin API for calling its own tools.
Codex and a real Composio provider remain targets. See [host integration](docs/integrations.md) for what
each host has and has not been verified to do.

## Run it

Install Git, mise, and your platform's Rust compiler/linker prerequisites, then:

```sh
mise trust
mise install rust mr-boxington deno
mise exec -- cargo run -- run --code examples/compose.ts --concurrency 2 --trace /tmp/codemode-trace.json
```

With `--trace`, stdout contains a compact JSON object with `status`, the trace
path, and `output` when a guest value was observed (including JSON `null`). The file contains the full execution record. Without
`--trace`, stdout contains that record directly. Diagnostics use stderr.

The example calls four mock tools concurrently, preserves input ordering in its
returned array, and returns only two selected values. Rust starts queued calls as
provider slots become free. To inspect a controlled failure:

```sh
mise exec -- cargo run -- run --code examples/failure.ts
```

That command exits unsuccessfully and records the earlier completed call as well
as the provider failure. No service account or external tool call is involved.
Run `mise exec -- cargo run -- --help` for limits, stdin input, and exit behavior.

## Execution contract

- One absolute deadline includes child startup and provider work. Cancellation
  stops new dispatch and terminates the child. Remote effects cannot be undone.
- The host authorizes tool names and validates the mock tool arguments. A rolling
  scheduler limits active provider calls and the total helper-call allowance.
- Rust assigns trace identities and records outcomes. Child stdout is untrusted;
  it cannot supply authoritative provider results or trace events.
- Serialized tool replies and final output share a byte allowance. Frames are
  capped at 64 KiB. This is an output/transport limit, not a process-memory limit.
- A returned guest value, a provider result, and a run's terminal status are
  distinct. Unobserved dispatched calls remain unknown; queued calls never sent
  to a provider are identified separately. There are no automatic retries.

The Deno process receives a cleared environment and denied filesystem, network,
environment, process, system-information, FFI, and external-import permissions.
Guest code runs in a permission-restricted data-URL worker. Built-in and inline
`data:` imports and child stdio remain accessible. These runtime restrictions do
not establish OS isolation, hard memory containment, or crash recovery.

## Library, helpers, and providers

Reusable Rust APIs live in [src/lib.rs](src/lib.rs). The CLI owns argument parsing,
source input, signals, trace files, stdout, and exit codes in [src/main.rs](src/main.rs).
The [TypeScript declarations](types/codemode.d.ts) describe `tools.call` for the
mock tools through an augmentable `CodemodeTools` interface;
[`integrations/pi/pi-tools.d.ts`](integrations/pi/pi-tools.d.ts) adds Pi's built-ins.

A Rust `ToolProvider` receives an authorized `ToolCall` and a `CallContext`
containing the shared deadline and cancellation signal. It returns a JSON value
or `ProviderError`. Configure the authorized names separately from guest code.
Provider credentials belong on this trusted side. A host adapter must preserve
its own permission and approval behavior; a type declaration does not supply it.

`serve_bridge` (and `codemode bridge`) supplies a provider whose calls are sent
to the parent host over a versioned stdio protocol and answered by it. The host
decides which tool names are authorized; Rust still rejects everything else,
schedules calls, and writes the trace. To try it without credentials:

```sh
mise run pi-demo
mise run opencode-demo
```

Composio is one intended adapter. Its backend ToolRouter informs experiments
around scheduling, result movement, and outcome preservation. Authentication,
catalogs, billing, and persistent workbenches do not define this runtime's core.

## Development

```sh
mise run check
mise exec -- cargo +1.85.0 test --locked
```

Tests execute real guest programs with the mock provider and an in-process fake
bridge host, so Deno must be on the mise-managed PATH. `mise run pi-check` runs
the ignored Pi 1.4.0 integration tests and `mise run opencode-check` the OpenCode
1.19.5 ones; mise fetches each host on first use. Rust 1.85 is the MSRV. One Cargo package, safe Rust, and default
build profiles are retained. `target/` belongs to Mr Boxington; use
`mise exec -- mbx clean` for project cleanup.

See [GOAL.md](GOAL.md), [STRATEGY.md](STRATEGY.md), the [design](docs/design.md),
and [CONTRIBUTING.md](CONTRIBUTING.md). Performance and stronger isolation claims
need representative measurements and explicit evidence.

## License

[MIT](LICENSE). Vendored skills retain their notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
