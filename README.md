# codemode

A local Rust runtime for agent-written TypeScript/JavaScript tool workflows.
Designed for use from **Pi, OpenCode, Codex, and the Composio CLI**, with one
provider-independent core and host-specific adapters.

Rust owns three things: **run-level control**, **rolling tool scheduling**, and
**execution evidence**. A replaceable backend executes the guest program;
a trusted provider executes explicitly authorized tools outside it.

The default program backend uses Deno 2.9.8. An optional Rust embedding of V8
runs JavaScript/TypeScript in a dedicated worker without an external Deno
executable. Approved plans run directly through Rust and need neither engine.
`codemode bridge` lets a parent host execute the tools.
A native [Pi 1.4.0 integration](integrations/pi/README.md) uses the bridge to run
Pi's own tools with Pi's permission gates. An [OpenCode 1.19.5 plugin](integrations/opencode/README.md)
gates every call through OpenCode's permission service and abort, but runs only
host tools passed to it: OpenCode exposes no plugin API for calling its own tools.
An optional [Composio GitHub host](docs/composio-github.md) investigates a
repository with read-only Composio tools from `composio run`, which keeps the
credential in the host, and executes the reviewed change list as a recoverable
plan; its investigation has passed a live, read-only check. Codex remains a target. See
[host integration](docs/integrations.md) for what each host has and has not been
verified to do.

## Run it

For a versioned binary and matching adapters usable outside a checkout, follow
the [source installation guide](docs/installation.md). It installs one explicit
commit, preserves earlier versions for rollback, and includes an offline check
of the installed bridge, OpenCode adapter, trace inspection, and native plans.

For development from this checkout:

Install Git, mise, and your platform's Rust compiler/linker prerequisites, then:

```sh
mise trust
mise install rust mr-boxington deno
mise run recovery-demo   # offline: approve a plan, kill the executor, resume, review
mise run pi-demo         # offline: a scripted Pi model runs a program over the bridge
```

The binary has no built-in tools: programs run through `codemode bridge`, whose
parent host supplies and executes the authorized tools, and approved plans run
through `codemode plan run|resume --bridge`. Neither demo uses a service account
or an external tool call. `codemode inspect <trace.json>` asks a saved execution
record why it was slow, which calls ran, and which outcomes need reconciliation;
`codemode plan trace --journal <file> --session <n>` prints that record for a plan
session. Run `mise exec -- cargo run -- --help` for limits and exit behavior.

To check the optional embedded program backend:

```sh
mise run embedded-check
```

The default build supports Rust 1.85. The embedding feature is tested on Rust
1.99.1 and requires a newer compiler; its complete minimum is unverified.
See [embedded execution and measurements](docs/embedded-evaluation.md) for the
supported globals, deployment costs, and comparison with Deno and prewarming.

## Execution contract

- One absolute deadline includes child startup and provider work. Cancellation
  stops new dispatch and terminates the child. Remote effects cannot be undone.
  A host can start a guest ahead of a run (`PreparedGuest`, or `startBridge` in
  the bridge client); the deadline then starts at the hand-over, and the guest
  still serves only that run.
- The host authorizes tool names and validates their arguments. A rolling
  scheduler limits active provider calls and the total helper-call allowance.
  The first request beyond the allowance is recorded as rejected (`call_limit`)
  and ends the run, so a trace holds at most one call more than the allowance.
- Rust assigns trace identities and records outcomes. Child stdout is untrusted;
  it cannot supply authoritative provider results or trace events.
- Serialized tool replies and final output share a byte allowance. Each reply
  to the guest is capped at 1 MiB, and source and call frames from the guest at
  64 KiB. These are output/transport limits, not process-memory limits.
- A returned guest value, a provider result, and a run's terminal status are
  distinct. Unobserved dispatched calls remain unknown; queued calls never sent
  to a provider are identified separately. There are no automatic retries.

The Deno process receives a cleared environment and denied filesystem, network,
environment, process, system-information, FFI, and external-import permissions.
Guest code runs in a permission-restricted data-URL worker. Built-in and inline
`data:` imports and child stdio remain accessible. These runtime restrictions do
not establish OS isolation, hard memory containment, or crash recovery.

## Recoverable agent actions

**Review a batch of external changes, execute it, and know how to continue after
an interruption.** A plan lists operations (tool, arguments, provider account,
preconditions) with stable identities; `codemode plan check` shows its digest and
each tool's recovery contract, and `codemode plan approve --digest ...` binds an
approval to that digest, the provider's contracts, and the budgets.
`codemode plan run` executes it through the same scheduler, journaling each
operation in SQLite before it can reach the provider and after its result.
It submits the approved operation identities natively, with no generated program
or guest process.
After a crash, `codemode plan resume` asks the provider what happened, by
receipt or by current state, as the provider's own contract allows, continues
only what is safe, and leaves the rest for a person (`codemode plan review`).
`codemode plan inspect` says what was approved, attempted, changed, still
uncertain, and what can safely happen next.

```sh
mise run recovery-demo   # offline: ten label changes, a SIGKILL, reconciliation, a review
```

See [recoverable agent actions](docs/recovery.md) for the contract, the
durability guarantee (process crashes are tested; power loss is not), and the
limits. There is no exactly-once claim.

## Library, helpers, and providers

Reusable Rust APIs live in [src/lib.rs](src/lib.rs). The CLI owns argument parsing,
source input, signals, trace files, stdout, and exit codes in [src/main.rs](src/main.rs).
The [TypeScript declarations](types/codemode.d.ts) describe `tools.call` through
an augmentable `CodemodeTools` interface;
[`integrations/pi/pi-tools.d.ts`](integrations/pi/pi-tools.d.ts) adds Pi's built-ins.

A Rust `ToolProvider` receives an authorized `ToolCall` and a `CallContext`
containing the shared deadline and cancellation signal. It returns a JSON value
or `ProviderError`. Configure the authorized names separately from guest code.
Provider credentials belong on this trusted side. A host adapter must preserve
its own permission and approval behavior; a type declaration does not supply it.

Use `run_calls` with finite `NativeCall` requests when code execution is unnecessary.
For arbitrary programs, `RunConfig::backend` selects `GuestBackend::Deno` or
`GuestBackend::EmbeddedV8 { executable }`. Library users explicitly supply the
embedded worker binary; the CLI uses its own executable. Reports record the
actual backend, with no guest transport or lifecycle evidence for native runs.

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
COMPOSIO_GITHUB_ACCOUNT=ca_... mise run composio-live-check  # read-only, needs `composio login`
```

## Development

```sh
mise run check
mise run embedded-check
mise run typecheck
mise exec -- cargo +1.85.0 test --locked
```

Tests execute real guest programs with the mock provider and an in-process fake
bridge host, so Deno must be on the mise-managed PATH. `mise run pi-check` runs
the ignored Pi 1.4.0 integration tests and `mise run opencode-check` the OpenCode
1.19.5 ones; mise fetches each host on first use. Rust 1.85 is the default-build MSRV;
the optional embedding feature is checked separately on the current compiler.
One Cargo package, safe Rust, and default
build profiles are retained. `target/` belongs to Mr Boxington; use
`mise exec -- mbx clean` for project cleanup.

## Evaluation

The [embedded-engine comparison](docs/embedded-evaluation.md) measures the same
workflow at 18.9 ms with embedding versus 37.8 ms with Deno at zero provider
latency; prewarming lowers those to 8.0 and 12.6 ms. All 400 timed runs passed
the output and ledger checks. This trades a newer compiler and a 75 MiB binary
for lower local overhead; the default binary is 6 MiB. Deno remains the default.

`mise run bench` runs one realistic workflow, its large-result variant, a
permission denial, a deadline, a guest exception, a lost response, and a holdout
through codemode, Pi's codemode engine, and OpenCode's, on shared tools whose
results are checked twice and whose calls are recorded by an independent
ledger. With a guest started ahead of the run, codemode finished the
zero-latency workflow in 14.0 ms (from 44.4 ms), within 4 percent of OpenCode at
the same concurrency; Pi was faster under latency because it never queues a
call. See [the benchmark](docs/benchmark.md), [the comparison](docs/comparison.md),
and `mise run demo`.

A historical [dispatch evaluation](docs/evaluation.md), run before the
fixed-batch policy and its evaluation binary were removed, found rolling dispatch
finished mixed-latency tool work in 230 ms against 404 ms for fixed batches on
identical offline fixtures with the same budget.

See [GOAL.md](GOAL.md), [STRATEGY.md](STRATEGY.md), the [design](docs/design.md),
and [CONTRIBUTING.md](CONTRIBUTING.md). Performance and stronger isolation claims
need representative measurements and explicit evidence.

## License

[MIT](LICENSE). Vendored skills retain their notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
