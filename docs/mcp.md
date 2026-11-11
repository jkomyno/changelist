# Use any MCP server

`codemode mcp` connects the `codemode` binary to one
[Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28)
server. A program reads through the server's tools and proposes a change list.
You review and approve that list. codemode sends it under a deadline with a
durable journal, and after an interruption it continues only what is safe.

No host adapter and no Deno are needed: the MCP client is part of the Rust
binary. The tool list comes from the server at run time, so nothing about a
tool is written into codemode.

**Status.** The client is tested offline against a stand-in server in both
protocol eras and on both transports. Live, it has listed the tools of two
public servers that speak revision 2026-07-28. Against a Composio Tool Router
session, which speaks 2025-11-25, it has investigated, run approved label
writes on one GitHub issue, and resumed after a deadline
([details](#composio-tool-router)). No other live server has received a write.

## The server file

One JSON file names the server and records what you trust about it.

```json
{
  "url": "https://mcp.example.com/mcp",
  "headers": { "x-api-key": "${EXAMPLE_API_KEY}" },
  "trust_annotations": true,
  "read_tools": []
}
```

```json
{
  "command": "my-mcp-server",
  "args": ["--workspace", "demo"],
  "env": { "TOKEN": "${TOKEN}" }
}
```

- `url` selects Streamable HTTP; `command` selects stdio. Give exactly one.
- `${NAME}` in a header or `env` value is read from the environment of the
  `codemode` process. The file itself holds no credential.
- A `url` must be `https`. Plain `http` is accepted for a loopback host only.
  A URL with a user name or password is refused. Redirects are not followed
  and proxy settings in the environment are not used, so a header never goes
  to another host.
- A stdio server inherits the environment of the `codemode` process, plus
  `env`.
- `trust_annotations` and `read_tools` decide each tool's class; see below.
  Both are off or empty unless you set them.

The **server identity** is what an approval binds. For a URL it is the URL
without its fragment; a query string is bound too, but shown only as
`?sha256=` and a digest, because a query can select a tenant and can hold a
key. For a command it is `stdio:` and the command with its arguments as a JSON
array. A name over 128 bytes is cut to 96 bytes plus a digest of the whole
name. It never includes a header or `env` value, so do not put a secret in the
URL path or in `args`.

## Tool classes

MCP describes a tool with a name, a JSON Schema for its arguments, and optional
annotations. The specification calls annotations hints, and says a client must
treat them as untrusted unless the server is trusted. codemode follows that:

| Class | A tool is in it when | What codemode does |
| --- | --- | --- |
| `read` | `read_tools` names it, or `trust_annotations` is true and the server marks it `readOnlyHint: true` | A program may call it during `investigate`. It cannot be in a plan. |
| `repeatable_write` | `trust_annotations` is true and the server marks it `idempotentHint: true` | It may be in a plan. After a lost reply it is sent again, up to the approved attempt budget. |
| `write` | every other tool | It may be in a plan. After a lost reply it is not sent again. A person decides. |

With an empty server file every tool is a `write`, and a program can call
nothing. Set `trust_annotations` only for a server you trust to describe its
tools correctly: a wrong `idempotentHint` makes codemode repeat a write that is
not safe to repeat.

```sh
codemode mcp tools --server server.json          # name, class, definition digest
codemode mcp tools --server server.json --json   # also schemas and annotations
```

## The workflow

```sh
# 1. Investigate: run a program with the read tools, get a plan.
codemode mcp investigate --server server.json --program triage.ts --out plan.json

# 2. Review: print every operation exactly as approval binds it.
codemode mcp declare --server server.json > declaration.json
codemode plan check plan.json --provider declaration.json

# 3. Approve the digest you reviewed.
codemode plan approve plan.json --journal run.db --provider declaration.json \
  --digest sha256:... --approver you

# 4. Run. From a second terminal, `codemode plan inspect --journal run.db`
#    shows progress without writing to the journal.
codemode mcp run --server server.json --journal run.db --deadline-ms 300000

# 5. After Ctrl-C, a crash, or a lost reply: resume, then review what is left.
codemode mcp resume --server server.json --journal run.db
codemode plan review --journal run.db --operation <id> --as applied --reviewer you
```

`plan check`, `plan approve`, `plan inspect`, `plan review`, and `plan trace`
are the same commands every provider uses; see
[recoverable agent actions](recovery.md).

### The program

The program is TypeScript that runs once in a V8 worker with no imports,
filesystem, network, or environment. It has two globals:

- `tools.call(name, arguments)` calls a `read` tool and returns its value.
  Any other tool name is refused before the server sees it.
- `input` is the JSON value of `--input <file>`, or `null`.

It returns the change list:

```ts
const { notes } = await tools.call("notes.list", {});
return {
  summary: "Mark every note as reviewed",
  operations: notes.map((note) => ({
    id: `mark-${note.id}`,
    tool: "notes.put",
    arguments: { id: note.id, text: `${note.text} (reviewed)` },
  })),
};
```

`investigate` refuses an operation that names a tool the server does not list,
names a `read` tool, or holds anything except `id`, `tool`, and `arguments`. It
checks the arguments against the top level of the tool's schema: required
properties, unknown properties when `additionalProperties` is `false`, and
simple types. The server stays the authority on its own schema.

A tool's value is its `structuredContent` when the server sends one. Otherwise,
when every content block is text, it is that text, parsed as JSON when it is
JSON. Otherwise it is the content array.

Limits: `--timeout-ms` (default 60000), `--max-calls` (32), `--concurrency`
(4), `--max-output-bytes` (1048576), `--heap-mb` (256). `--trace <file>` saves
the execution record for `codemode inspect`.

## What an approval binds

`codemode mcp declare` prints the provider declaration:

- the provider name `mcp` and the server identity;
- for each write tool, its contract: `repeat: safe` for a `repeatable_write`,
  `repeat: unsafe` for a `write`, and `reconcile: none` for both;
- for each write tool, a digest of its name, argument schema, and annotations.

`run` and `resume` connect again and build the declaration again. They refuse
the session, before any write, when the identity, a contract, or a definition
digest of a tool in the plan differs from the approved one. A reworded tool
description does not change the digest. A changed `trust_annotations` or
`read_tools` setting changes the contracts, so it also needs a new approval.

## What happens to each result

| The server's answer to a write | Recorded as | Next |
| --- | --- | --- |
| A result | applied | done |
| A result with `isError: true` | unknown | see below |
| A result that asks for input (elicitation, sampling, roots) | unknown | see below |
| The connection could not be made; nothing was sent | not applied | sent again, within the attempt budget |
| HTTP 400, 401, 403, 404, 405, 406, 413, 415, or 431, or JSON-RPC error -32700, -32601, -32020, -32021, -32022 | refused | review |
| Any other error or status, a broken stream, no reply, the deadline, cancellation | unknown | see below |

For an **unknown** outcome, a `repeatable_write` is sent again within the
attempt budget, and a `write` waits for `codemode plan review`. MCP gives a
client no way to ask a server whether an earlier call took effect, so codemode
never settles an unknown `write` by itself.

A tool error is recorded as unknown, not as a failure, because `isError` does
not say whether part of the work was done. This is strict: a plain validation
error from a `write` tool also needs a person. JSON-RPC "invalid params"
(-32602) and "invalid request" (-32600) are unknown for the same reason: some
servers raise them from a tool handler after the tool did its work.

Connecting, agreeing on a version, and listing the tools must finish within
60 seconds, before the session deadline starts.

When codemode stops waiting, it tells the server as the specification
requires: on Streamable HTTP it closes the connection, on stdio it sends
`notifications/cancelled`. Neither proves that the tool had no effect.

## Protocol support

- Revision **2026-07-28**: stateless requests with per-request `_meta`, the
  `MCP-Protocol-Version`, `Mcp-Method`, and `Mcp-Name` headers,
  `Mcp-Param-*` headers from `x-mcp-header` schema annotations, JSON and
  SSE responses, `tools/list` pagination, and `resultType`.
- Older servers: codemode first sends `server/discover`. A server that does
  not answer it as a 2026-07-28 server gets the `initialize` handshake of
  **2025-11-25**, **2025-06-18**, or **2025-03-26**, including the
  `Mcp-Session-Id` header on Streamable HTTP.
- Transports: Streamable HTTP and stdio. The deprecated HTTP+SSE transport of
  2024-11-05 is not supported.

Not supported: OAuth authorization flows (give a token in `headers`),
elicitation, sampling, roots, resources, prompts, tasks, subscriptions, and
caching by `ttlMs`. codemode declares no client capabilities, so a server that
needs one of them for a tool makes that call fail.

## Composio Tool Router

**Checked live on 2026-11-11.** Read-only against a staging project, then
with writes against production: label changes on one closed issue of this
repository, [#3](https://github.com/jkomyno/codemode/issues/3), which ended
with the labels it started with. Each write was compared with GitHub's own
API.

A Tool Router session has an MCP URL. By default the session lists three
meta-tools (`COMPOSIO_SEARCH_TOOLS`, `COMPOSIO_GET_TOOL_SCHEMAS`,
`COMPOSIO_MULTI_EXECUTE_TOOL`) and not the app tools. A generic client can
classify and bind only the tools the server lists, so create the session with
the app tools you need in `preload.tools`, and pin the account with
`connected_accounts`:

```sh
curl -s https://backend.composio.dev/api/v3.1/tool_router/session \
  -H "x-api-key: $COMPOSIO_API_KEY" -H 'content-type: application/json' \
  -d '{"user_id": "you", "connected_accounts": {"github": "ca_..."},
       "manage_connections": {"enable": false}, "workbench": {"enable": false},
       "preload": {"tools": ["GITHUB_LIST_REPOSITORY_ISSUES", "GITHUB_ADD_LABELS_TO_AN_ISSUE"]}}'
```

```json
{
  "url": "<mcp.url from the response>",
  "headers": { "x-api-key": "${COMPOSIO_API_KEY}" },
  "trust_annotations": true
}
```

What the live checks showed:

- The session endpoint speaks revision **2025-11-25**. codemode's
  `server/discover` probe is not answered as a 2026-07-28 server, and the
  `initialize` fallback connects.
- Every listed tool carries all four annotations. With `trust_annotations`,
  the classes were: `GITHUB_LIST_REPOSITORY_ISSUES` and `GITHUB_GET_AN_ISSUE`
  read; `GITHUB_ADD_LABELS_TO_AN_ISSUE` and
  `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE` repeatable write;
  `GITHUB_CREATE_AN_ISSUE_COMMENT` and `GITHUB_UPDATE_AN_ISSUE` write. The two
  search and schema meta-tools are reads; `COMPOSIO_MULTI_EXECUTE_TOOL` is a
  write.
- Two listings of the same session returned the same tools and definitions,
  on staging and on production.
- The session id is part of the URL path, so an approval is bound to one
  session, and through it to that session's user and pinned accounts. After
  the session is deleted, codemode cannot connect to it.
- A tool's value is Composio's envelope, `{successful, data, error, log_id}`.
  A program reads `result.data`.
- **A read with data.** A program read the issue through
  `GITHUB_GET_AN_ISSUE` and proposed one label addition.
- **A failed execution sets `isError`.** A read of an issue that does not
  exist (GitHub 404), a removal of a label that is not on the issue (GitHub
  404), and a read without an active connection each came back as a tool
  error with `successful: false` inside. The program can catch the read
  error; the failed write was recorded as unknown, not as applied.
- **Writes.** An approved `GITHUB_ADD_LABELS_TO_AN_ISSUE` and an approved
  `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE` each ran once and were recorded as
  applied. GitHub showed the label, then no label.
- **A late effect.** The same addition ran again with a 400 ms deadline.
  codemode stopped waiting and recorded the outcome as unknown; GitHub showed
  the label anyway. `mcp resume` sent the addition a second time, because its
  class is repeatable write, and recorded it as applied. The issue held the
  label once.

Know this about `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE`: the server marks it
idempotent, but a repeat after a removal that worked returns GitHub's 404 as a
tool error. So after a lost reply codemode sends it again, gets tool errors
until the attempt budget is spent, and then asks for a review. That is safe,
but it does not settle by itself.

Not checked: a `write` class tool (for example a comment), a killed process,
more than one operation in a plan, and every way an execution can fail. A
failure that came back as a normal result with `successful: false` and no
`isError` would be recorded as applied; none was seen.

Keep `COMPOSIO_MULTI_EXECUTE_TOOL` out of plans. It is one opaque write to
codemode, which cannot see or bind the tools it runs.

## Limits

- No automatic reconciliation. A per-tool way to verify a write (which read
  confirms it) is not part of MCP. Until a server can declare one, every
  uncertain non-repeatable write goes to a person.
- No preconditions and no destination scoping: MCP has no place for either, so
  a plan that holds them is refused.
- The account behind a server is not visible to the client. An approval binds
  the server identity, not a login. If the same URL starts to act as another
  account, codemode cannot tell.
- Argument checking before approval covers the top level of the schema only.
- One server per plan.
