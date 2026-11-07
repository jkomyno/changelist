# Host integration contract

The target hosts are **Pi, OpenCode, Codex, and the Composio CLI**. The Rust
library and CLI share one execution contract. Host registration and tool access
are adapter responsibilities.

## Two distinct capabilities

**CLI invocation:** a host launches `codemode run --code ...`, receives JSON and
an exit status, and can request a separate trace file. That command uses the
built-in mock provider.

**Native tool callbacks:** a host adapter supplies selected registered tools to
the runtime and keeps the host's authorization, approvals, cancellation, and
result semantics. A child process does not inherit its parent's tool registry,
so this needs `codemode bridge` plus a host-specific adapter.

## Status

| Host | Verified | Remaining target |
| --- | --- | --- |
| Pi 1.4.0 | Native callbacks through [`integrations/pi`](../integrations/pi/README.md): nested calls, Pi permission gates, Rust-side authorization, cancellation, and environment containment. Checked offline with a scripted model. | A live model session; interactive approval prompts (`ctx.ui.confirm`) were not exercised; other Pi versions. |
| OpenCode 1.19.5 | Plugin tool through [`integrations/opencode`](../integrations/opencode/README.md): each guest call passes OpenCode's permission service (`allow`, `deny`, `ask`; approvals granted over OpenCode's HTTP API and refused by `opencode run`, handled as a native rejection), Rust-side authorization, session abort (including during an approval wait), and environment containment. Checked offline with a scripted model. | **Blocker:** plugins cannot call OpenCode's registered tools (built-in, plugin, or MCP), and nested calls do not reach other plugins' `tool.execute` hooks; guest calls reach only host tools passed to the adapter. A live model session; the TUI approval dialog; other OpenCode versions. |
| Codex | Not checked. | CLI invocation with an explicitly configured provider. Native callbacks need a demonstrated interface for calling Codex tools back; MCP exposes our tool to Codex, not Codex's tools to us. |
| Composio CLI 0.6.0 | `composio run --file` can launch `codemode run` and read its JSON result (offline mock tools only). | An optional provider bridge using the SDK, with authentication outside the guest. A live provider is untested. |

Validate launch, framing, tool scope, failure states, approval behavior, and
cancellation for each host/version before marking it compatible. Do not install
configuration or expose additional tools implicitly.

## Bridge protocol (version 1)

`codemode bridge [limit options]` serves one run over stdin and stdout. Frames are
LF-terminated JSON objects. Host frames are at most 1 MiB; the report frame is
bounded by the run limits (64 KiB source, 64 KiB arguments per call, and the
output budget). The library entry point is `codemode::serve_bridge`, which
accepts any async reader and writer.

| Direction | Frame | Meaning |
| --- | --- | --- |
| host → bridge | `{"type":"start","version":1,"code":"...","tools":["read"]}` | Must be first. `tools` is the host's authorization list. |
| bridge → host | `{"type":"call","id":1,"tool":"read","arguments":{}}` | Rust admitted and dispatched this call; `id` is the trace identity. |
| host → bridge | `{"type":"result","id":1,"ok":true,"value":...}` | Observed success. Use `"ok":false,"error":{"code","message"}` for failure. |
| host → bridge | `{"type":"result","id":1,"ok":true,"too_large":true}` | Observed value (or, with `"ok":false`, error) that cannot fit in a frame; recorded as `result_too_large`, ending the run with `output_limit`. |
| host → bridge | `{"type":"cancel"}` | Cancel the run. Closing the host's output does the same. |
| bridge → host | `{"type":"report","report":{...}}` | The execution record; the bridge then exits with the run's exit code. |
| bridge → host | `{"type":"error","message":"..."}` | The run never started, for example an invalid start frame. |

Rust behavior is unchanged from `run`: limits, the rolling scheduler, and call
identities are enforced on the Rust side, and calls outside `tools` are rejected
before the host sees them. The host never receives guest correlation tokens.
Results for unknown or finished calls are ignored, because they can race with a
deadline. A malformed host frame ends the run with `protocol_error`. Dispatched
calls without an observed reply become `unknown`. The host should abort its
outstanding calls when it reads the report.

The bridge does not authenticate the host: whoever owns its stdio is the trusted
provider. Limits come from command-line options chosen by the host, and
credentials stay in the host process.

## Host adapters

Both adapters use [`integrations/shared/bridge-client.ts`](../integrations/shared/bridge-client.ts),
a host-neutral client built on Node built-ins. It spawns the bridge with an empty
environment, frames calls and results, refuses calls that arrive after an abort,
replaces oversized replies with `too_large`, sends `cancel` on abort, and kills the
bridge two seconds later if it has not exited. An adapter supplies the authorized
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

## Guest protocol

The Deno launcher is an implementation detail, not a host API. It receives one
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
- [Composio CLI](https://docs.composio.dev/docs/cli).
