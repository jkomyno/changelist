# codemode

codemode turns many writes into one change list that a person reads and
approves once. It then sends exactly that list through an
[MCP](https://modelcontextprotocol.io/specification/2026-07-28) server, stops
at the first failure, and never repeats a write it cannot show is safe to
repeat.

It is one Rust binary with five commands. There is no release yet. You build
it from source.

## The problem

An agent that labels 200 issues or updates 2,000 records has two ways to ask
for permission, and both fail.

- **One prompt per call.** Nobody reads the 40th prompt. The person approves
  them all, and the approval means nothing.
- **One prompt for a script.** The person approves a program, not its effects.
  One approved call can run every tool the program decides to call.

Then the batch breaks in the middle. A reply is lost, or a rate limit hits, or
write 31 of 200 is refused. The agent retries and sends a comment or an email
twice, or it gives up and nobody knows which writes happened.

codemode puts a plan file between the agent and the server. The plan is data,
not code. It lists every write with its exact arguments. A person approves that
file, and only that file is sent.

## What it looks like

```sh
codemode tools  --server server.json                       # what the server offers
codemode plan   --server server.json --program triage.ts   # writes triage.plan.json, sends nothing
codemode apply  triage.plan.json                           # shows the review, asks, then sends
```

`plan` and `apply` print the same review. It groups the writes by tool and
says which arguments are the same in every write and which differ, so 200
writes read as a few lines and a table. This is the output for a plan against
the offline test server.

```
triage.plan.json  sha256:8fbe6dca0bf3
server   http://127.0.0.1:63656/mcp  (MCP 2026-07-28)
config   server.json  sha256:d7f8edfc46ca
summary  Triage the open issues
status   not approved

12 writes with 3 tools

notes.delete  2 writes  repeatable  destructive
  different             id: 2 values ("stale", "old")

  write       id
  drop-stale  "stale"
  drop-old    "old"

notes.put  7 writes  repeatable
  same in every write   text="needs-triage"
  different             id: 7 values ("issue-101", "issue-102", "issue-105", "issue-106", ...)

  write      id
  label-101  "issue-101"
  label-102  "issue-102"
  label-105  "issue-105"
  label-106  "issue-106"
  label-110  "issue-110"
  label-111  "issue-111"
  label-112  "issue-112"

notes.append  3 writes  not repeatable
  different             id: 3 values ("issue-101-comments", "issue-105-comments", "issue-110-comments")
                        text: 3 values

  write        id                    text
  comment-101  "issue-101-comments"  "Thanks for the report. Can you add the version you are ..." (118 bytes)
  comment-105  "issue-105-comments"  "This looks like a duplicate of #88."
  comment-110  "issue-110-comments"  "Fixed on main. Please try the next release."

Notes
  ! 2 writes use a tool the server marks destructive: notes.delete.
  Tool classes come from the server's own hints.

If a reply is lost: a repeatable write is sent again (up to 3 attempts). Any other write waits for a person.
Sending: the first write alone, then up to 4 at a time. A failed write stops the rest.
Next: codemode apply triage.plan.json
```

When a write fails, `apply` stops and `status` says what happened to every
write and which command comes next.

```
status   stopped after 1 failure
         approved by jkomyno on 2026-11-11 14:08 UTC

applied 8 · failed 1 · no reply 0 · not sent 3

failed (1): the server answered with an error
  comment-101  notes.append
    upstream said no
    codemode cannot tell whether a failed write changed anything. Check the target, then run:
      codemode resolve triage.plan.json comment-101 --as not-applied   (or --as applied)
    Use --as skipped for a write that must not be sent.

not sent (3)
  Run `codemode apply triage.plan.json` to continue.
```

`mise run demo` runs the whole flow offline in about a second after the build.

## What it does

- **One approval for the exact list.** A plan has one SHA-256 digest over its
  writes, their order, the server's identity, the server file, and the
  definition of every tool it uses. On a terminal, `apply` asks and accepts
  only `yes`. Without a terminal it refuses unless `--approve` carries the
  full digest. It connects to the server only after the approval. An edited
  plan, another server file, a changed tool definition, or another server is
  refused before any write.
- **A plan a second person can review.** The plan file is stable, sorted JSON
  with one write per line. It holds no credential. It can go through a pull
  request like any other change.
- **The operator limits the tools.** `write_tools` in the server file names the
  tools a plan may use. `plan` refuses any other write. A plan names the
  server file it was made with by digest, and `apply --server <file>` accepts
  only that file.
- **A broken batch fails once.** The first write goes alone. After a failed
  write, no new write is sent. A rate limit (HTTP 429) pauses the session for
  the time the server asks.
- **No blind repeats.** Every attempt is written to a journal before it is
  sent. After a lost reply or a killed process, a write the server marks
  idempotent is sent again. Every other uncertain write waits for a person,
  and `resolve` records what the person found. A write that can never
  succeed, for example because its target was deleted, is recorded
  `--as skipped`. It is never sent, and the plan can finish without it.
- **Hostile text stays visible.** Control, bidirectional, and zero-width
  characters from a plan or a server are printed as escapes, so a plan cannot
  hide or fake a line of the review.

## What it does not do

- **It does not check a write by reading it back.** MCP gives a client no way
  to ask whether an earlier call took effect, so an uncertain write that is
  not repeatable needs a person.
- **No exactly-once and no rollback.** A write that was applied stays applied.
- **Approval is not authentication.** The approver's name is recorded as
  given. A process that can run `codemode apply --approve` and can read the
  server's credential can approve its own plan. If the agent must not be able
  to do that, keep the credential away from it. Review the plan file in a pull
  request and run `apply` where the credential lives.
- **One server per plan.** No OAuth flow. Give a token in a header.
- **It is not for one or two writes.** Your agent's own permission prompt
  handles those.

## Status

Tested offline against a stand-in MCP server, on Streamable HTTP and stdio,
with protocol revision 2026-07-28 and the older `initialize` handshake. The
tests cover a killed `apply`, lost replies, tool errors, rate limits, and the
review of a plan with 10,000 writes.

Live, the five commands ran one plan of 9 writes through a Composio Tool
Router session on 2026-11-11: one Linear ticket, and a label and a comment on
each of four GitHub issues. Seven writes were applied. Two failed because
their issue had been deleted on purpose after `plan`, and `resolve --as
skipped` closed the plan. The [guide](docs/mcp.md#composio-tool-router)
records the run. A lost reply, a rate limit, and a killed process are covered
by the offline tests only.

That commit also holds what this repository used to carry and no longer does:
a Composio CLI host, Pi and OpenCode adapters, and a comparison benchmark.

## Build

```sh
mise trust && mise install
mise exec -- cargo build --release --locked   # target/release/codemode
mise run check                                 # fmt, clippy, tests, docs
mise run demo                                  # the whole flow, offline
```

The minimum supported Rust is 1.91. The build downloads a prebuilt V8 archive
on first use, because `plan --program` runs TypeScript in an embedded V8
worker.

## Read more

- [Guide](docs/mcp.md): the server file, tool classes, every command, exit
  codes, and what happens to each kind of answer.
- [How codemode works](docs/design.md): the digest, the journal, the worker,
  and the limits.
- [Strategy](STRATEGY.md) and [contributing](CONTRIBUTING.md).

## License

[MIT](LICENSE). Vendored skills retain their notices in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
