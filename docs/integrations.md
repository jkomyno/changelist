# Host integration contract

The primary host is the **Composio CLI**, which runs the
[GitHub triage workflow](composio-github.md). **Pi 1.4.0** and **OpenCode 1.19.5**
are secondary hosts that reach the same executor through their own adapters,
pinned to the versions their offline checks use. Codex is not integrated. The
Rust library and CLI share one execution contract; host registration and tool
access are adapter responsibilities.

## MCP servers (built in)

`codemode mcp` needs no host adapter. The binary holds an MCP client for
Streamable HTTP and stdio, lists a server's tools at run time, and uses the
server as the provider for investigation and for approved plans. It has no
reconciliation: MCP gives a client no way to ask whether an earlier call took
effect. See [the MCP guide](mcp.md) for tool classes, what an approval binds,
and what is and is not verified.

## Native tool callbacks

The binary has no built-in tools. A host adapter supplies selected registered
tools to the runtime and keeps the host's authorization, approvals,
cancellation, and result semantics. A child process does not inherit its
parent's tool registry, so this needs `codemode bridge` (or, for an approved
plan, `codemode plan run|resume --bridge`) plus a host-specific adapter.

## Status

| Host | Verified | Remaining target |
| --- | --- | --- |
| Composio CLI 0.6.0 (primary) | GitHub triage host through [`integrations/composio`](../integrations/composio/README.md): `composio run` hosts an investigation program over the bridge and serves approved plans over the plan bridge, with the CLI's authenticated `execute` helper and one pinned connected account. **Live, read-only (2026-11-10):** the installed launcher investigated `jkomyno/codemode` with the example program (the repository had no open issues, so it proposed nothing), and a placeholder four-operation proposal was written and checked as a plan; nothing was approved or run. **Offline:** read restrictions, signed-URL stripping, unobserved reads (`unknown`), the four write contracts, lost replies, killed executors, late effects, dry-run results, and identity drift, against a stand-in GitHub with its own ledger. **Earlier live write (2026-11-09):** one approved label addition with a lost reply, SIGKILL, and a resume to `satisfied`, with an earlier host. See [live validation](composio-live-validation.md). | Live writes through the current host are not yet validated: the four contracts are prepared but not run. The CLI's permission-control flow (off for the checked account); other CLI versions. A live SDK run needs a project API key. |
| Pi 1.4.0 (secondary) | Native callbacks through [`integrations/pi`](../integrations/pi/README.md): nested calls, Pi permission gates, Rust-side authorization, cancellation, and environment containment. Checked offline with a scripted model. **Plan mode** (`rust_codemode_plan`, protocol 3): an approved plan runs with every attempt passing Pi's gates through `ctx.executeTool()`; after Pi is killed with SIGKILL following a lost response, resume reconciles through a host-registered reconciler with no duplicate effect; a gate-blocked operation stays `unknown` and needs review. Fixture tools and a scripted model only. | A live model session; interactive approval prompts (`ctx.ui.confirm`) were not exercised; other Pi versions. |
| OpenCode 1.19.5 (secondary) | Plugin tool through [`integrations/opencode`](../integrations/opencode/README.md): each guest call passes OpenCode's permission service (`allow`, `deny`, `ask`; approvals granted over OpenCode's HTTP API and refused by `opencode run`, handled as a native rejection), Rust-side authorization, session abort (including during an approval wait), and environment containment. Checked offline with a scripted model. | Plan mode (recoverable plans) not checked. **Blocker:** plugins cannot call OpenCode's registered tools (built-in, plugin, or MCP), and nested calls do not reach other plugins' `tool.execute` hooks; guest calls reach only host tools passed to the adapter. A live model session; the TUI approval dialog; other OpenCode versions. |
| Any MCP server (built in) | [`codemode mcp`](mcp.md), revisions 2026-07-28 and 2025-11-25 to 2025-03-26. **Offline:** both eras and both transports against a stand-in server with its own ledger: tool classes with and without trusted annotations, pagination, SSE responses, `Mcp-Param-*` headers, header credentials, a program that cannot write, lost replies, tool and server errors, a deadline with a late effect, stdio cancellation, and refusal after a changed server, trust setting, or tool definition. **Live, read-only (2026-11-11):** `mcp tools` listed two public 2026-07-28 servers. | No live write. Composio Tool Router is not run: its protocol revision, its annotations, and the stability of its tool list are unknown. OAuth flows, elicitation, sampling, roots, resources, and tasks are not supported. |
| Codex | Not integrated. | Deferred. Native callbacks need a demonstrated interface for calling Codex tools back; MCP exposes our tool to Codex, not Codex's tools to us. |

Validate launch, framing, tool scope, failure states, approval behavior, and
cancellation for each host/version before marking it compatible. Do not install
configuration or expose additional tools implicitly.

Programs run in a fresh V8 worker process started from the `codemode`
executable, with an empty environment. Approved plans submit operation
identities natively and start no worker. Neither changes a host's tool access or
permission gates. The Pi and OpenCode checks run the same embedded worker as
the Composio host; adapters do not choose an engine.

## Bridge protocol (version 2)

`codemode bridge [limit options]` serves one run over stdin and stdout. The
limit options are `--timeout-ms`, `--max-calls`, `--concurrency`,
`--max-output-bytes`, `--heap-mb`, and, on Linux only, `--process-memory-mb`
(see `codemode --help`). The worker starts after the start frame, and the run's
deadline, which includes worker startup, starts with that frame. Frames are
LF-terminated JSON objects. Host frames are at most 1 MiB; the report frame is
bounded by the run limits (64 KiB source, 64 KiB arguments per call, and the
output budget). The library entry point is `codemode::serve_bridge`, which
accepts any async reader and writer. Other start versions are refused.

| Direction | Frame | Meaning |
| --- | --- | --- |
| host → bridge | `{"type":"start","version":2,"code":"...","tools":["read"]}` | Must be first. `tools` is the host's authorization list. |
| bridge → host | `{"type":"call","id":1,"tool":"read","arguments":{}}` | Rust admitted and dispatched this call; `id` is the trace identity. |
| host → bridge | `{"type":"result","id":1,"ok":true,"value":...}` | Observed success. Use `"ok":false,"error":{"code","message"}` for failure. |
| host → bridge | `{"type":"result","id":1,"ok":true,"too_large":true,"bytes":2097152}` | Observed value (or, with `"ok":false`, error) that cannot fit in a frame; recorded as `result_too_large`, ending the run with `output_limit`. The optional `bytes` is the host-measured size; without it the size is recorded as unknown. |
| host → bridge | `{"type":"fact","id":1,"kind":"executor_started","host_ms":12.5}` | A milestone for a running call: `received`, `permission_requested`, `permission_granted`, `permission_denied`, or `executor_started`. `host_ms` is optional and uses the host's clock. Send it before the call's result. |
| host → bridge | `{"type":"cancel"}` | Cancel the run. Closing the host's output does the same. |
| bridge → host | `{"type":"report","report":{...}}` | The execution record; the bridge then exits with the run's exit code. |
| bridge → host | `{"type":"error","message":"..."}` | The run never started, for example an invalid start frame. |

Limits, the rolling scheduler, and call identities are enforced on the Rust side, and calls outside `tools` are rejected
before the host sees them. The host never receives guest correlation tokens.
Results for unknown or finished calls are ignored, because they can race with a
deadline. A malformed host frame ends the run with `protocol_error`. Dispatched
calls without an observed reply become `unknown`. The host should abort its
outstanding calls when it reads the report.

Facts are host claims, not Rust observations. The report keeps them in
`provider_facts`, stamped with the Rust time each arrived and the host's own
`host_ms`, which is never compared with Rust times. Rust keeps at most 8 facts per
call, ignores facts for calls that are not running, and counts what it drops. A
missing fact is not evidence that a call did not run: the host may not report
facts at all, or may have run the tool before its acknowledgment arrived. The
shared bridge client sends `received` for every call frame; adapters report the
rest:

| Adapter | Facts |
| --- | --- |
| OpenCode | `permission_requested`, then `permission_granted` or `permission_denied` around `context.ask`, and `executor_started` before the host tool runs. |
| Pi | Only `received`. Pi's permission and approval handlers run inside `ctx.executeTool`, so the adapter cannot tell when the tool itself starts, and Pi permission waits are not separated. |
| Composio | Only `received`. The CLI helper and the SDK can both refuse a call before the tool runs (local input validation, a missing account, a rate limit), so the adapter cannot tell when the tool itself starts. |

The bridge does not authenticate the host: whoever owns its stdio is the trusted
provider. Limits come from command-line options chosen by the host, and
credentials stay in the host process.

## Plan bridge (version 3)

`codemode plan run|resume --journal <file> --bridge` serves one
session of an approved [plan](recovery.md) over stdin and stdout. The host is the
trusted provider: it declares the recovery contracts, executes each attempt
through its own permission gates, and answers reconciliation probes. Rust checks
the approval against that declaration, owns the journal, and decides what may
happen next. The library entry point is `codemode::serve_plan_bridge`.
No guest process is used in plan mode.

| Direction | Frame | Meaning |
| --- | --- | --- |
| host → bridge | `{"type":"start","version":3,"declaration":{...}}` | Must be first. The host's provider declaration, with an optional `"retention"` of `"metadata/1"` (default) or `"summary/1"`; the session is refused unless the approval binds exactly it. |
| bridge → host | `{"type":"authorize","id":1,"scope":{...}}` | Is this scope authorized now? Answer `{"type":"authorized","id":1,"ok":true}`, or `"ok":false` with `error`. |
| bridge → host | `{"type":"call","id":2,"tool":"...","arguments":{...},"operation":"...","attempt":"...","preconditions":[...],"scope":{...}}` | One attempt, sent only after its intent is durable. The host must refuse a `scope` it does not act as. Answer with a version 2 `result` frame, plus an optional `"effect":"refused"\|"not_applied"\|"unknown"` on failure; without it the effect is `unknown`. |
| bridge → host | `{"type":"reconcile","id":3,"operation":"...","tool":"...","arguments":{...},"keys":[...],"scope":{...}}` | Establish an uncertain outcome. Answer `{"type":"reconciled","id":3,"verdict":"...","evidence":"..."}`, or with `error`. |
| host → bridge | `{"type":"cancel"}` | Cancel the session; closing the input does too. Any other host frame, including a program-bridge `fact`, is malformed and ends the session. |
| bridge → host | `{"type":"outcome","outcome":{...}}` | Last. A refused session's outcome is `{"session","status":"refused","detail"}`; `diagnostic` is present when host input ended the session. Exit codes match `codemode plan run`. |
| bridge → host | `{"type":"error","message":"..."}` | No session ran, for example an invalid start frame or an unavailable journal. |

The shared client's `runPlanBridge` spawns the bridge with an empty environment.
The Pi adapter's `rust_codemode_plan` dispatches each attempt through
`ctx.executeTool()`, so plan approval supplements Pi's gates and never bypasses
them; Pi errors are effect `unknown`, because Pi reports gate blocks and tool
failures alike. Provider extensions register attempt-key, precondition, and scope
arguments, an authorization check, and reconcilers on `codemode:plan-providers`;
an attempt whose tool cannot receive its approved scope is refused. The Pi and
Composio plan flows have been checked; OpenCode's has not. See
[`integrations/pi`](../integrations/pi/README.md) and the
[Composio guide](composio-github.md) for exactly what each check establishes.

## Host adapters

The adapters use [`integrations/shared/bridge-client.ts`](../integrations/shared/bridge-client.ts),
a host-neutral client built on Node built-ins. It spawns the bridge with an empty
environment, frames calls and results, refuses calls that arrive after an abort,
replaces oversized replies with `too_large`, sends `cancel` on abort, and kills the
bridge two seconds later if it has not exited. `runBridge` takes the bridge's
limits, including `heapMb` and the Linux-only `processMemoryMb`; the Pi and
OpenCode adapters pass only the deadline, concurrency, and call allowance, so
their runs use the default output and memory budgets. An adapter supplies the authorized
tool names and one function that runs a call; host registration, permissions, and
result conversion stay in the adapter.

## Pi adapter

The [Pi extension](../integrations/pi/codemode-bridge.ts) registers a
`rust_codemode` tool with `model-only` exposure, so neither guest programs nor
Pi's own `codemode` can call it. For each run it:

1. authorizes the tools named in `--codemode-tools` that are callable in the
   current session (`ctx.tools`), excluding orchestrators;
2. starts `codemode bridge` with an empty environment and explicit executable paths;
3. runs each call with `ctx.executeTool()`, which applies Pi's argument validation
   and every extension's `tool_call` and `tool_result` handlers;
4. on Pi abort, sends `cancel`, aborts nested calls, and escalates to SIGKILL
   after two seconds if the bridge has not exited;
5. returns compact status and output to the model, keeps the full report in the
   tool result `details`, and optionally writes it to `--codemode-trace`.

Pi reports blocked, invalid, unknown, and thrown calls as `isError` results; the
adapter maps them to `tool_error` with Pi's message. A result or error whose frame
would exceed 1 MiB is sent as `too_large`, so Rust stops the run exactly as for any
over-budget result. A call frame that arrives after Pi aborted is refused without
running.
Structured results use `structuredContent`; others use their text, with
unstructured image blocks replaced by a placeholder.

The run deadline (`--codemode-timeout-ms`, default 30 s) includes time spent
waiting for approvals. Pi also keeps its own bounded `nestedCalls` record on the
calling tool's result; the Rust report remains the authoritative call trace.

## OpenCode adapter

The [OpenCode plugin](../integrations/opencode/codemode-bridge.ts) registers a
`rust_codemode` tool through the plugin `tool` hook. For each run it:

1. authorizes exactly the host tools passed to `codemode({ tools })`;
2. starts `codemode bridge` with an empty environment and explicit executable paths;
3. for each call, awaits `context.ask({ permission: "codemode_<tool>", patterns: ["*"], always: ["*"] })`,
   so OpenCode's agent and session rules and approval replies decide, then runs the
   host tool with a per-call abort signal;
4. on OpenCode abort (`context.abort`), sends `cancel`, aborts nested calls, and
   escalates to SIGKILL after two seconds;
5. returns compact status and output to the model, raising a tool error when the
   run did not succeed, and optionally writes the full report to `trace`.

A rule denial is a `permission_denied` call error with OpenCode's message, and the
program may continue. A rejected approval records `permission_rejected` and
refuses every later call; when the program ends the adapter rethrows it, so
OpenCode handles it as a rejected native call and, by default, stops the turn.
An abort or deadline while waiting for approval leaves the call `unknown`;
OpenCode keeps that approval request pending, and approving it later
does not run the tool. The `codemode_` prefix keeps host tools apart from
OpenCode's built-in permission keys. "Always" approves later calls of that host
tool for the OpenCode project while that instance runs, because OpenCode keeps
approvals in memory per project directory.

OpenCode 1.19.5 offers no plugin API that executes its registered tools: the plugin
`ToolContext` has `ask`, `abort`, and `metadata` but no tool executor, and the HTTP
API lists tools without running them. OpenCode's own `execute` code mode calls MCP
clients through internal services. Reusing OpenCode's built-in, plugin, or MCP
tools from guest code is therefore unsupported, and nested calls do not trigger
other plugins' `tool.execute.before` or `after` hooks. The deadline includes time
spent waiting for approvals.

## Composio adapter

The [Composio host](../integrations/composio/README.md) runs one workflow under
`composio run`, which injects an authenticated `execute(slug, data, { account })`
helper and keeps the CLI credential in that process; the
[guide](composio-github.md) walks through it. One entry,
[`host.ts`](../integrations/composio/host.ts), has five commands, each bound to
one connected-account id (`--account ca_...`) and one repository (`--repo`):

- `investigate` runs a program through `codemode bridge` with GitHub read tools
  only. Rust denies every other tool; the host refuses reads of another
  repository without calling Composio, strips signed download URLs, and maps a
  failed read to a code without Composio's or GitHub's text. The program's
  returned proposal becomes a plan file: the host adds each operation's scope,
  appends a `<!-- codemode:<plan id>:<operation id> -->` marker to each comment,
  checks every operation against the contracts, and runs `codemode plan check`.
  The trace records result sizes, not values.
- `declare`, `preflight`, `run`, and `resume` serve the plan through the plan
  bridge with four contracts: add a label, remove a label, and set issue state
  (reconciled by state), and create a comment (reconciled by its marker,
  authored by the approved GitHub login). An `issue_state` precondition is read
  before each attempt. Each session re-checks the connection, the GitHub login,
  and the numeric repository id that the approval binds.

An unobserved execution is left unanswered and the run cancelled, so Rust records
it as `unknown`; the CLI helper cannot abort an execution. `codemode bridge`
treats SIGTERM like Ctrl-C on Unix, and `composio run` forwards signals to its
process group. An `@composio/core` executor (`sdkExecutor`) forwards each call's
abort signal; it is checked offline only. The Rust core contains nothing
Composio-specific.

What has been verified: offline, through the real bridges with a stand-in GitHub
and its own ledger (`tests/composio.rs`, `tests/composio_plan.rs`); live,
read-only investigation; and one scoped live label write and recovery with an
earlier host. See the [live record](composio-live-validation.md).

## Guest protocol

The worker is an implementation detail, not a host API. It receives one
`{ "type": "run", "code": "..." }` object. Guest requests are `call` objects with
an untrusted correlation `id`, a `tool` name, and JSON `arguments`. Rust supplies
`result` replies; the guest can return an `output` value or an `error` message.
All child stdout is validated as untrusted input. Rust assigns its own call IDs
and never accepts provider-result or trace-event assertions from the child.

The core does not select models or supply an agent loop. It runs a program
against explicit capabilities. MCP can be a transport adapter when needed; it is
not required inside the scheduler.

## Sources

- Pi 1.4.0 package docs: `docs/extensions.md` (`ctx.executeTool`, exposure, `tool_call`),
  `docs/rpc-commands.md` (`abort`), and `docs/codemode.md`; online at
  [pi.dev extensions](https://pi.dev/docs/latest/extensions).
- OpenCode 1.19.5 source (tag `v1.19.5`): `packages/plugin/src/tool.ts` and `index.ts`
  (`ToolContext`, `Hooks`), `packages/opencode/src/tool/registry.ts` (plugin tool
  wrapping), `session/tools.ts` (`ask` ruleset and abort signal), `permission/index.ts`,
  `tool/code-mode.ts`, and `cli/cmd/run.ts` (non-interactive approval refusal); online
  at [OpenCode tools](https://opencode.ai/docs/tools/) and [server plugins](https://opencode.ai/docs/build/plugins/server).
  The v2 plugin API (`@opencode-ai/plugin/v2`) registers no tools.
- [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).
- [Composio CLI](https://docs.composio.dev/docs/cli) 0.6.0 (release tag `@composio/cli@0.6.0`):
  `composio run --help` and `composio execute --help`; the `run` helper contract was
  checked by probing it. `@composio/core` 0.28.0 `tools.execute`.
