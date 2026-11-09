# Composio plan provider research

Research date: 2026-11-09. Repository baseline: `8e3835f`. This is an implementation
brief for the optional Composio adapter, not a claim that plan mode works today.
No external mutation was performed. No live target has been selected or authorized.

## Confirmed interface

The installed Composio CLI is **0.6.0**. Its `execute --help` exposes an account
selector; `run --help` documents the injected helper as `execute(slug, data?)`.
The implementation also accepts **`execute(slug, data, { account })`** and forwards
the selector as `--account`. A read-only probe of the installed binary with a
nonexistent selector was rejected and identified that selector. Therefore this
is observed behavior, despite the abbreviated help signature.

Source: local Composio checkout `1242e075b`,
`ts/packages/cli/src/services/run-helpers-runtime.ts:675-692`.
The existing codemode [`RunHelperExecute`](../../integrations/composio/composio-provider.ts)
only models two arguments. Plan mode needs to preserve the third argument.

The selector may be an account ID, alias, or word ID. The CLI resolves it to an
active account ID and binds that account into its session; an unmatched explicit
selector fails. Use an explicit **stable account ID** in the approved scope rather
than a mutable alias or implicit default. Validate the declared account and every
operation's account before execution and probes. Local source:
`ts/packages/cli/src/services/connected-account-selection.ts:200-290`,
`commands/tools/execute/tools.execute.context.ts:87-96`.

The helper has no version or AbortSignal option. Its backend request supplies
`tool_slug` and arguments, without a tool version. A schema version observed before
approval is not an execution-time version pin. Current source sends the execution
once, and the underlying Effect client defaults POST retries to zero unless an
explicit per-call retry budget is supplied. This does not establish the behavior
of all backend or provider layers. Sources: CLI
`services/tools-executor.ts:236-255`, `services/run-helpers-runtime.ts:561-635`;
local composio-client checkout `7d3c088`, `src/compat/request.ts:359-365`.
These source checkouts explain the design; their commits are not proven build
provenance for the installed binary.

A fresh live identity read through `composio run` succeeded; only success and the
presence of a login field were printed. A successful valid-account-pinned call and
interactive approval flow have not been checked in this research.

## Verified tool names and upstream semantics

CLI schema inspection returned both tools at version **`20261104_00`**:

| Tool | Required arguments | Use |
| --- | --- | --- |
| `GITHUB_ADD_LABELS_TO_AN_ISSUE` | `owner`, `repo`, `issue_number`, `labels` | Proposed mutation |
| `GITHUB_LIST_LABELS_FOR_AN_ISSUE` | `owner`, `repo`, `issue_number` | State reconciliation; paginate with `page`, `per_page` |

`GITHUB_LIST_LABELS_ON_AN_ISSUE` does not exist in this catalog. Do not copy the
shorter slug from CLI usage examples without checking it.

GitHub documents `POST /repos/{owner}/{repo}/issues/{issue_number}/labels` for
adding labels and the corresponding GET for listing them. Listing defaults to
30 labels, with a maximum page size of 100. Mutation requires Issues or Pull
Requests write permission. These docs provide no operation-key receipt/fencing
contract for this workflow. [GitHub label endpoints](https://docs.github.com/en/rest/issues/labels).

GitHub advises serial requests to avoid secondary limits and at least one second
between a large number of mutations. The live check should be a small, serial
correctness demonstration, not a parallel throughput benchmark.
[GitHub API best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api).

## Proposed recovery contract

Start with **add one already-existing label to one issue**, explicit repository,
issue and account scope, and **`repeat: unsafe`, `reconcile: state`**. No upstream
guarantee was found that repeating the request cannot trigger another observable
effect beyond label membership, such as integrations reacting to events. This
conservative declaration avoids depending on that unproven guarantee.

- A complete, successful state probe finding the requested label establishes
  `satisfied`, with attribution unknown. It is not a receipt proving our call ran.
- Absence, a failed/incomplete probe, or a malformed response leaves uncertainty.
  A previous request may still apply. Absence must never become `not_applied`.
- Do not automatically repeat an uncertain mutation. Pending operations can still
  dispatch; unresolved operations go to review after the configured probe budget.
- Generic `successful: false`, transport errors, cancellation and timeouts are
  effect `unknown`. Only a demonstrable local rejection before dispatch can be
  `refused`. Do not classify failures using message substring guesses.
- Reject nonempty unsupported preconditions before any mutation. A GET followed
  by a POST cannot enforce an atomic issue-state precondition. GitHub does not
  generally support conditional requests for unsafe methods unless an endpoint
  specifically documents them. [Conditional requests](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api#use-conditional-requests).

Bind `scope.destination` to canonical `owner/repo` and reject conflicting arguments.
Restrict the mutation allowlist to this operation family; a generic proxy, arbitrary
URLs, comment creation and repository setup are unnecessary for the first adapter.
Keep the Composio credential inside the authenticated host. Use the existing plan
bridge, host-owned account selection and permission flow; never bypass checks.

## Required offline evidence

Use an injectable stand-in for the three-argument helper. Check selected-account
forwarding on execution and every probe; destination mismatch; denied/revoked
authorization; absent or paginated labels; malformed success; reported failures;
late effects after a timeout; and cancellation without further dispatch.

Prove recovery with `runPlanBridge`, not only direct calls to the adapter. Drop a
response after the stand-in applies a label, kill the executor, and resume to
`satisfied` without a second mutation. Separately leave the old request pending,
return absent state, and prove resume requests review instead of issuing another
mutation. These cases deliberately differ from the fixture's atomic receipts and
fencing. Preserve their distinctions in docs and output.

Raw helper results/errors must not become recovery evidence by default. The host
needs only a small approved projection, such as whether the requested label is
present. Error text, issue bodies, account metadata and unrelated labels need not
enter the journal or trace. Test with secret-bearing stand-in outputs.

## Live verification and blockers

Complete the adapter, offline checks and read-only preflight before requesting
write approval. No project SDK API key is necessary; the logged-in CLI is usable.
The live write step needs a concrete approved account, repository, issue list,
existing label and cleanup plan. Write permission or ownership does not itself
authorize using an arbitrary repository for a demonstration. Do not create
repositories, issues or labels as implicit setup.

For the approved check, confirm identity and target access, execute a tiny plan,
discard an observed success at the host boundary, kill the executor, and resume.
Describe this accurately as host-side response loss after observed success, not
proof of a network-failure scenario. Re-read external state and report which
properties were live, simulated, or still untested. Keep journals and live
artifacts outside the tracked repository. Cleanup is itself an approved mutation
and must not remove pre-existing state or race an outstanding operation.

Recommendation: implement this conservative state-based adapter first. It proves
usefulness on a real provider while preserving the runtime's honest uncertainty.
Do not fabricate receipts, weaken account binding, or claim atomic preconditions
to reproduce the stronger offline fixture demonstration.
