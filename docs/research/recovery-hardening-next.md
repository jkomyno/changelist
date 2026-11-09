# Recovery hardening and a Composio plan provider

Research date: 2026-11-09. Inspected implementation: `8e3835f` on local `main`.
This is a source-backed execution brief for a subsequent `ce-work` invocation;
research does not establish that new behavior or live writes have been verified.

## Product and scope

Keep the product promise: review external changes, execute them, and know how to
continue after an interruption. Complete three bounded improvements: prove the
commit-before-dispatch ordering, control durable provider-data retention, and add
a conservative Composio CLI plan adapter for GitHub issue-label additions.

Keep the Rust library and CLI, existing scheduler, plan bridge, host permissions,
and precise uncertainty semantics. Do not add another scheduler, a workflow DSL,
a UI, a credential scanner, encryption infrastructure, or arbitrary guest replay.
Do not pursue structural file splitting or unrelated optimization in this work.
Specific presentation venues remain private.

## Source-backed findings

### Ordering

- [`JournalingProvider::execute`](../../src/executor.rs) awaits `journaled`
  around `record_intent` before it calls the trusted provider. It separately
  awaits the observation write before replying to the scheduler.
- [`journaled`](../../src/executor.rs) uses `spawn_blocking`; cancellation can
  drop the waiting future while the journal work continues. The existing queued
  write accounting must remain correct.
- [`Journal::write`](../../src/journal.rs) returns only after
  `transaction.commit()` succeeds.
- [`assert_intent_preceded_every_request`](../../tests/plan_cli.rs) compares
  provider-ledger keys against journal keys after execution stops. This proves
  membership, not temporal order: concurrent dispatch and intent writes could
  both finish before the assertion.

This is a test-oracle gap; source inspection did not establish an ordering bug.

### Retention

- [`bounded_observation`](../../src/journal.rs) retains successful values up to
  16 KiB and bounded failure messages. Truncation is not a confidentiality policy.
- `Journal::record_probe` also persists provider evidence and probe errors.
  `Journal::end_session` persists diagnostics and the dispatch trace.
- [`inspect_journal`](../../src/plan_inspect.rs) prints successful probe evidence
  by default; `--messages` controls other prose fields. A provider's evidence is
  documented as a short safe explanation, but is still a free-form string.
- The credential-looking-key guard in [`plan.rs`](../../src/plan.rs) checks
  supplied plan data. It does not sanitize provider replies or error messages.
- SQLite commits can leave data in the database and WAL; filtering after a write
  is too late. See [SQLite WAL](https://www.sqlite.org/wal.html#the_wal_file) and
  [secure-delete limitations](https://www.sqlite.org/pragma.html#pragma_secure_delete).

### Composio

See [the focused provider research](composio-plan-provider.md) for installed CLI
observations, source paths, current schemas, and official GitHub references.
The existing Composio adapter serves ordinary runs, not recoverable plan sessions.
The shared client already has `runPlanBridge`; reuse it.

The CLI helper has explicit account-selection capability that the existing
two-argument adapter type does not expose. Verify it against the installed CLI
before using it. The helper does not establish remote receipts, idempotency,
atomic preconditions, cancellation of a remote effect, or version pinning.

## Execution unit 1: prove commit-before-dispatch

Add the narrowest private test support needed to hold an intent transaction
before commit. Prefer test-only barriers and observable events over production
environment flags, a public storage abstraction, or a general fault framework.

Prove all of these against the actual journaling execution path:

1. While intent commit is held, the provider has not entered execution.
2. Releasing a successful commit permits provider entry, with the committed
   intent corresponding to that attempt.
3. A failed intent commit permits zero provider calls and returns the existing
   journal failure outcome.
4. Cancellation or deadline during the held write cannot cause dispatch after
   the write is released; preserve accounting for writes still running.

Use explicit synchronization and controlled polling/scheduling. A sleep followed
by a negative assertion is not sufficient. Use timeouts only to bound failure.
Demonstrate that a temporary mutation allowing dispatch concurrently with the
intent write makes the ordering test fail, then restore it. Preserve the existing
SIGKILL/provider-ledger tests as complementary evidence.

## Execution unit 2: provider-owned retention before persistence

Introduce a small explicit retention contract with a safe metadata-only default.
The trusted host/provider owns it; plans, guests, and arbitrary result fields
cannot select or widen it. Make the selected policy/version visible in inspection
and bound it to approval. A material policy change requires renewed approval.

Preserve the facts needed for recovery: operation/attempt identity, ordering,
outcome/effect classification, byte counts, probe verdict, and an approved bounded
explanation or evidence reference. Arbitrary raw provider values, error messages,
diagnostics, and evidence strings are excluded by default. If retaining selected
evidence is necessary, use an explicit bounded host projection with a clear
policy identifier; do not build a general redaction language or secret detector.

Enforce this before any excluded data reaches a durable write. For host adapters,
project replies in the host before sending them across the bridge as appropriate,
and enforce the approved retention contract in Rust before persistence. Audit
observations, probe replies/failures, authorization diagnostics, session ends,
embedded traces, inspection, CLI stdout/stderr, and adapter diagnostics. Retaining
one safe copy does not permit an unfiltered duplicate elsewhere.

Malformed or failed projections must not erase observed effect evidence. Preserve
the known effect with omitted payload metadata, or fail closed while keeping an
honest uncertainty record. Keep bounded machine-owned codes separate from raw
provider-controlled prose. `--messages` cannot resurrect excluded data.

Test synthetic distinct secret markers in successful results, error messages,
probe evidence/errors, and session diagnostics. Check reopened journal history,
raw database and WAL files when present, embedded traces, and public outputs.
Include bridge-host behavior and policy-change refusal. Prove retained metadata
still yields the same recovery decisions, and that an allowed safe summary remains
useful. Do not claim that task arguments or arbitrary private data are secret-free.

Define compatibility deliberately: keep existing evidence readable where safe,
or refuse older formats/policies with a useful diagnostic and a documented
upgrade path. Do not silently approve a new policy, rewrite old facts, or claim
old database/WAL contents were securely erased. Historical files may contain data
that the new default would exclude.

## Execution unit 3: conservative Composio CLI plan adapter

Add an optional host adapter under `integrations/composio`, using
`composio run` and the existing plan bridge. Keep credentials in the CLI host and
the Rust child environment empty. Keep the Rust core provider-independent.

Start with `GITHUB_ADD_LABELS_TO_AN_ISSUE`; use
`GITHUB_LIST_LABELS_FOR_AN_ISSUE` for state reconciliation. Revalidate slugs and
schemas against the actual installed version. Initially accept one already-existing
label per operation, with a positive issue number, explicit owner/repository, and
no unsupported argument keys. Refuse empty label arrays and implicit resource
setup. Confirm the label exists before a live mutation; do not describe this
preflight as an atomic guarantee against concurrent deletion. Allow only the
narrow supported operation family and explicitly reject unsupported atomic
preconditions before performing a mutation. A separate read does not enforce an
atomic precondition.

Bind approval and enforce execution against a specific connected account and
destination. Pass explicit account selection on every mutation and read. Validate
frozen owner/repository/issue arguments against the approved destination and host
allowlist; do not silently fall back to a default account. Check authorization
anew for each session and preserve CLI approval behavior. Prove account mismatch,
destination mismatch, and unsupported preconditions produce no mutation calls.

Use `repeat: unsafe`, `reconcile: state` for the initial live contract unless
stronger upstream guarantees are independently established for the entire
relevant effect. Label set membership alone does not establish the absence of
repeated webhook or automation effects.

- All desired labels observed on the issue: `satisfied`, attribution unknown.
- A desired label missing: `unknown`, never confirmed `not_applied`; an earlier
  request could still execute. No automatic resend of this uncertain mutation.
- Read failure, pagination ambiguity, transport loss, generic tool failure, or
  cancellation after dispatch: preserve uncertainty.
- Only the local adapter's proven pre-dispatch rejection can assert refusal.
  An observed remote error does not automatically prove no effect.
- Do not manufacture receipts, fences, idempotency keys, rollback, or remote
  cancellation from local attempt IDs or a process signal.

Check the helper/transport's retry behavior. Account for pagination, malformed
responses, and GitHub mutation pacing; do not turn the fixture's concurrency of
four into a live GitHub throughput benchmark. Distinguish observed tool schema
version from an execution version pin the helper cannot provide.

Provide a credential-free offline check/demo with an injectable executor and
independent call/effect ledger. Exercise lost replies after a label effect, state
satisfaction without attribution, absence with a late possible effect, failed
probes, exact account routing, scope refusal, cancellation, and no blind retry.
Use the actual Composio host/plan-bridge path with a killed Rust executor for the
crash/restart case, rather than substituting the fixture Rust provider. Keep fault
controls in the test/demo harness and never enable them implicitly for live runs.

## Live validation and authorization boundary

No live mutations are authorized by the research request. Do not choose an
existing repository or issue, create a repository, change labels, post comments,
or perform cleanup writes without a concrete user-authorized target and scope.

Complete all local units and read-only live preflight first. Prepare a reviewable
write proposal naming the connected account, repository, issue IDs, labels,
maximum calls, proposed before/after state, and restoration actions. Keep private
account identifiers and live payloads out of committed evidence. Do not replace
an uncertain write with a different transport or automatically retry it.

When that exact scope is authorized, perform a tiny serial mutation through the
adapter. Inject response loss in the local harness only after a successful reply
from the real provider has reached the host, then kill the Rust executor and
resume with fresh host processes. Clearly label the artificial response loss;
this establishes recovery from lost local observation, not from every possible
remote failure. Reconcile through live reads and independently inspect the issue
state. Any cleanup is a separately explicit, accounted-for external change and
must preserve unrelated labels and concurrent user work.

Without write authorization, report local completion and read-only verification,
with live mutation/recovery verification explicitly blocked by the missing target
authorization. Do not label the adapter live-write-verified.

## Verification and delivery

Follow repository instructions and `ce-work`'s mandatory workflow. Finish units in
the order above, using bounded workers where useful; the coordinator owns final
integration and verification. Use existing tests unless new behavior or the
specific oracle gap needs a new test. Avoid unrelated coverage expansion.

Run `mise run check`, `mise exec -- cargo +1.85.0 test --locked`, the new ordering
and retention checks, and the new offline Composio plan check. Run Pi plan checks,
OpenCode checks, existing Composio offline demo, and recovery demo because the
provider declaration/bridge or shared recovery behavior can change. Do not rerun
historical benchmark suites without a relevant change. If changed paths affect
the recovery timings, retain old results and add a clearly fingerprinted
measurement, with durability modes kept separate.

Complete `ce-code-review` and two independent Codex default-model reviews of
ordering, persistence/retention, and account/reconciliation boundaries, the second
after fixes. Record actual receipts and unavailable checks honestly. Resolve
findings necessary for the requested outcome; no unbounded review loop.

Update `docs/recovery.md`, `docs/integrations.md`, and the Composio README with
retention semantics, provider guarantees, and precise offline/read-only/live-write
status. Do not add unsupported speed or exactly-once claims.

When invoked to implement this brief, work on local `main`, preserve the existing
44 ahead-of-remote commits and unrelated files, and commit completed owned units,
including these supplied research artifacts. Refresh the actual branch/status;
the ahead count is only the research snapshot. Do not push or open a PR.

Final delivery: commit hashes, verification receipts, proof that the ordering
mutation was caught, retention guarantees and legacy limits, exact provider
contract, offline/read-only/live status, runnable commands, and any concrete live
authorization blocker.
