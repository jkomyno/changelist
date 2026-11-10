# GitHub triage through the Composio CLI

This guide runs one workflow end to end: a program investigates a GitHub
repository with read-only tools, its proposal becomes a plan file, you review and
approve that exact plan, codemode executes it with a journal, you watch it from a
second terminal, interrupt it, resume it, and export a diagnosis.

The program and the plan stay separate. The program runs once, during
investigation, and can only read. The plan is data: it is what you review, what
approval binds, and what runs. The program is never run again and is not part of
the plan or its digest.

## Before you start

- Composio CLI 0.6.0 (the verified version), logged in with `composio login`.
- A GitHub connection: `composio link github`. Then find its connected-account
  id, which starts with `ca_`, in `composio connections list` under `github`.
  Use that id, never an alias or word id: aliases can be reassigned, and every
  command refuses anything but a `ca_` id before making a call. No project SDK
  key is needed.
- An installed bundle (see [installation](installation.md)). It provides
  `bin/codemode` and the `bin/codemode-composio` launcher. The installed
  `integrations/composio/` directory must stay writable by its owner, because
  `composio run` copies its entry script while it runs.
- A test repository you can triage, with the labels your plan will add or
  remove (the example uses `bug`) and an open issue or two.

```sh
INSTALL=$(cd "$HOME/.local/share/codemode/current" && pwd -P)
ACCOUNT=ca_...            # from `composio connections list`
REPO=you/triage-sandbox   # the only repository any command reads or writes
```

`codemode-composio` runs `composio run --file $INSTALL/integrations/composio/host.ts`
with the bundled binary. From a source checkout, run
`composio run --file integrations/composio/host.ts -- <command> ... --bin target/debug/codemode`
instead. Every command prints one JSON line.

## 1. Investigate

```sh
"$INSTALL/bin/codemode-composio" investigate --account "$ACCOUNT" --repo "$REPO" \
  --program "$INSTALL/integrations/composio/examples/triage.ts" \
  --out plan.json --trace investigate.json
```

[`examples/triage.ts`](../integrations/composio/examples/triage.ts) reads the open
issues, the repository's labels, and the latest failed workflow run. It proposes
`bug` and a findings comment for an unlabeled issue that mentions CI or a
failure, and a comment plus a close as `not_planned` for an issue whose title
repeats an earlier one. Write your own program the same way:

- The host defines `repository` (`{ owner, repo }` from `--repo`).
- Only GitHub reads are authorized: issues, labels, comments, issue events and
  timelines, workflow runs, jobs, job and run logs, and check runs (the list is
  `READ_TOOLS` in [`composio-provider.ts`](../integrations/composio/composio-provider.ts);
  types are in [`composio-tools.d.ts`](../integrations/composio/composio-tools.d.ts)).
  Rust refuses every other tool, including writes and `COMPOSIO_*` tools, with
  `tool_denied`. The host refuses a read whose `owner`/`repo` is not `--repo`
  with `tool_denied`, without calling Composio. The program has no `proxy`,
  sub-agent, environment, file, or network access.
- Every read runs as `--account`. Signed download URLs, such as a run-log
  archive's, are replaced before the program sees the result. A failed read's
  error carries a code and the GitHub status, never Composio's or GitHub's text.
- Return `{ operations: [{ id, tool, arguments, preconditions? }] }`. The tools
  are the four writes in [contracts](#contracts); a precondition is
  `{ "issue_state": "open" | "closed" }`.

The host then generates a random plan id, appends the marker line
`<!-- codemode:<plan id>:<operation id> -->` to every comment body, adds each
operation's scope, checks every operation against the contracts, writes
`plan.json`, and runs `codemode plan check` on it. The summary includes the
digest. An empty proposal writes no plan. The trace records each call's tool,
outcome, and result size, never result values.

Scopes bind identity: the account is `<connected account id>:<GitHub login>`
and the destination is `<owner>/<repo>:<repository id>`. If the connection later
acts as another login, or the name points at another repository, sessions refuse
before any write.

## 2. Review and approve

```sh
"$INSTALL/bin/codemode-composio" declare --account "$ACCOUNT" --repo "$REPO" > declaration.json
"$INSTALL/bin/codemode" plan check plan.json --provider declaration.json
```

`plan check` prints every operation exactly as approval binds it: tool, scope,
canonical arguments including each comment's full body and marker, preconditions,
each tool's recovery contract, retention, and the digest. Any edit to the file,
including to a marker, changes the digest.

Optionally confirm against GitHub now, read-only:

```sh
"$INSTALL/bin/codemode-composio" preflight --account "$ACCOUNT" --repo "$REPO" --plan plan.json
```

It reports, per operation, a contract refusal (for example a comment without its
marker, an issue update with a `labels` field, or a destination outside `--repo`),
whether the session would be authorized, whether each label exists, whether each
precondition holds, and whether the change already holds. When authorization is
refused (for example `account_ambiguous` or `repository_changed`), it reports
only that code for the operations in that scope and reads nothing more as that
account.

Approve with the digest you reviewed:

```sh
"$INSTALL/bin/codemode" plan approve plan.json --journal triage.db \
  --provider declaration.json --digest sha256:... --approver "$USER"
```

`--concurrency`, `--max-attempts`, and `--max-probes` are part of the approval.
Operations run in no fixed order; give an operation a precondition only if no
other operation in the plan can change it.

## 3. Run, watch, interrupt

```sh
"$INSTALL/bin/codemode-composio" run --account "$ACCOUNT" --repo "$REPO" \
  --declaration declaration.json --journal triage.db --deadline-ms 300000
```

The session first re-checks the connection (`composio connections list` must show
the id as active and as nobody else's alias), the GitHub login, the repository id,
and triage permission. For each attempt, Rust records the intent before the host
sees it. The host checks the operation against its contract, reads the issue if
the operation has a precondition, confirms an added label exists, then sends the
write once.

From a second terminal, at any time:

```sh
"$INSTALL/bin/codemode" plan inspect --journal triage.db
```

Inspection reads only committed evidence and writes nothing. An open session
means "no end recorded", not proof that an executor is alive. A second `run` or
`resume` on the same journal is refused while one holds it.

Interrupt with Ctrl-C, or send SIGTERM to `codemode-composio`: the launcher
forwards it to the CLI, waits for the host to end the session, and exits with
its status. A write already handed to the CLI cannot be aborted: its
effect may still land, so it is recorded as unknown, never as failed. Exit
status: 0 every operation is done, 3 work remains, 4 the session was refused,
1 the session ended without an outcome, 2 a usage or startup error.

## 4. Resume and review

```sh
"$INSTALL/bin/codemode-composio" resume --account "$ACCOUNT" --repo "$REPO" \
  --declaration declaration.json --journal triage.db
```

Resume probes each unknown operation by its contract before anything else, and
continues the work that is safe to continue. No write is sent twice
automatically. An operation that stays unknown after its probes, or that was
refused, waits for a person:

```sh
"$INSTALL/bin/codemode" plan review --journal triage.db --operation comment-1 \
  --as applied --reviewer "$USER" --note "comment is on the issue"
```

Always resume the original journal. A new journal for a plan that already ran
starts from nothing: it would send every comment again, because comments are not
searched for their marker before their first attempt.

## 5. Export a diagnosis

```sh
"$INSTALL/bin/codemode" plan inspect --journal triage.db --json > diagnosis.json
```

The diagnosis names each operation as applied, satisfied, not applied, unknown,
or refused, with its attempts, probes, reviews, and what may safely happen next.
It omits argument values. `--messages` adds the host's fixed error messages and
probe evidence. `codemode plan trace --journal triage.db --session N` prints one
session's execution record.

## Contracts

Every write is `repeat: unsafe`. Retention is `summary/1`: the journal keeps the
host's own fixed messages and evidence, never Composio's or GitHub's text.

| Tool | Allowed arguments | After a lost reply |
| --- | --- | --- |
| `GITHUB_ADD_LABELS_TO_AN_ISSUE` | `owner`, `repo`, `issue_number`, `labels` with one existing label | Label on the issue: `satisfied`. Absent: unknown. |
| `GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE` | `owner`, `repo`, `issue_number`, `name` | Label absent: `satisfied`. Present: unknown. |
| `GITHUB_UPDATE_AN_ISSUE` | `owner`, `repo`, `issue_number`, `state`, optional `state_reason` | State (and reason) match: `satisfied`. Otherwise unknown. |
| `GITHUB_CREATE_AN_ISSUE_COMMENT` | `owner`, `repo`, `issue_number`, `body` ending with its marker | A comment ending with the approved marker (plan id and operation id), by the approved login: `applied`. By another user, or ending with this operation id under another plan id: recorded as evidence, still unknown. None: a probe error, still unknown. |

`satisfied` says the state holds, not who caused it. Absence never proves that
a write did not happen: an earlier request may still land. A probe that cannot
read everything (a failure, an unexpected shape, more than 1,000 labels or
comments) is a probe error. A refused operation (`precondition_failed`,
`label_missing`, a contract violation) sent nothing.

Denied: `GITHUB_SET_LABELS_FOR_AN_ISSUE` (it replaces every label), any other
`GITHUB_UPDATE_AN_ISSUE` field, and every other tool, including `COMPOSIO_*`
meta tools. A result marked `dryRun: true` (from `composio run --dry-run`) means
nothing was sent: the operation is recorded as not applied and the session ends.

## What is stored, and where

The journal (`triage.db`, with `-wal`, `-shm`, and `.lock` beside it) keeps the
approved plan, because recovery needs it for the journal's lifetime: every
argument including full comment bodies, issue numbers, labels, the repository and
its id, and the connected-account id with its GitHub login. It also keeps
approver and reviewer names and notes, attempt and probe records, error codes,
and the host's fixed messages and evidence. It never keeps result values,
Composio or GitHub text, or the CLI credential, which stays inside `composio run`.
Deleting the journal files removes all of it.

The investigation trace keeps the program source, call arguments, outcomes,
error codes, and sizes. `plan.json` holds the proposal itself.

The Composio CLI writes each call's arguments, comment bodies included, to
plaintext files in a `composio-run-artifacts-*` directory under `$TMPDIR`, with a
`run.log`, and does not remove them. CLI 0.6.0 honors `TMPDIR`, so the launcher
points it at a private directory that it removes when the command ends. Running
`composio run` yourself leaves those files in your `$TMPDIR`.

## Limits

- Preconditions and the label existence check are separate reads before the
  write: GitHub offers no conditional write, so the issue can change in between.
- There is no exactly-once delivery and no receipt from Composio. Comments are
  reconciled by their marker; GitHub renders the marker as nothing, but it stays
  in the comment's source.
- The CLI helper cannot cancel a running call, and it sends no tool version: the
  version shown by `--get-schema` is evidence, not a pin. Outside a session,
  `composio connections list` is given 30 s, and resolving the login and
  repository, preflight authorization, and each preflight operation's reads are
  given 60 s each; past that the command exits 2 with an error naming the step.
- When an organization enables Composio's permission controls, the CLI asks
  inside the call, within the session deadline. This was not exercised.
- Verified with Composio CLI 0.6.0 on macOS. See the
  [live validation record](composio-live-validation.md) for what ran against
  GitHub.
