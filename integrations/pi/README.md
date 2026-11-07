# Pi integration

A Pi extension that runs agent-written TypeScript through `codemode bridge` and
executes its tool calls as Pi nested tool calls. Verified with Pi 1.4.0.

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
mise run pi-check    # Pi boundary tests, including abort through Pi RPC
```

Both fetch Pi 1.4.0 through mise on first use and need no model credentials.
A [scripted model](demo/offline-host.ts) calls `rust_codemode` once with
[`demo/workflow.ts`](demo/workflow.ts). An independent [approval gate](demo/approval-gate.ts)
blocks `bash` because no UI is available. Expected output:

```text
rust_codemode returned {"status":"success","calls":4,"output":{"entries":5,"environment":"denied","refused":["tool_error","tool_denied"],"title":"codemode Pi bridge fixture"}}
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
