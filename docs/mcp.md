# Use any MCP server

codemode connects to one
[Model Context Protocol](https://modelcontextprotocol.io/specification/2026-07-28)
server. A program or a script proposes writes. codemode puts them in one plan
file. A person reads the plan and approves it once. codemode then sends exactly
those writes, stops at the first failure, and never repeats a write it cannot
show is safe to repeat.

```
codemode tools   --server <server.json>
codemode plan    --server <server.json> (--program <file.ts> | --operations <file|->)
codemode status  <plan file>
codemode apply   <plan file>
codemode resolve <plan file> (<id>... | --errors | --no-reply) --as applied|not-applied
```

The MCP client is part of the Rust binary. The tool list comes from the server
at run time, so nothing about a tool is written into codemode.

**Status.** The client and the five commands are tested offline against a
stand-in server in both protocol eras and on both transports; `mise run demo`
runs the whole flow. Live, an earlier build listed the tools of two public
servers that speak revision 2026-07-28, and ran writes against a Composio Tool
Router session ([details](#composio-tool-router)). The commands on this page
have not been run against a live server.

## The server file

One JSON file names the server and records what you trust about it.

```json
{
  "url": "https://mcp.example.com/mcp",
  "headers": { "x-api-key": "${EXAMPLE_API_KEY}" },
  "trust_annotations": true,
  "read_tools": [],
  "write_tools": ["GITHUB_ADD_LABELS_TO_AN_ISSUE"]
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
- `write_tools` is optional. When it is present, a plan may hold only the
  tools it names. `plan` refuses any other write, and `apply` checks again
  before it sends anything. When it is absent, a plan may hold every tool that
  is not a read. The operator writes this list, not the program: it is the
  least a program needs, stated on the client side.
- `read_tools` and `write_tools` must name tools the server lists.

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
| read | `read_tools` names it, or `trust_annotations` is true and the server marks it `readOnlyHint: true` | A program may call it. A plan cannot hold it. |
| repeatable write | `trust_annotations` is true and the server marks it `idempotentHint: true` | A plan may hold it. After a lost reply it is sent again, up to the approved number of attempts. |
| write | every other tool | A plan may hold it. After a lost reply it is not sent again. A person decides. |

With an empty server file every tool is a write, and a program can call
nothing. Set `trust_annotations` only for a server you trust to describe its
tools correctly: a wrong `idempotentHint` makes codemode repeat a write that is
not safe to repeat.

```
$ codemode tools --server server.json
server   http://127.0.0.1:63656/mcp  (MCP 2026-07-28)

  tool               class             server marks it  in a plan
  notes.append       write             -                yes
  notes.delete       repeatable write  destructive      yes
  notes.get          read              -                no: a program may call it
  notes.list         read              -                no: a program may call it
  notes.put          repeatable write  -                yes
  plain.unannotated  write             no annotations   yes
  raw.json_text      read              -                no: a program may call it
  raw.text           read              -                no: a program may call it
  region.echo        read              -                no: a program may call it
```

"destructive" means the server sent `destructiveHint: true`. "no annotations"
means the server sent no hint at all. With `write_tools`, the last column says
`no: not in write_tools` for every other write. When `trust_annotations` is
off, a line under the table says so. `--json` prints the same with each tool's
schemas, annotations, and definition digest.

## Make a plan

`codemode plan` connects, lists the tools, gets the writes from one of two
sources, checks them, and writes one plan file. It sends no write.

```sh
codemode plan --server server.json --program triage.ts       # triage.plan.json
codemode plan --server server.json --operations ops.json     # ops.plan.json
some-script | codemode plan --server server.json --operations -   # codemode.plan.json
```

The file goes to the current directory under the name of its source, or to
`--out <file>`. A plan that was approved is never overwritten: `plan` refuses
when the plan's journal exists, and says to choose another `--out`. A change
list without operations prints `No changes.`, writes no file, and exits with 0.

### The change list

Both sources give the same JSON object, and both get the same checks:

```json
{
  "summary": "Mark every note as reviewed",
  "operations": [
    { "id": "mark-7", "tool": "notes.append", "arguments": { "id": "7", "text": "reviewed" } }
  ]
}
```

`plan` refuses an operation that names a tool the server does not list, names
a read tool, names a tool that `write_tools` does not allow, or holds anything
except `id`, `tool`, and `arguments`. It checks the arguments against the top
level of the tool's schema: required properties, unknown properties when
`additionalProperties` is `false`, and simple types. The server stays the
authority on its own schema. An `id` is 1 to 64 of `A-Z a-z 0-9 . _ -` and is
unique in the plan.

### The program

`--program` runs TypeScript once in a V8 worker with no imports, filesystem,
network, or environment. It has two globals:

- `tools.call(name, arguments)` calls a read tool and returns its value.
  Any other tool name is refused before the server sees it.
- `input` is the JSON value of `--input <file>`, or `null`. The program and
  its input together must fit in 64 KiB. A program gets larger data through
  a read tool.

It returns the change list:

```ts
const { notes } = await tools.call("notes.list", {});
return {
  summary: "Add a review line to every note",
  operations: notes.map((note) => ({
    id: `mark-${note.id}`,
    tool: "notes.append",
    arguments: { id: note.id, text: "reviewed" },
  })),
};
```

A tool's value is its `structuredContent` when the server sends one. Otherwise,
when every content block is text, it is that text, parsed as JSON when it is
JSON. Otherwise it is the content array.

Limits: `--timeout-ms` (default 60000), `--max-calls` (32), `--concurrency`
(4), `--max-output-bytes` (34603008), `--heap-mb` (256). The output budget
covers the results of the tool calls and the returned change list together. It
is the largest plan plus 1 MiB, so a program can return as many operations as
a plan can hold. One tool call's arguments stay within 64 KiB. `--trace
<file>` saves the program's execution record as JSON.

### The plan file

```json
{
  "codemode_plan": 1,
  "summary": "Mark every note as reviewed",
  "server": {
    "identity": "http://127.0.0.1:63656/mcp",
    "file": "server.json",
    "config": "sha256:d7f8edfc46ca5d48bc3e11541950e307fafbddb8e2ea5ae10068dcf2ac7857b1",
    "protocol": "2026-07-28",
    "trust_annotations": true
  },
  "tools": {
    "notes.put": {"class": "repeatable_write", "definition": "sha256:b773d3e9163b605272fc4a849c6288a5f65b800e5b0751eb5f51ec518340da97", "destructive": false, "annotated": true}
  },
  "operations": [
    {"id": "mark-a", "tool": "notes.put", "arguments": {"id": "a", "text": "alpha (reviewed)"}},
    {"id": "mark-b", "tool": "notes.put", "arguments": {"id": "b", "text": "beta (reviewed)"}}
  ]
}
```

The file is self-contained, and it is made to be read by a second person, for
example in a pull request:

- Keys are in a fixed order. There is one tool per line and one write per
  line, in the order given. Argument keys are sorted. The same change list
  for the same server always gives the same bytes.
- `tools` records what `plan` saw for each tool the writes use: its class, a
  digest of its name, argument schema, and annotations, whether the server
  marks it destructive, and whether the server sent any annotation.
- `server.file` is the path of the server file, relative to the plan file.
  Because of it, `status`, `apply`, and `resolve` need no `--server`.
- `server.config` is the SHA-256 of the server file's bytes as `plan` read
  them. `apply` uses only a server file with that digest. So an approval
  binds the server file itself: its URL or command, its `env` and header
  entries, its trust settings, and its `write_tools`.
- The file never holds a header or `env` value.
- A control character, a bidirectional control, or a zero-width character in
  a string is written as a JSON `\u` escape, so it cannot hide or reorder
  text in a terminal or a diff view. The value is unchanged.

### One digest

A plan has one digest: `sha256:` and 64 hex digits. Text output shows the
first 12 digits. `status --json` prints all of them.

The digest is the SHA-256 of the plan file's canonical JSON (keys sorted, no
white space) without `server.file`. It covers the writes and their order, the
summary, the server identity, the digest of the server file, and each tool's
class, definition digest, and flags. Layout does not change it. One changed
character in an argument does. The server file's path is left out so that the
files may move. Its content is bound through `server.config`.

`plan` refuses to write a plan file over 32 MiB. Escaping can make the file
larger than the change list it came from.

## Review

`plan` prints the review. `status` prints the same text until the plan is
approved, and `apply` prints it before it asks.

```
$ codemode plan --server server.json --operations tidy.json
tidy.plan.json  sha256:535c97e0b847
server   http://127.0.0.1:63656/mcp  (MCP 2026-07-28)
config   server.json  sha256:d7f8edfc46ca
summary  Tidy the notes
status   not approved

4 writes with 3 tools

notes.delete  1 write  repeatable  destructive
  drop-b  id="b"

notes.put  2 writes  repeatable
  different             id: 2 values ("c", "d")
                        text: 2 values ("gamma", "delta")

  write  id   text
  put-c  "c"  "gamma"
  put-d  "d"  "delta"

notes.append  1 write  not repeatable
  log  id="log"  text="tidied"

Notes
  ! 1 write uses a tool the server marks destructive: notes.delete.
  Tool classes come from the server's own hints.

If a reply is lost: a repeatable write is sent again (up to 3 attempts). Any other write waits for a person.
Sending: the first write alone, then up to 4 at a time. A failed write stops the rest.
Next: codemode apply tidy.plan.json
```

- The `config` line names the server file and the first digits of its
  digest. `status` reads the file again each time. It prints `changed since
  the plan was made` when the bytes differ and `cannot be read` when the file
  is gone.
- For a stdio server whose identity had to be shortened, a `command` line
  prints the whole command and its arguments. An approval will run that
  command.
- The summary is printed on one line. Each run of white space becomes one
  space, and a summary over 200 characters is cut and shown with its size in
  bytes. `--full` prints it as a JSON string, with nothing dropped.
- Writes are grouped by tool. Tools the server marks destructive come first,
  then tools without annotations, then the tools with the most writes.
- For each tool the review says which arguments have the same value in every
  write, and which differ. For an argument that differs it gives the number of
  distinct values, and a few of them when they are short.
- Then it lists the writes: the identity and one column for each argument that
  differs. It shows 20 writes per tool and says how many it hides. A value
  over about 60 characters is cut and shown with its size in bytes.
- A plan with one or two writes prints every argument in full.
- `--full` prints every write with its arguments as canonical JSON, never cut.
- Notes appear only when they apply: a tool without annotations, a tool
  marked destructive, writes that are identical to another write, and that
  the classes come from the server's hints.

Every string from a plan, a program, or a server is escaped before it is
printed. A control character, a format character such as a bidirectional
control, or a character that is drawn as nothing (zero-width characters,
fillers, variation selectors) appears as `\u{1b}` in text and as `\u001b`
inside JSON. A value can never start a new line of output. What a stdio server
writes to its standard error is shown the same way, one line at a time, with
the prefix `server: ` and at most 4 KiB of a line.

## Approve and apply

`codemode apply <plan file>` is the only command that sends writes. It
approves, runs, and continues.

**The first run** works in this order:

1. It reads the plan file and the server file. It refuses, with exit code 2,
   a server file whose digest is not the plan's `server.config`, and a write
   that `write_tools` does not allow.
2. It prints the review.
3. It gets the approval, as described below.
4. It connects to the server and compares it with the plan.
5. It creates the journal, records the approval, and sends the writes.

No server process is started and no request is made before the approval. When
step 4 fails, the command exits with 2 and says that the plan was approved
but nothing was recorded or sent. Run it again when the server is reachable.

The approval comes in one of two ways:

- When stdin, stdout, and stderr are all terminals it asks, for example
  `Apply these 4 writes through http://127.0.0.1:63656/mcp? Type "yes" to
  approve:`. Only the exact answer `yes` approves. The question says first
  when a write uses a tool marked destructive or a tool without annotations.
  All three must be terminals, so that the review is on the screen when the
  person answers.
- Otherwise it asks nobody. It exits with 4 and says what to do:

  ```
  codemode: not approved. A person must approve this plan:
    run `codemode apply tidy.plan.json` in a terminal and answer the question, or
    pass `--approve sha256:535c97e0b847670c96b5ded03c3dbaac6f49046e3b3e617362a750ee9e3bff6c`
  Nothing was sent.
  ```

- `--approve <digest>` approves when it is the plan's full digest. It is also
  accepted in a terminal, where it replaces the question. A wrong or shortened
  digest is refused with exit code 4, and nothing is written.
- With `--json` nobody is asked either. Pass `--approve`.

An agent that can run a shell can also run `apply`. The rule above is what
keeps a person in the loop: an approval is an answer typed at a terminal, or a
digest that someone passed on purpose.

The approval records who approved: `--approver <name>`, or `$USER`, or
`unknown`. The name is recorded as given. It is not authenticated.

Two options are part of the approval. They are accepted on the first run only,
and the review's last lines show them:

- `--concurrency <n>` (default 4): writes in flight at once.
- `--max-attempts <n>` (default 3): times one write may be sent, the first
  included.

**The server file.** `apply` reads the file that `server.file` names, or the
file given with `--server <file>`. In both cases the file must have the
digest in `server.config`. Otherwise `apply` exits with 2, says that the
server file is not the one the plan was made with, and says to run
`codemode plan` again. Use `--server` where the operator keeps the server
file, for example in CI. The path in a plan file can be edited without
changing the plan's digest. The file's content cannot.

**Before any write**, every run connects and compares the server with the
plan. It refuses, with exit code 2, when the server identity differs, when a
tool the plan uses is gone, or when a tool's class or definition digest
differs. The message says to run `codemode plan` again. A reworded tool
description does not change a definition digest.

**The journal** is created next to the plan file: the plan's path with `.json`
replaced by `.journal.db` (`tidy.plan.json` gives `tidy.plan.journal.db`). It
holds the approval and every attempt, and it is the only memory of what was
sent. There is no option to keep it elsewhere. A plan file copied to another
directory without its journal is a new plan, and its writes are sent again
once it is approved.

**A later run** continues the same plan and asks no question:

- Work remains: it sends what can be sent.
- Nothing remains: it prints the status and sends nothing.
- The plan file was edited after the approval, so its digest differs: it
  refuses with exit code 4 and changes nothing. Restore the approved file. To
  apply the edited plan as a new plan, save it under another name.

Every run takes two options that are not part of the approval:

- `--deadline-ms <n>` (default 300000) is one deadline for the run.
- `--max-failures <n>` (default 1) stops the run early; see
  [when a run stops early](#when-a-run-stops-early). `0` never stops.

While it runs in a terminal, `apply` prints one line to stderr for each
attempt, after the journal holds its outcome: the identity, the tool, and the
outcome. At the end it prints the status. With `--json`, stdout holds exactly
one JSON document.

## Status

`codemode status <plan file>` reads the plan file and its journal. It uses no
network and changes nothing, so it works from a second terminal while `apply`
runs. Before approval it prints the review. Afterwards:

```
$ codemode status tidy.plan.json
tidy.plan.json  sha256:535c97e0b847
server   http://127.0.0.1:63656/mcp
config   server.json  sha256:d7f8edfc46ca
status   stopped after 1 failure
         approved by demo on 2026-11-11 15:14 UTC

applied 1 · failed 1 · no reply 0 · not sent 2

failed (1): the server answered with an error
  log  notes.append
    "upstream said no"
    codemode cannot tell whether a failed write changed anything. Check the target, then run:
      codemode resolve tidy.plan.json log --as not-applied   (or --as applied)

not sent (2)
  Run `codemode apply tidy.plan.json` to continue.

applied (1)
  put-c
```

One line counts the writes. Sections follow, each only when it has writes:

| Section | A write is in it when | Next |
| --- | --- | --- |
| failed | the server answered with an error that does not say whether the write changed anything | `resolve`; a repeatable write is sent again by `apply` |
| no reply | the write may have reached the server, and no answer came back | `resolve`; a repeatable write is sent again by `apply` |
| refused | the write was not applied and is not sent again: the server refused it before running it, or every attempt is used | fix the cause and `resolve --as not-applied`, or make a new plan |
| resolved by a person | a person recorded it as applied | nothing |
| not sent | it was not sent yet, or it is known not to be applied and will be sent again | `apply` |
| applied | the server answered with a result | nothing |

Inside a section, writes with the same error text are shown together, with the
text once. Each section that needs an action gives the exact command. A list
of identities stops at 20 and says how many it hides; `--full` prints all.

The `status` line is one of: `not approved`, `approved, not started`,
`running, or interrupted before it could record its end`, `stopped after N
failures`, `finished with N failures`, `stopped at the deadline`, `cancelled`,
`stopped with writes left to send`, `done: every write is applied`, and
`edited after approval`. A session without a recorded end is running, or it
was killed; the journal cannot tell which.

### `--json`

`plan`, `status`, and `apply` print the same document with `--json`:

```json
{"file": "tidy.plan.json",
 "digest": "sha256:535c97e0b847670c96b5ded03c3dbaac6f49046e3b3e617362a750ee9e3bff6c",
 "server": {"identity": "http://127.0.0.1:63656/mcp", "protocol": "2026-07-28",
            "config": "sha256:d7f8edfc46ca5d48bc3e11541950e307fafbddb8e2ea5ae10068dcf2ac7857b1",
            "file": "server.json", "file_state": "same"},
 "summary": "Tidy the notes",
 "state": "needs_attention",
 "detail": "stopped after 1 failure",
 "approval": {"approver": "demo", "approved_unix_ms": 1794410077119,
              "approved_at": "2026-11-11 15:14 UTC", "concurrency": 1, "max_attempts": 3,
              "digest": "sha256:535c97e0b847670c96b5ded03c3dbaac6f49046e3b3e617362a750ee9e3bff6c"},
 "counts": {"total": 4, "applied": 1, "failed": 1, "no_reply": 0, "refused": 0,
            "resolved": 0, "not_sent": 2},
 "writes": [
   {"id": "put-c", "tool": "notes.put", "state": "applied", "attempts": 1,
    "error_code": null, "error_text": null, "answered": true, "next": "none"},
   {"id": "log", "tool": "notes.append", "state": "failed", "attempts": 1,
    "error_code": "tool_error", "error_text": "upstream said no", "answered": true,
    "next": "resolve"},
   {"id": "put-d", "tool": "notes.put", "state": "not_sent", "attempts": 0,
    "error_code": null, "error_text": null, "answered": false, "next": "apply"},
   {"id": "drop-b", "tool": "notes.delete", "state": "not_sent", "attempts": 0,
    "error_code": null, "error_text": null, "answered": false, "next": "apply"}
 ]}
```

- `state` is `not_approved`, `approved`, `running`, `done`, `needs_attention`
  (a write waits for a person), `incomplete` (only `apply` has work left), or
  `edited`. `approval` is `null` before an approval.
- `server.file_state` is `same`, `changed`, or `unreadable`, as the command
  found the server file. `server.command` is present for a stdio server whose
  identity was shortened.
- A write's `state` is `applied`, `failed`, `no_reply`, `refused`, `resolved`,
  or `not_sent`. Its `next` is `none`, `apply`, `resolve`, or `new_plan`.
- `error_code` and `error_text` describe the write's last attempt when it did
  not succeed; see [codes](#codes). `answered` says whether an answer came
  back for it. A write that a person resolved also has `resolved_by`.
- `plan --json` for a change list without operations prints
  `{"file": null, "state": "no_changes", "writes": []}`.

## Resolve

When a write failed or got no answer, codemode does not know whether it
changed anything. MCP gives a client no way to ask a server whether an earlier
call took effect, so codemode never settles such a write by itself. A person
checks the target and records what they found:

```
$ codemode resolve tidy.plan.json log --as not-applied
Recording 1 write as not applied (reviewer demo):

  log  notes.append
    "upstream said no"

Recorded 1 write. Next: codemode apply tidy.plan.json
```

- `--as applied`: the change is there. The write is done.
- `--as not-applied`: the change is not there. `apply` sends the write again,
  while attempts are left.
- Name writes by identity, or select them: `--errors` takes every write in
  the `failed` section, and `--no-reply` every write in the `no reply`
  section. Identities and selectors can be combined. A `refused` write is
  resolved by its identity.
- It prints what it selected, grouped by error text, and then records it.
- It refuses a write that is applied, resolved, or not sent yet, and then
  records nothing.
- The reviewer is `--reviewer <name>`, or `$USER`, or `unknown`. It is
  recorded as given and not authenticated.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Every write is applied or resolved, or there is nothing to change, or a command that only reads succeeded. |
| 2 | Usage error, unreadable file, invalid plan, a server file that is not the plan's, a damaged or locked journal, or a server or program failure before any write. |
| 3 | The plan needs attention: a write failed, has no answer, or was refused, or the run stopped with writes left to send. Also a journal failure after a run started, because a write may have been sent. |
| 4 | Not approved: no approval was given, the digest was wrong, or the plan was edited after its approval. |

`status` exits with 0 whenever it can read the plan, whatever the plan's
state.

## What happens to each answer

| The server's answer to a write | Section | Next |
| --- | --- | --- |
| A result | applied | nothing |
| A result with `isError: true` | failed | see below |
| A result that asks for input (elicitation, sampling, roots) | failed | see below |
| The connection could not be made; nothing was sent | not sent | sent again by a later `apply`, while attempts are left |
| HTTP 429 | not sent | sent again in the same run after a wait; see [rate limits](#rate-limits) |
| HTTP 400, 401, 403, 404, 405, 406, 413, 415, or 431, or JSON-RPC error -32700, -32601, -32020, -32021, -32022 | refused | fix the cause, then `resolve --as not-applied` |
| Any other error or status, or a reply that is not valid MCP | failed | see below |
| A broken stream, no reply, the deadline, cancellation, a killed `apply` | no reply | see below |

For a **failed** write and for a write with **no reply**, the outcome is
unknown. A repeatable write is sent again by a later `apply`, while attempts
are left. A write waits for `codemode resolve`.

A tool error is unknown, not a plain failure, because `isError` does not say
whether part of the work was done. This is strict: a plain validation error
from a write tool also needs a person. JSON-RPC "invalid params" (-32602) and
"invalid request" (-32600) are unknown for the same reason: some servers raise
them from a tool handler after the tool did its work.

Connecting, agreeing on a version, and listing the tools must finish within
60 seconds, before the deadline starts.

When codemode stops waiting, it tells the server as the specification
requires: on Streamable HTTP it closes the connection, on stdio it sends
`notifications/cancelled`. Neither proves that the tool had no effect.

A second `apply` or a `resolve` cannot take a journal that an `apply` holds.
It exits with 2 and says so. `status` can read meanwhile.

### When a run stops early

Every run sends its first write alone. The other writes start only after that
write has an answer. So a server that fails every write receives one write,
not as many as the approved concurrency.

A run stops sending after `--max-failures` writes have ended without being
applied. The default is 1: the run stops after the first one. A write counts
when its attempt ended in any way but applied: failed, with no reply,
refused, or not sent because the connection could not be made. A write that
a rate limit only delayed does not count.

- Writes already sent finish and are recorded as usual. After the first
  write, up to `--concurrency` writes can be out when one fails.
- A write that was not sent stays `not sent`, and a later `apply` sends it.
- The run exits with 3, and the status line says `stopped after 1 failure`.

Use `--max-failures 0` to send every write whatever happens. Each failed
write then needs its own `resolve`; `resolve --errors` takes them together.

### Rate limits

An HTTP 429 answer to a `tools/call` means the server refused the request
before it ran the tool. This holds for every tool class, so the attempt is
recorded as not applied and the write is sent again in the same run:

1. The run waits. The wait is the `Retry-After` header, given as seconds or
   as an HTTP date. Without the header it is 1 second. The wait doubles for
   each 429 in a row, and one wait is at most 60 seconds.
2. During the wait the run sends nothing to the server, for any write. A rate
   limit belongs to the credential, not to one write.
3. After the wait the run sends one write first. When the server answers it
   with anything but a 429, the others follow.

Each time a write is sent it uses one of its approved attempts
(`--max-attempts`, default 3). When the attempts are used, the write is
`refused` with the code `rate_limited` and needs a new plan. When the wait
would end after the deadline, the write stays `not sent`, and a later `apply`
sends it. Both count for `--max-failures`.

A stdio server has no status codes, so it cannot signal a rate limit this way.
A 429 while codemode connects or lists the tools is a startup error; nothing
was sent yet, so run the command again later.

### Codes

A write whose last attempt did not succeed has an `error_code` in
`status --json`:

| `error_code` | Meaning | `answered` | Section |
| --- | --- | --- | --- |
| `tool_error` | The tool's result has `isError: true` | true | failed |
| `input_required` | The server asked for input codemode cannot give | true | failed |
| `rpc_<code>` | A JSON-RPC error, for example `rpc_-32603` | true | refused for -32700, -32601, -32020, -32021, -32022; otherwise failed |
| `http_<status>` | An HTTP error status without a JSON-RPC error, for example `http_500` | true | refused for 400, 401, 403, 404, 405, 406, 413, 415, 431; otherwise failed |
| `rate_limited` | HTTP 429 | true | not sent; refused when every attempt is used |
| `invalid_response` | A reply arrived that is not valid MCP | true | failed |
| `no_response` | The request was sent and the connection broke or ended without a reply | false | no reply |
| `deadline` | The deadline passed before a reply | false | no reply |
| `cancelled` | The run was cancelled before a reply | false | no reply |
| `not_sent` | The connection could not be made | false | not sent |
| `not_a_write_tool`, `preconditions_unsupported` | codemode refused the write before sending it | false | refused |
| `withdrawn` | The run ended after the attempt was recorded and before it was sent | false | not sent |
| `no_result` | The journal has no answer for the attempt: `apply` was killed, or is still running | false | no reply |

### The error text

For a failed attempt the journal keeps the error text, cut to 1 KiB:

- for a tool error, the text of the result;
- for a JSON-RPC error, its code and message;
- for an HTTP error, the status code with its standard name, for example
  `the server returned HTTP status 500 Internal Server Error`. The body of an
  HTTP error is not kept.

The text is the server's own words. codemode puts no header, `env` value, or
URL into it. A server could repeat a credential it received, so before the
text is kept or shown, codemode replaces these values in it with `[redacted]`:
each configured header value, each `env` value, and the URL's query and each
of its values. A header or `env` value is also matched without its first
words, so the token of `Bearer <token>` is replaced by itself.

A value shorter than 8 bytes is not replaced. A short value is not a
credential, and removing it would garble ordinary words. Do not rely on the
replacement for a short header, `env`, or query value.

The same replacement applies to every error a server answers while codemode
connects and lists the tools, in `tools`, `plan`, and `apply`, and to each
line a stdio server writes to its standard error.

`status` and `apply` print the text in the section of the write, and as
`error_text` with `--json`. `resolve` prints it for what it selects.

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
API. The check used the earlier commands (`codemode mcp ...`) at commit
`9be2351` and has not been repeated with the commands on this page.

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
  error; the failed write was recorded with an unknown outcome, not as
  applied.
- **Writes.** An approved `GITHUB_ADD_LABELS_TO_AN_ISSUE` and an approved
  `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE` each ran once and were recorded as
  applied. GitHub showed the label, then no label.
- **A late effect.** The same addition ran again with a 400 ms deadline.
  codemode stopped waiting and recorded the outcome as unknown; GitHub showed
  the label anyway. The next session sent the addition a second time, because
  its class is repeatable write, and recorded it as applied. The issue held
  the label once.

Know this about `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE`: the server marks it
idempotent, but a repeat after a removal that worked returns GitHub's 404 as a
tool error. So after a lost reply codemode sends it again, gets tool errors
until the attempt budget is spent, and then waits for a person. That is safe,
but it does not settle by itself.

Not checked: a `write` class tool (for example a comment), a killed process,
more than one write in a plan, and every way an execution can fail. A
failure that came back as a normal result with `successful: false` and no
`isError` would be recorded as applied; none was seen.

Keep `COMPOSIO_MULTI_EXECUTE_TOOL` out of plans. It is one opaque write to
codemode, which cannot see or bind the tools it runs.


## Limits

- No automatic check of an unknown write. A per-tool way to verify a write
  (which read confirms it) is not part of MCP. Until a server can declare one,
  every uncertain write that is not repeatable goes to a person.
- No preconditions and no restriction to one destination: MCP has no place for
  either, so a change list that holds them is refused.
- The account behind a server is not visible to the client. An approval binds
  the server identity, not a login. If the same URL starts to act as another
  account, codemode cannot tell.
- The name of an approver or a reviewer is recorded as given. It is not
  authenticated.
- Argument checking before approval covers the top level of the schema only.
- One server per plan.
- A plan holds at most 10,000 writes and 32 MiB, and the arguments of one
  write at most 64 KiB. Every journal commit is flushed to storage, so a run
  takes time: 5,000 `notes.put` writes against the offline test server over
  Streamable HTTP took about 53 seconds on one laptop with a debug build, with
  at most 87 MiB of memory and a 4.5 MiB journal. Choose `--deadline-ms` with
  that in mind; a run that reaches its deadline is continued by the next
  `apply`.
