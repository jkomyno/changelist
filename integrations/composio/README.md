# Composio provider

An optional host adapter that executes guest tool calls as Composio tools. The
Rust core stays provider-independent: `codemode bridge` admits, schedules, and
records every call, and this adapter runs each one in the trusted host process
that holds the Composio credential. Verified with Composio CLI 0.6.0.

```text
composio run (Bun, holds the CLI credential)
  host.ts ── start ──► codemode bridge (empty env) ──► Deno guest
     ▲                    Rust limits, scheduler, trace       │ tools.call("SLACK_TEST_AUTH", {})
     └── execute(slug, args) ◄── call ──┘ ◄──────────────────────┘
```

## Run it

```sh
mise run composio-demo        # offline: recorded fake results, no account or network
mise run composio-live-check  # live, read-only: needs `composio login` with GitHub and Slack connected
```

Both run [`demo/workflow.ts`](demo/workflow.ts), which issues two identity reads in
parallel, one call with missing arguments, and one slug the host did not authorize.
Expected live output (timings vary):

```text
{"status":"success","calls":4,"output":{"environment":"denied","github":true,"refused":["composio_tool_failed","tool_denied"],"slack":true},...}
--- call outcomes recorded by Rust
1 GITHUB_GET_THE_AUTHENTICATED_USER succeeded 21..2145ms
2 SLACK_TEST_AUTH succeeded 21..1874ms
3 GITHUB_GET_A_REPOSITORY provider_failed composio_tool_failed 2146..3193ms
4 TELEGRAM_GET_ME rejected tool_denied -..3195ms
--- credential containment (values are never printed)
trace.json: credential absent
stderr: credential absent
```

The missing-argument call fails the CLI's input validation, so the tool is not
executed. `TELEGRAM_GET_ME` never reaches Composio: Rust refuses it.

To run your own program:

```sh
composio run --file integrations/composio/host.ts -- \
  --program my-program.ts --tools SLUG_A,SLUG_B \
  --bin target/debug/codemode --deno "$(mise which deno)" --trace "${TMPDIR:-/tmp}/trace.json"
```

Options: `--timeout-ms` (30000), `--concurrency` (4), `--max-calls` (16); unknown
options and missing values are errors. A live trace contains account data, so keep
it outside the repository. Pass
absolute or relative paths for `--bin` and `--deno`; the bridge receives an empty
environment. `composio run` copies the script next to itself while it runs, so
`integrations/composio/` must be writable.

## Behavior

| Composio observation | Rust call outcome | Guest sees |
| --- | --- | --- |
| `successful: true` | `succeeded` | the tool's `data` |
| `successful: false` (tool error, invalid input, no active connection) | `provider_failed`, `composio_tool_failed` | a catchable error with Composio's message |
| SDK error raised before execution (validation, missing tool or account, rate limit) | `provider_failed`, `composio_sdk_refused` | a catchable error |
| CLI ended without a result, or any other thrown error | `unknown`; the host cancels the run | nothing; the summary adds `host_diagnostic` |
| non-object arguments | `provider_failed`, `invalid_arguments` | a catchable error; Composio is not called |
| slug not in `--tools` | `rejected`, `tool_denied` | a catchable error; the host is not called |
| cancelled or past the deadline while running | `unknown` | nothing; the run stops |

- **Credentials.** Under `composio run`, the CLI injects an authenticated `execute`
  helper into the host script and keeps its key in that process. The bridge is
  spawned with an empty environment, and the guest's Deno permissions deny
  environment, file, network, and process access.
- **Authorization.** `--tools` is the complete list of callable slugs. There is no
  wildcard, and the guest cannot widen it.
- **Cancellation.** The CLI helper cannot abort an in-flight execution. On SIGINT or
  SIGTERM (to the host or, as `composio run` forwards it, to its process group) or at
  the deadline, Rust records running calls as `unknown`, the host prints its summary
  and exits, and the remote effect may still happen.
- **Unobserved executions.** The bridge protocol lets a host report a value or an
  error for one call, not an unknown outcome. When the CLI ends without a result (a
  crash, a lost response, a signal), the tool may still have run, so the adapter
  leaves the call unanswered and cancels the run. Rust records it as `unknown`
  rather than a failure the guest could retry.
- **Results.** A value larger than the run's output budget ends the run with
  `output_limit`; a reply frame over 1 MiB is sent as `too_large`. The adapter does
  not summarize or offload large results. No call is retried.
- **Approvals.** When an organization enables the CLI's permission controls, each
  execution waits for the CLI's own approval flow, inside the run deadline. This was
  not exercised: the checked account has them off.

## Plan sessions: GitHub issue labels

[`plan-host.ts`](plan-host.ts) makes the Composio CLI the trusted provider of a
recoverable [plan](../../docs/recovery.md): Rust journals each attempt, and the
host adds one existing label to one GitHub issue per operation through
`execute(slug, data, { account })`. Write a host-owned config with the stable
connected-account id (`composio connections list`) and the allowed repositories:

```json
{"account": "<connected account id>", "destinations": ["owner/repo"]}
```

```sh
composio run --file integrations/composio/plan-host.ts -- declare --config config.json > declaration.json
codemode plan check plan.json --provider declaration.json
codemode plan approve plan.json --journal j.db --provider declaration.json --digest <sha256:...> --approver <name>
composio run --file integrations/composio/plan-host.ts -- preflight --config config.json --plan plan.json  # read-only
composio run --file integrations/composio/plan-host.ts -- run --config config.json --journal j.db \
  --bin target/debug/codemode --deno "$(mise which deno)"
composio run --file integrations/composio/plan-host.ts -- resume ...   # after an interruption
```

Each operation is `{"tool": "GITHUB_ADD_LABELS_TO_AN_ISSUE", "scope": {"provider":
"composio.github", "account": "<id>", "destination": "owner/repo"}, "arguments":
{"owner", "repo", "issue_number", "labels": ["one-label"]}}` with no preconditions.

| Observation | Recorded effect | After an interruption |
| --- | --- | --- |
| Scope, account, destination, arguments, or a precondition not supported | `refused`, nothing called | review |
| Label does not exist in the repository | `refused`, no mutation | review |
| Label existence could not be confirmed | `not_applied`, no mutation | may be sent again |
| Mutation succeeded | applied | done |
| Mutation failed, or the CLI ended without a result | `unknown` | probe the issue's labels |
| Probe: label on the issue | `satisfied`, attribution unknown | done |
| Probe: label absent | `unknown` (an earlier request may still apply) | review after the probe budget; never resent |
| Probe failed, malformed, or over 1,000 labels | probe error, still `unknown` | as above |

The declaration is `repeat: unsafe`, `reconcile: state`, and retention
`summary/1`: the journal keeps this host's fixed messages and evidence, never
Composio's results or error text. There are no receipts, idempotency keys,
atomic preconditions, or remote cancellation: the label check before a mutation
is a separate read, and a cancelled session cannot stop a mutation the CLI
already started. Label membership says nothing about repeated webhooks or
automations; that is why the contract is `repeat: unsafe`. The tool version
observed when approving is not an execution pin: the helper sends none.

`--drop-reply <operation>` is fault injection for recovery demonstrations: after
the helper reports a successful mutation, the host withholds the reply. It is
off unless given, and it simulates a lost local observation, not a network
failure.

Verification status:

- **Offline:** `mise run composio-plan-check` runs the handlers against a
  stand-in helper with its own ledger, then the real bridge: a dropped reply
  after the label was applied, a pending request, and a SIGKILLed executor
  resume to `satisfied` and `review` without a second mutation; a late effect is
  then reconciled; cancellation leaves the mutation `unknown`; scope and
  precondition refusals send nothing; every call used the configured account;
  no stand-in text reaches the journal or inspection.
- **Live, read-only:** `mise run composio-plan-preflight owner/repo 1 label`
  checks authorization, the label, and the issue's labels with the logged-in
  CLI (verified on a public repository; a wrong account selector fails closed).
- **Live writes:** not yet run. They need an explicitly authorized account,
  repository, issue, label, and cleanup plan.

## Using the SDK instead

[`composio-provider.ts`](composio-provider.ts) also exports `sdkExecutor`, which
adapts an `@composio/core` client (`composio.tools.execute(slug, { userId, arguments,
version })`) and forwards each call's abort signal. Construct the client in your
host with a project API key, pin tool versions, and pass the executor to
`runComposio`. SDK errors raised before execution become `composio_sdk_refused`; any other thrown
error, such as a network failure after the request may have been sent, is handled as
unobserved. This path is checked offline against a stand-in client only (see
`tests/composio.rs`); it has not been run against the live API, because the checked
machine has only a CLI user key, which `@composio/core` 0.28.0 does not accept.

## Files

- [`composio-provider.ts`](composio-provider.ts): result mapping, the CLI helper and
  SDK executors, and `runComposio`, built on the shared [bridge client](../shared/bridge-client.ts).
- [`host.ts`](host.ts): the host script for `composio run`, or `--offline` under Deno or Node.
- [`composio-plan.ts`](composio-plan.ts) and [`plan-host.ts`](plan-host.ts): the plan
  provider for GitHub issue labels, and its host script; [`run-plan-preflight.sh`](run-plan-preflight.sh)
  runs the live read-only preflight.
- [`composio-tools.d.ts`](composio-tools.d.ts): guest declarations for the example slugs.
- [`demo/`](demo): the example program, the offline fake helpers (`fake-labels.ts` for
  plans), the SDK and plan-handler checks, and the credential check.
