# codemode

codemode lets an agent change several GitHub issues through the Composio CLI and
lets you know, after a crash or a lost reply, exactly what happened. An
investigating program reads GitHub with authorized tools and proposes a change
list. You review and approve that exact list, codemode executes it under
host-owned limits with a durable journal, and after an interruption it
reconciles what it can with GitHub and hands the rest to a person.

It is a small Rust CLI (`codemode`) plus a Composio host
(`integrations/composio/host.ts`) run by the installed `codemode-composio`
launcher. Status: the investigation path ran live, read-only, on 2026-11-10
([record](docs/composio-live-validation.md)); the four write contracts are tested
offline against a stand-in GitHub and are not yet validated live.

## Install

Install one reviewed commit into a prefix, from a clone of this repository:

```sh
git checkout --detach "$REVISION"     # a full 40-character commit SHA
mise trust && mise install rust mr-boxington deno
scripts/install.sh --revision "$REVISION" --prefix "$HOME/.local/share/codemode"
INSTALL=$(cd "$HOME/.local/share/codemode/current" && pwd -P)
```

The bundle holds `bin/codemode`, the `bin/codemode-composio` launcher, the host
adapters, type declarations, and the user guides. See
[installation](docs/installation.md) for prerequisites, the offline install
check, upgrade, and rollback.

## The workflow

Each step is a command in the [GitHub triage guide](docs/composio-github.md),
which explains every option and output.

1. **Connect.** Run `composio login` and `composio link github`, then take the
   connected-account id (`ca_...`) from `composio connections list`. No project
   SDK key is needed.
2. **Investigate.** `codemode-composio investigate --account ca_... --repo owner/repo --program triage.ts --out plan.json`
   runs a program (the bundle includes `examples/triage.ts`) once, with GitHub
   read tools for that repository only, then writes its proposal as `plan.json`
   and checks it.
3. **Declare.** `codemode-composio declare ... > declaration.json` prints the
   recovery contract of each of the four writes: add a label, remove a label,
   set issue state, and create a marked comment.
4. **Review.** `codemode plan check plan.json --provider declaration.json` shows
   every operation exactly as approval binds it: tool, account, repository,
   canonical arguments in full, contracts, retention, budgets, and the digest.
   `codemode-composio preflight` optionally confirms labels and preconditions
   against GitHub, read-only.
5. **Approve.** `codemode plan approve plan.json --journal triage.db --provider declaration.json --digest sha256:... --approver you`.
   Any edit to the plan changes its digest and needs a new approval.
6. **Run and watch.** `codemode-composio run ... --journal triage.db --deadline-ms 300000`
   executes the plan. From a second terminal, `codemode plan inspect --journal triage.db`
   shows committed progress without writing to the journal.
7. **Interrupt and resume.** After Ctrl-C or a crash,
   `codemode-composio resume ... --journal triage.db` probes each uncertain
   operation by its contract, continues only what is safe, and leaves the rest
   for `codemode plan review`.
8. **Diagnose.** `codemode plan inspect --journal triage.db --json` exports which
   operations were applied, satisfied, not applied, unknown, or refused, without
   argument values.

To try investigation, review, approval, a run, and inspection offline, against
a stand-in GitHub with its own ledger:

```sh
mise run composio-demo
```

## Execution contract

- **Host-owned limits.** `codemode bridge` runs each program in a fresh V8
  worker process started from the `codemode` executable. One deadline
  (`--timeout-ms`, including worker startup), a call allowance (`--max-calls`),
  a concurrency cap (`--concurrency`), and an output budget
  (`--max-output-bytes`) are enforced in Rust; guest code cannot enlarge them.
- **Authorization outside the guest.** The host names the tools a program may
  call; Rust refuses every other name before the host sees it. Credentials stay
  in the host process. The worker gets an empty environment and no imports,
  filesystem, network, process, or FFI access.
- **Typed outcomes.** A returned value, a provider result, and the run status
  are separate. A dispatched call without an observed reply is `unknown`, never
  failed. There are no automatic retries.
- **Memory, honestly.** `--heap-mb` (default 256) bounds the V8 JavaScript
  heap; reaching it ends the run with status `memory_limit`. It is not a
  process-memory limit: `ArrayBuffer` backing stores and native allocations sit
  outside it. On Linux the worker also gets a data-segment limit
  (`RLIMIT_DATA`, default heap budget + 256 MiB, `--process-memory-mb`), which
  bounds those too. On macOS no process limit is applied, and
  `--process-memory-mb` is refused before a worker starts. WebAssembly,
  `SharedArrayBuffer`, and `Atomics` are removed from the guest. A worker that
  dies for another reason ends the run with `worker_crashed`.
- **Evidence.** Every run produces a JSON execution record (report schema 5).
  `codemode inspect <trace.json>` answers why it was slow, which calls ran, and
  which outcomes need reconciliation, without printing argument or result
  values.

These restrictions are not OS isolation.

## Recoverable plans

A plan is a finite JSON list of operations, each with a stable id, tool,
arguments, scope (account and destination), and optional preconditions. The
program that proposed it is never replayed. Approval binds the plan digest, the
provider's account and contracts, the retention policy, and the attempt, probe,
and concurrency budgets.

During `plan run` and `plan resume`, each attempt's intent is committed to a
SQLite journal before the provider sees it, and each observation is committed
before anyone is told of it. One session deadline (`--deadline-ms`) covers
authorization, reconciliation, permission waits, and dispatch. One executor
holds a journal at a time through `<journal>.lock`; `plan inspect` and
`plan trace` read it read-only while a session runs. After an interruption,
only the provider's declared contract can settle an uncertain operation; what
it cannot settle goes to a person. There is no exactly-once claim.

The journal keeps the approved arguments, including full comment bodies,
because recovery needs them; deleting the journal files removes them. See
[recoverable agent actions](docs/recovery.md) for the contract, durability
(process crashes are tested; power loss is not), and limits.

## Other hosts

The same executor is reachable from two secondary hosts, each pinned to the
version its offline checks use:

- [Pi 1.4.0](integrations/pi/README.md): programs call Pi's own tools through
  Pi's permission gates, and approved plans run and recover through Pi.
- [OpenCode 1.19.5](integrations/opencode/README.md): each call passes
  OpenCode's permission service, but only host tools passed to the plugin can be
  called; OpenCode exposes no plugin API for its own tools.

See [host integration](docs/integrations.md) for the bridge protocols and what
each host has and has not been verified to do. Codex is not integrated.

## Development

```sh
mise trust && mise install
mise run check          # fmt, clippy, tests, docs
mise run typecheck      # host adapters against pinned host APIs
mise run pi-check       # pinned Pi 1.4.0, offline
mise run opencode-check # pinned OpenCode 1.19.5, offline
mise run recovery-demo  # offline: approve a batch, kill the executor, resume, review
```

The minimum supported Rust is 1.91. Every build compiles V8 from a prebuilt
`rusty_v8` archive downloaded on first use. Deno is installed by mise only to
run the TypeScript host tests and scripts; codemode does not use it. See
[CONTRIBUTING.md](CONTRIBUTING.md), the [design](docs/design.md), and the
[strategy](STRATEGY.md).

## Evaluation

See [docs/comparison.md](docs/comparison.md) for how this workflow compares
with Pi Durable, OpenCode Code Mode, and a Composio CLI script. Earlier
measurements ([evaluation](docs/evaluation.md),
[benchmark](docs/benchmark.md), [recovery](docs/recovery-evaluation.md),
[embedded engine](docs/embedded-evaluation.md)) are historical records of
engines and options that have since changed.

## License

[MIT](LICENSE). Vendored skills retain their notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
