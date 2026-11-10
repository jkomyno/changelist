# Composio GitHub host

A host for the Composio CLI (verified with 0.6.0) that runs one workflow: a
program investigates a GitHub repository with read-only tools, its proposal
becomes a plan, and codemode executes the approved plan with a journal and
recovers it after an interruption. The [guide](../../docs/composio-github.md)
walks through it end to end.

The Rust core stays provider-independent. `composio run` injects an
authenticated `execute` helper into [`host.ts`](host.ts) and keeps the CLI
credential in that process; codemode runs with an empty environment.

```text
codemode-composio investigate|declare|preflight|run|resume --account <ca_...> --repo <owner/repo> ...
  = composio run --file host.ts -- ... --bin <bundle>/bin/codemode   (private TMPDIR, removed on exit; HUP/INT/TERM forwarded)
      investigate ─► codemode bridge ─► V8 worker (program: GitHub reads of --repo only)
                     proposal ─► plan.json ─► codemode plan check
      run|resume  ─► codemode plan run|resume --bridge (journal) ─► the four writes, probes
```

| Command | Options | Does |
| --- | --- | --- |
| `investigate` | `--program`, `--out`, `--trace`, `--timeout-ms` (60000), `--concurrency` (4), `--max-calls` (32) | Runs the program once with read tools, writes its proposal as a plan with comment markers, and checks it. |
| `declare` | | Prints the provider declaration to check and approve with. |
| `preflight` | `--plan` | Read-only: contract, authorization (nothing more is read for a refused scope), labels, preconditions, and whether each change already holds. |
| `run`, `resume` | `--declaration`, `--journal`, `--deadline-ms` | Serves one session of the approved plan. |

Every command requires `--account` (a connected-account id, never an alias or
word id) and `--repo` (the only repository it reads or writes), and takes
`--bin <codemode>`. stdout receives one JSON line. Exit status: 0 success,
1 no outcome or the program failed, 2 usage or startup error, 3 work remains or
not ready, 4 refused.

## Contracts

| Tool | Reconcile | Notes |
| --- | --- | --- |
| `GITHUB_ADD_LABELS_TO_AN_ISSUE` | state | One label, which must already exist. |
| `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE` | state | A 404 after dispatch stays unknown until probed. |
| `GITHUB_UPDATE_AN_ISSUE` | state | Only `state` and `state_reason`. |
| `GITHUB_CREATE_AN_ISSUE_COMMENT` | receipt | The body ends with `<!-- codemode:<plan id>:<operation id> -->`; only a comment ending with the approved marker, by the approved login, counts. |

All are `repeat: unsafe`, retention `summary/1`. Any operation may require
`{"issue_state": "open"|"closed"}`, checked by a read before each attempt (not
atomic with the write). Investigation reads are listed in `READ_TOOLS`
([`composio-provider.ts`](composio-provider.ts)); `COMPOSIO_*` tools, other
writes, `proxy`, and the sub-agent are unreachable. A `dryRun: true` result is
never success. Details and failure semantics are in the guide.

## Checks

```sh
mise run composio-demo           # offline walkthrough: investigate, check, approve, run, inspect
mise run composio-plan-check     # offline tests: tests/composio.rs, tests/composio_plan.rs
COMPOSIO_GITHUB_ACCOUNT=ca_... mise run composio-live-check              # live, read-only investigation of jkomyno/codemode
COMPOSIO_GITHUB_ACCOUNT=ca_... mise run composio-plan-preflight owner/repo /abs/plan.json  # live, read-only
```

The offline checks drive the real host through [`testing/host.ts`](testing/host.ts),
which adds `--offline <ledger.json>` (a stand-in GitHub with an independent
ledger, [`testing/fake-github.ts`](testing/fake-github.ts)) and
`--drop-reply <operation>` (withhold one observed success, to simulate a lost
reply). It is not installed. Live runs are recorded in the
[validation record](../../docs/composio-live-validation.md).

## Files

- [`host.ts`](host.ts): the entry for `composio run`; [`composio-host.ts`](composio-host.ts): its commands.
- [`github-plan.ts`](github-plan.ts): the contracts, identity checks, and plan handlers.
- [`composio-provider.ts`](composio-provider.ts): investigation reads, result mapping, and
  `sdkExecutor` for an `@composio/core` client (checked offline only; it needs a project API key).
- [`composio-tools.d.ts`](composio-tools.d.ts): what an investigation program sees.
- [`examples/triage.ts`](examples/triage.ts): the guide's investigation program.
- [`testing/`](testing): offline stand-ins, the test entry, and handler checks; not installed.
