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
An optional [Composio provider](integrations/composio/README.md) runs guest calls
as Composio tools from `composio run`, which keeps the credential in the host;
it has passed a live, read-only check. Codex remains a target. See
[host integration](docs/integrations.md) for what each host has and has not been
verified to do.

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
To ask a saved record why it was slow, which calls ran, and which outcomes need
reconciliation:

```sh
mise exec -- cargo run -- inspect /tmp/codemode-trace.json
```
Run `mise exec -- cargo run -- --help` for limits, stdin input, and exit behavior.

## Execution contract

- One absolute deadline includes child startup and provider work. Cancellation
  stops new dispatch and terminates the child. Remote effects cannot be undone.
- The host authorizes tool names and validates the mock tool arguments. A rolling
  scheduler limits active provider calls and the total helper-call allowance.
  The first request beyond the allowance is recorded as rejected (`call_limit`)
  and ends the run, so a trace holds at most one call more than the allowance.
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

Composio is the worked example of adding a provider: [`composio-provider.ts`](integrations/composio/composio-provider.ts)
supplies one executor function to the shared bridge client and maps Composio's
`successful`/`data`/`error` result to a bridge reply. Nothing in the Rust core
changed for it. Authentication, catalogs, billing, and persistent workbenches stay
outside this runtime.

```sh
mise run composio-demo        # offline
mise run composio-live-check  # read-only, needs `composio login`
```

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

## Evaluation

`mise run eval` compares sequential, fixed-batch (`--dispatch batch`), and rolling
dispatch on identical offline fixtures with the same budget, and checks every
run's outputs, outcomes, and policy behavior from its trace. On the measured
machine, rolling finished mixed-latency tool work in 230 ms against 404 ms for
batch, matched batch on equal latencies, and started more work before a failure.
See [the evaluation](docs/evaluation.md) for method, distributions, limits, and
which claims are offline or live.

See [GOAL.md](GOAL.md), [STRATEGY.md](STRATEGY.md), the [design](docs/design.md),
and [CONTRIBUTING.md](CONTRIBUTING.md). Performance and stronger isolation claims
need representative measurements and explicit evidence.

## License

[MIT](LICENSE). Vendored skills retain their notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
