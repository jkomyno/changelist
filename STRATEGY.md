---
name: codemode
last_updated: 2026-11-07
---

# codemode Strategy

Based on [GOAL.md](GOAL.md). This document captures product direction; the
implementation brief remains in GOAL.md. Success metrics below are proposals
for review, with measurement still to be established.

## Purpose

Developers building tool-using AI agents need to execute composed workflows while
controlling access, resource use, and partial failures. They need to understand
which actions ran and which outcomes remain unknown when generated code fails
or stops, without exposing provider credentials to that code.

## Positioning

Build a small, provider-independent Rust execution core and CLI that Pi,
OpenCode, Codex, and the Composio CLI can use through verified host integrations.
Rust owns run-level control, rolling bounded dispatch, and typed execution
evidence; a replaceable backend executes TypeScript/JavaScript and adapters own
authentication and external tool calls.

## Users

**Primary:** Developers building and evaluating tool-using AI agents — they use
codemode to execute agent-written tool workflows locally, control their access,
and inspect the resulting evidence from their existing agent or CLI.

## Boundaries

- Keep v0 small and shippable: defer microVMs, persistent Jupyter-style state,
  a web UI, and additional execution languages.
- Prefer simple, reliable isolation for v0; describe its actual guarantees and
  limitations without claiming perfect security.
- Keep secrets and credentials outside the sandbox, including when using a real
  tool provider.
- Keep Composio optional; the core must run with a mock provider and support
  other providers through the same contract.
- Keep the design explainable in a 30-minute technical presentation around
  run-level control, scheduling, and execution evidence; defer durable recovery,
  live replay, general dependency planning, and catalog or billing infrastructure.
- Treat shell invocation and native-tool integration as separate claims. Each
  target host needs its own versioned validation before compatibility is asserted.

## Key metrics

Proposed measures; no baselines, targets, or measurement infrastructure are
established yet. Compare workflows against a sequential-tool-call baseline to
test the code-mode bet.

- **Workflow success rate** — share of representative tasks that achieve their
  expected outcome; measure from task assertions and run traces in repeatable
  evaluations.
- **Time to successful outcome** — elapsed time for successful runs on those
  tasks, including tool execution; measure from traces for both approaches,
  alongside success rate.
- **Credential containment failures** — runs in boundary evaluations where
  sandboxed code can access provider credentials; measure through controlled
  access attempts, recording verdicts without credential values.
- **Trace completeness rate** — share of evaluation runs whose JSON captures
  executed code, every tool call's name, arguments, result summary, timing and
  error, and the final outcome; validate traces against observed execution,
  including failed runs.
- **Verified host compatibility** — target host/version pairs that pass launch,
  tool-boundary, output, and cancellation checks; record in the integration guide,
  distinguishing shell invocation from native-tool callbacks.

## Tracks

### Run-level control and scheduling

Invest in one Rust-owned deadline, admission and concurrency contract around
restricted TypeScript/JavaScript execution, including bounded output and cleanup.

_Why it serves the approach:_ Generated loops and fan-out need limits enforced by
the host, with scheduling and failure behavior that can be demonstrated and tested.

### Helper contract and provider integration

Invest in a small, typed helper surface backed by interchangeable tool providers,
with credentials handled outside the guest and host-specific registration kept
in adapters.

_Why it serves the approach:_ One core can serve Pi, OpenCode, Codex, and the
Composio CLI while a mock provider keeps evaluation independent of accounts.

### Execution evidence and evaluation

Invest in machine-readable traces that connect code, tool calls, errors, timing,
and outcomes and remain useful to external evaluation and debugging tools.

_Why it serves the approach:_ Traces provide the evidence needed to investigate
failures and test whether code-mode workflows improve agent behavior.

### Developer adoption and extensibility

Invest in a small Rust library/CLI, minimal TypeScript types, a common subprocess
contract, and one understandable example that demonstrates useful composition
and a controlled failure.

_Why it serves the approach:_ A compact, understandable prototype lets developers
try the workflow and extend it without first adopting a larger platform.
