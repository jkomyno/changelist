---
name: codemode
last_updated: 2026-11-11
---

# codemode strategy

Based on [GOAL.md](GOAL.md), whose original brief is kept for history.

## Purpose

A person who lets an agent make many writes through an MCP server needs two
things. Before the writes, one review of the exact changes that is short enough
to read. After a failure, a plain answer to which writes happened, which did
not, and which nobody can tell.

## Positioning

codemode is a command-line tool that sits between an agent and one MCP server
for batches of writes. The agent, or a script, produces a plan file. A person
approves that file once. codemode sends exactly that file and keeps a journal.

It differs from the two common designs in one way each.

- An agent host that asks before every call gives 200 prompts for 200 writes.
  codemode gives one review that groups the writes by what is the same and what
  differs.
- A code-execution tool that asks before running a script approves the program.
  codemode approves the effects. The program only reads, and the list it returns
  is data.

It does not replace a gateway, a permission system, or the server's own access
control. It works with the credentials the operator already has.

## Boundaries

- One workflow, five commands. A new command or flag needs a named user.
- One server per plan. No tool catalog, no authentication flows, no policy
  language, no proxy mode, no UI.
- A plan is a finite list of independent writes. No dependencies between
  writes, no control flow, and no replay of the program that proposed them.
- No exactly-once claim and no rollback. The states are applied, failed, no
  reply, refused, resolved by a person, and not sent.
- Approval is a deliberate act by a person, not a security boundary against a
  process that holds the same credentials. The docs say so.
- Credentials stay outside the program worker, the plan file, and the journal.
  The worker's restrictions are not OS isolation.
- Unreleased formats get no compatibility.
- Offline and live evidence are reported separately.

## What decides whether this is useful

- **Review time.** Can a person say what a 200-write plan will do after
  reading one screen? The review must hide nothing silently.
- **Wrong approvals.** Can an edited plan, a changed tool, or another server
  be sent under an old approval? The answer must stay no.
- **Duplicate effects.** After a lost reply or a killed process, is any write
  that is not repeatable sent twice? The answer must stay no.
- **Damage from a broken batch.** How many writes are sent before the session
  stops when every write fails? Today it is one.
- **Steps to recover.** After a failure, how many commands does a person need
  before the plan is finished, and does `status` name each of them?

## Next, in this order

1. **Run the five commands against a live server.** The live check on record
   used the earlier commands at commit `9be2351`.
2. **A verification read for an uncertain write.** After a lost reply, the
   operator could name the read that shows whether a write took effect. This
   would settle most writes that now wait for a person. It needs a design for
   how the operator declares that read, and it is not started.
3. **Idempotency keys**, when MCP or a server offers a way to send one.

## Not planned

Host adapters, a benchmark against other code-execution tools, a general
workflow engine, and a hosted service. The first two existed and were removed
on 2026-11-11 because they served a comparison, not this workflow.
