# Pi integration

A Pi extension that runs agent-written TypeScript through `codemode bridge` and
executes its tool calls as Pi nested tool calls. Verified with Pi 1.4.0.

Program execution defaults to Deno. Select `--codemode-backend embedded-v8` to
use a binary built with `--features embedded-v8`; this selection does not look
up or require Deno. `--codemode-deno` applies only to the default Deno backend.
`rust_codemode_plan` uses native Rust execution and requires neither engine.

```text
Pi model ── rust_codemode {code} ──► extension ── start ──► codemode bridge ──► Deno guest
                                       ▲    │                  (Rust limits,       │
       ctx.executeTool(read, …) ◄──────┘    └── result ──►      scheduler, trace) ◄─┘ tools.call
       (Pi validation, tool_call gates, approvals, credentials)
```

## Run the offline example

From the repository root, with mise installed:

```sh
mise run pi-demo     # one scripted Pi session; prints the result, gate audit, and trace outcomes
mise run pi-check    # Pi boundary tests: abort through Pi RPC, and a plan run, crash, and resume
```

Both fetch Pi 1.4.0 through mise on first use and need no model credentials.
A [scripted model](demo/offline-host.ts) calls `rust_codemode` once with
[`demo/workflow.ts`](demo/workflow.ts). An independent [approval gate](demo/approval-gate.ts)
blocks `bash` (and `issues_add_comment`, used by the plan check) because no UI is available. Expected output:

```text
rust_codemode returned {"status":"success","calls":4,"output":{"entries":7,"environment":"denied","refused":["tool_error","tool_denied"],"title":"codemode Pi bridge fixture"}}
--- Pi approval gate audit
{"event":"tool_call","tool":"rust_codemode","nested":false,"decision":"allowed"}
{"event":"tool_call","tool":"read","nested":true,"decision":"allowed"}
{"event":"tool_call","tool":"ls","nested":true,"decision":"allowed"}
{"event":"tool_call","tool":"bash","nested":true,"decision":"blocked"}
--- call outcomes recorded by Rust
1 read succeeded
2 ls succeeded
3 bash provider_failed tool_error
4 write rejected tool_denied
```

`read` and `ls` ran concurrently through Pi. `bash` was authorized for codemode
but blocked by Pi's gate. `write` is active in Pi but was not listed in
`--codemode-tools`, so Rust rejected it and Pi never saw it. The guest could not
read environment variables.

## Use it in a Pi session

```sh
mise exec -- cargo build --release
pi --extension integrations/pi/codemode-bridge.ts \
   --codemode-bin target/release/codemode \
   --codemode-tools read,grep,find,ls
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--codemode-tools` | none | Pi tools guest programs may call, if also callable in the session |
| `--codemode-bin` | `codemode` on PATH | Rust executable |
| `--codemode-deno` | `deno` on PATH | Deno executable for the guest |
| `--codemode-timeout-ms` | 30000 | Run deadline, including time waiting for approvals |
| `--codemode-concurrency` | 4 | Maximum concurrent Pi tool calls |
| `--codemode-max-calls` | 32 | Maximum tool calls per run |
| `--codemode-trace` | unset | Write each run's full JSON report to this file |

Approval prompts that need a terminal UI count toward the deadline. Interactive
prompts were not exercised by the checks above. See
[host integration contract](../../docs/integrations.md) for the protocol and status.

## Run and recover an approved plan

`rust_codemode_plan {journal, action: "run" | "resume"}` runs or resumes an
operation plan approved with `codemode plan approve`. It starts
`codemode plan <action> --journal <file> --bridge` (bridge protocol version 3)
with an empty environment. Rust checks the approval, owns the journal, and
records each attempt's intent before Pi sees it; Pi executes every attempt with
`ctx.executeTool()`.

Plan execution submits operation identities natively inside Rust, with no Deno
lookup or guest process. `--codemode-deno` still selects the ordinary program
guest and has no effect on plans. The plan check uses a nonexistent Deno path.

```text
Pi model ── rust_codemode_plan ──► extension ── start {declaration} ──► codemode plan run --bridge
                                     ▲   │◄── authorize / call / reconcile ──┤  (approval check,
   ctx.executeTool(issues_add_label) ┘   └── authorized / result / reconciled ►   journal, recovery)
   (Pi validation, tool_call gates, credentials)   provider reconcilers (host code)
```

- **Declaration.** `--codemode-plan-declaration <file>` names the provider
  declaration: provider, account, and a recovery contract per Pi tool. It is
  operator configuration, never model input. Approve plans with the same file
  (`codemode plan approve --provider <file>`); a session whose declaration does
  not match the approval is refused.
- **Gates.** Plan approval supplements Pi's gates and never bypasses them: each
  attempt passes Pi's argument validation and every `tool_call` handler. Only tools
  in the declaration that are callable in the session are reached; `rust_codemode`,
  `rust_codemode_plan`, `codemode`, and `tool_search` never are. An undeclared or
  uncallable tool is answered `not_applied` without reaching Pi.
- **Authorization and scope.** Before a session sends anything, each declared
  tool's provider confirms that its live account may act on every approved scope
  (`authorize`; the fixture tools run `codemode-fixture authorize`, which also
  refuses a revoked account). Every attempt and probe carries the operation's
  approved scope. The adapter passes it to the tool, which must refuse a scope
  that is not the account and destination it acts as, and the fixture's
  reconciler forwards it so a probe never reads state or fences keys elsewhere: a host whose configuration
  changed since approval cannot act under that approval. Pi's gates still decide
  each call when it is dispatched.
- **Failures.** Pi reports gate blocks, validation errors, and thrown errors alike
  (`isError`), so the adapter cannot tell a blocked call from one that failed after
  an effect. Every Pi error becomes effect `unknown`: an operation with a
  reconciliation method is probed on resume; one without (`issues_add_comment`
  here) is left for a person (`codemode plan review`) and never repeated.
- **Providers.** Provider extensions listen on `codemode:plan-providers`. When a
  plan session starts, the adapter emits a registry, and each listener
  synchronously calls `registry.register(tool, { keyArgument, preconditionsArgument,
  scopeArgument, authorize, reconcile })`, so load order does not matter. The adapter
  adds the attempt key, the preconditions, and the approved scope under those
  argument names, and refuses an attempt whose tool registered no scope argument. It refuses
  (effect `refused`, before Pi runs anything) an attempt whose tool reconciles by
  receipt but registered no key argument, or whose operation has preconditions the
  tool cannot receive. A probe for a tool without a reconciler fails with
  `no_reconciler`, and the operation stays unknown.

The model sees `{session, status, counts, review}`; the full outcome is in the
tool result's `details`.

### What the plan check establishes

[`tests/pi_plan.rs`](../../tests/pi_plan.rs), run by `mise run pi-check`, uses Pi
1.4.0 with a [scripted model](demo/plan-host.ts), the
[fixture tools](demo/fixture-tools.ts) (`issues_add_label`, `issues_remove_label`,
`issues_add_comment`, each running `codemode-fixture call` with its attempt key;
that offline tracker is a development-only binary and is not installed),
and the [approval gate](demo/approval-gate.ts). It approves five operations with
concurrency 1 and a lost response on `label-103`, then:

1. runs the plan through Pi, waits until the fixture ledger shows `label-103`'s
   effect, and kills Pi's whole process group (Pi, bridge, guest, and the hung
   fixture call);
2. finds `label-101` applied, `comment-102` blocked by Pi's gate yet recorded
   `unknown` with next step `review` (the fixture never received it),
   `label-103` `unknown` with its intent recorded and no observation, and the last
   two operations pending;
3. resumes through Pi: the fixture reconciler registered by the extension finds
   `label-103`'s receipt (`applied`, basis `reconciled`), the pending operations
   run, `comment-102` still needs review, and the session ends `needs_review`;
4. checks the fixture ledger: zero duplicate effects, exactly one request for
   `label-103`, and no request for the comment;
5. checks Pi's gate audit: every journaled attempt has one nested `tool_call`
   entry carrying its attempt key, allowed except the comment; and a secret in Pi's
   environment appears in neither the journal, the session's trace, nor the outcome.

It does not establish: other Pi versions; a live model; interactive approval
prompts (`ctx.ui.confirm`), since the gate blocks without a UI; tools other than
the fixture's; Pi distinguishing a gate block from a tool failure (it does not, so
blocked operations need review); durability beyond a killed process; or plan
mode on any other host.
