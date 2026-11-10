---
name: codemode
last_updated: 2026-11-10
---

# codemode Strategy

Based on [GOAL.md](GOAL.md), whose original brief is kept for history. The
requirements and decisions behind the current direction are in the
[workflow plan](docs/plans/2026-11-09-2345-refactor-composio-github-workflow-plan.md).
Metrics below say what is measured and what is not yet.

## Purpose

A developer who lets an agent change several GitHub issues needs to review the
exact changes before they happen and, after a crash or a lost reply, know which
changes happened, which are uncertain, and what can safely happen next, without
repeating effects blindly and without handing credentials to agent-written code.

## Positioning

A small Rust execution tool for one workflow: investigate a GitHub problem
through the existing Composio CLI connection, produce a finite change list,
review and approve it exactly, execute it under host-owned limits, inspect it
live, and recover it conservatively after interruption. Rust owns admission,
limits, the journal, and recovery decisions; the Composio host owns the account,
the credential, and GitHub calls. Pi and OpenCode are secondary hosts that reach
the same executor.

## Users

**Primary:** Composio developers who already use the Composio CLI with a
connected GitHub account and want an agent to triage issues (labels, issue
state, comments) with a reviewable, recoverable record.

**Secondary:** Pi 1.4.0 and OpenCode 1.19.5 users who want agent-written
programs to call host tools under Rust-enforced limits through those hosts'
own permission gates.

## Boundaries

- One workflow, GitHub issue triage through `composio run`, with four write
  contracts: add a label, remove a label, set issue state, and create a marked
  comment. No new authentication, tool catalog, session platform, policy
  language, configuration framework, or general agent harness.
- One program engine: a fresh embedded V8 worker per run, killable by Rust.
  Its restrictions are not OS isolation; say what each limit bounds. The V8 heap
  budget is not a process-memory limit; only Linux adds a data-segment limit.
- Credentials stay in the host; guest code and the journal never see them.
- Recovery never replays a program. A plan is a finite list of independent
  operations; there is no workflow engine, dependency planner, UI, or
  persistent notebook state.
- Recovery semantics belong to the provider's declared contract. No
  exactly-once claim; distinguish applied, satisfied without attribution,
  not applied, refused, and unknown.
- Unreleased formats get no compatibility. Every command, flag, and dependency
  needs a named consumer.
- Each host/version pair needs its own validation before it is called
  compatible. Offline and live evidence are reported separately, and automated
  installation checks are not usability validation.

## Key metrics

- **Unjustified claims.** Per comparison scenario, runs that claimed success or
  non-effect without evidence, scored against an independent provider ledger.
- **Unsafe duplicate effects.** Per scenario, effects sent twice where the
  contract did not allow it.
- **Operations needing human review.** Per scenario, and whether each came with
  its evidence.
- **Approval binding.** Whether an edited or widened change list can run
  without a new approval.
- **Live inspectability.** Whether progress can be read from another process
  during a run, and whether an interrupted batch yields an exportable diagnosis
  naming each operation applied, not applied, or unknown.
- **Latency.** p50 and p95 of the workflow and of recovery, and recovery
  request counts.
- **Operator completion.** Whether another Composio developer completes the
  [operator trial](docs/operator-trial.md) from the guide alone. No one has run
  it yet.

The comparison against current Pi Durable, OpenCode Code Mode, and a competent
Composio CLI script is in [docs/comparison.md](docs/comparison.md).

## Tracks

### Composio GitHub workflow

Keep the installed path from investigation to diagnosis working from one guide,
with each write contract validated live before it is described as live.

_Why it serves the approach:_ It is the one path the project now serves; every
other surface must justify itself against it.

### Execution limits and evidence

Keep deadline, call allowance, concurrency, output, and memory limits enforced
outside the guest, with typed outcomes and an inspectable execution record.

_Why it serves the approach:_ Investigation programs are agent-written; their
failures must end with a named status and evidence, not a guess.

### Recoverable approved plans

Keep approval bound to the exact reviewed plan and its scope, intent durable
before provider entry, read-only inspection during a run, and reconciliation
limited to the provider's contract.

_Why it serves the approach:_ This is the answer to "what happened, and what is
safe now?" after an interrupted batch.

### Fair comparison

Compare the same workflow against pinned alternatives with the same provider
contracts, and report ties, losses, and unsupported cases.

_Why it serves the approach:_ Claims of advantage are only as good as the
evidence that the alternatives were run competently.

### Secondary hosts

Keep the Pi and OpenCode adapters working at their pins with their offline
checks; updating them to newer host versions is deferred.

_Why it serves the approach:_ They show the executor is not Composio-specific,
at a fixed and checked cost.
