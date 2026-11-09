# OpenCode integration

An OpenCode plugin that runs agent-written TypeScript through `codemode bridge`.
Every guest tool call is gated by OpenCode's permission service, then runs a host
tool inside the OpenCode process. Verified with OpenCode 1.19.5.

Program execution defaults to Deno. Set `backend: "embedded-v8"` in
`codemode({ tools, bin, backend })` to use a binary built with
`--features embedded-v8`; the adapter then neither looks up nor requires Deno.
The `deno` option applies only to the default Deno backend.

```text
OpenCode model ── rust_codemode {code} ──► plugin ── start ──► codemode bridge ──► Deno guest
                                            ▲   │                (Rust limits,       │
  context.ask("codemode_<tool>") ◄──────────┘   └── result ──►    scheduler, trace) ◄┘ tools.call
  then the host tool's execute()   (OpenCode rules, approvals, abort; credentials stay in OpenCode)
```

## What is native, and what is not

| OpenCode 1.19.5 capability | Used | How |
| --- | --- | --- |
| Plugin tool registration (`Hooks.tool`) | Yes | Registers `rust_codemode` for the model. |
| Permission service (`ToolContext.ask`) | Yes | One ask per guest call for the permission `codemode_<tool>` and pattern `*`, as OpenCode's own code mode does for MCP tools. `allow`, `deny`, and `ask` rules and approval replies apply. |
| Session abort (`ToolContext.abort`) | Yes | Sends `cancel` to Rust and aborts outstanding host calls. |
| Calling OpenCode's registered tools (built-in, plugin, or MCP) from a plugin | **Not available** | `ToolContext` has no tool executor, and the HTTP API only lists tools (`/experimental/tool`, `/experimental/tool/ids`). OpenCode's built-in `execute` code mode reaches MCP tools through internal services that plugins cannot use. |
| Other plugins' `tool.execute.before`/`after` hooks for nested calls | **Not available** | A plugin cannot trigger them. Those hooks see only the outer `rust_codemode` call. |

Guest code can therefore call only the host tools you pass to `codemode({ tools })`.
They are OpenCode custom tool definitions (`{ description, args, execute }`).
The adapter preserves OpenCode 1.19.5's legacy raw JSON-schema argument support;
its published plugin types describe only Zod argument schemas. The typecheck uses
the real package's execution, result, and context types, with an explicit legacy
argument type. Guest arguments must be JSON objects; other values are refused
with `invalid_arguments` before requesting permission. Calling an OpenCode built-in
such as `bash` from guest code is rejected by Rust as `tool_denied`.

Approval outcomes follow OpenCode's own semantics for a tool call:

- **Rule denial** (`deny`): the guest call rejects with `permission_denied` and
  OpenCode's message. The program may continue, as an OpenCode turn does.
- **Rejected approval** (a user, or `opencode run`, which refuses every approval):
  the call and every later call reject with `permission_rejected`, so no further
  host tool starts; calls already running finish. When the program ends, the adapter rethrows OpenCode's rejection, and
  OpenCode handles it as for a rejected native call: it stops the turn unless
  `experimental.continue_loop_on_deny` is set.
- **Approval with feedback** (rejecting with a message): `permission_denied` with
  the feedback as its message.
- **Abort or deadline while waiting**: the call stays `unknown` in the trace and
  never runs. OpenCode 1.19.5 keeps the approval request pending after a session
  abort; approving it later does not run the tool.
- **"Always"**: approves later calls of that host tool, in any session of that
  OpenCode project, while that OpenCode instance runs. OpenCode keeps approvals per
  project directory, not per session.

## Run the offline example

From the repository root, with mise installed:

```sh
mise run opencode-demo    # one scripted OpenCode session; prints the result, permission events, and trace outcomes
mise run opencode-check   # OpenCode boundary tests, including approval and abort over OpenCode's HTTP API
```

Both fetch OpenCode 1.19.5 through mise (`aqua:anomalyco/opencode`) on first use and
need no model credentials. They isolate OpenCode's configuration and state in a
temporary home, route non-loopback HTTP to a closed port, and pre-seed OpenCode's
config directory so it does not install its plugin package from npm.

[`demo/host.ts`](demo/host.ts) is a plugin that serves a scripted model on loopback,
calls `rust_codemode` once with [`demo/workflow.ts`](demo/workflow.ts), and records
OpenCode's permission events. [`demo/opencode.json`](demo/opencode.json) allows
`codemode_lookup`, asks for `codemode_publish`, and denies `codemode_purge`.
Expected output:

```text
rust_codemode returned {"status":"success","calls":4,"output":{"environment":"denied","refused":["permission_denied","tool_denied"],"stock":[3,0]}}
--- OpenCode permission events and model requests
{"event":"model","reply":"tool"}
{"event":"model","reply":"text"}
--- call outcomes recorded by Rust
status success
1 lookup succeeded
2 lookup succeeded
3 purge provider_failed permission_denied
4 bash rejected tool_denied
```

The two `lookup` calls ran concurrently and used a host credential the guest
never saw. A rule denied `purge` without asking, and it did not run. The guest
could not read environment variables.

For a controlled failure, run `integrations/opencode/run-demo.sh integrations/opencode/demo/approval.ts`.
`opencode run` refuses the `publish` approval. The program catches that error and
calls `lookup`, which the adapter refuses without asking; both calls are recorded
as `permission_rejected`. OpenCode then stops the turn with its own rejection error.

`mise run opencode-check` asserts both sessions, then drives `opencode serve` over
HTTP. One check approves `publish` with `POST /permission/:id/reply` (`once`), so it
runs, then aborts the session with `POST /session/:id/abort` during a slow call: the
report ends `cancelled`, and the interrupted call is `unknown`. Another aborts while
`publish` waits for approval, then approves the still-pending request; `publish`
never runs.

## Use it in OpenCode

```sh
mise exec -- cargo build --release
```

Write a plugin file that passes your host tools to the adapter. Tools run in the
OpenCode process, so their credentials stay there:

```ts
// ~/.config/opencode/plugins/codemode.ts
import { codemode } from "/path/to/codemode/integrations/opencode/codemode-bridge.ts";
import { lookup } from "./tools/lookup.ts";

export default codemode({
  tools: { lookup },
  bin: "/path/to/codemode/target/release/codemode",
});
```

Then set rules for the `codemode_<tool>` permissions in OpenCode's configuration,
for example `"permission": { "codemode_lookup": "allow", "codemode_publish": "ask" }`,
or `"codemode_*": "ask"` for every guest call. OpenCode's default agent rules allow
any permission they do not list (`"*": "allow"`), so set `ask` or `deny` explicitly
where a tool needs it. The prefix keeps host tools apart from OpenCode's built-in
permission keys such as `read` or `bash`.

| Option | Default | Meaning |
| --- | --- | --- |
| `tools` | required | Host tools guest programs may call, keyed by guest-visible name |
| `bin` | `codemode` on PATH | Rust executable |
| `deno` | `deno` on PATH | Deno executable for the guest |
| `timeoutMs` | 30000 | Run deadline, including time waiting for approvals |
| `concurrency` | 4 | Maximum concurrent host calls |
| `maxCalls` | 32 | Maximum tool calls per run |
| `trace` | unset | Write each run's full JSON report to this file |

OpenCode 1.19.5 does not resolve bare package imports from a plugin file outside
a directory with its own `node_modules`, and failed plugin imports are silent: the
tool simply does not appear. The adapter imports only Node built-ins. As for
OpenCode's own plugin tools, arguments are not validated against `args` before
`execute`, so host tools must check their inputs. A host tool's result is its
`output` string; `metadata` and attachments are not passed to the guest.

A failed run is reported to OpenCode as a tool error carrying the same summary.
Live model sessions, the TUI approval dialog, and other OpenCode versions were
not exercised. See [host integration contract](../../docs/integrations.md) for
the protocol and status.
