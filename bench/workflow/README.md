# GitHub triage workflow comparison

This benchmark runs one GitHub issue-triage workflow through four implementations, against one
offline GitHub provider. Each run is scored against the provider's own ledger, not against
anything an implementation reports.

| Name | What runs | Recovery after interruption |
| --- | --- | --- |
| `codemode` | this repository: investigate with the triage program in `codemode bridge` (reads only), write the proposal as a plan, `plan check`, approve the printed digest, `plan run --bridge`; later attempts `plan resume --bridge` the same journal | journal plus provider contracts |
| `pi-durable` | `@earendil-works/pi-durable` 1.5.0, a durable code mode run over SQLite, driven by pi-ai's faux model | Pi resumes the durable run; interrupted writes are not rerun |
| `opencode` | `@opencode/codemode` 2.2.6 as a library, released interpreter | none; a restart reruns the program from scratch |
| `composio-script` | [`engines/composio-script.ts`](engines/composio-script.ts), the competent script defined below, against a Composio-shaped `execute` helper | its own fsynced progress file |

Pins, tarball integrity, and source commits are in [`pins.json`](pins.json) and are copied into
each result's `environment.json`. Cloudflare's `@cloudflare/codemode` 0.5.6 is documented there
but not run (it needs workerd and a Durable Object facet).

This README, including the hypotheses and protocol, was written before the first comparison
run; `environment.json` records this file's SHA-256 for each results directory.

## Reproduce

```sh
mise exec -- bench/workflow/setup.sh     # pinned packages into bench/workflow/.cache, build codemode
mise run workflow-bench                  # full protocol into bench/workflow/results/<date>/
mise exec -- node --test bench/workflow/oracle.test.ts   # the oracle's own checks
```

`node bench/workflow/run.ts --help` lists options for running a subset (`--scenarios`,
`--engines`, `--reps`).

## The workflow

The offline repository `bench/widgets` has seven issues (one closed). The shared triage program
([`programs/triage-core.js`](programs/triage-core.js), plain JavaScript so all three engines run
it unchanged) lists open issues and, per issue:

- a `duplicate` issue is closed (`GITHUB_UPDATE_AN_ISSUE`, `state: closed`);
- a `bug` gets the `triaged` label (`GITHUB_ADD_LABELS_TO_AN_ISSUE`), loses `needs-triage`
  (`GITHUB_REMOVE_A_LABEL_FROM_AN_ISSUE`), and gets one comment
  (`GITHUB_CREATE_AN_ISSUE_COMMENT`) unless its comments already contain the triage text.

That is eleven writes: three label additions, three label removals, three comments, two closes.
Each comment's last line is a marker naming the implementation and operation.

The program is convergent: every write is derived from the state it reads, so a rerun proposes
only what is missing. Pi and OpenCode run it and write directly; codemode runs it as the
investigation, and its return value becomes the plan; the script makes the same decision in
TypeScript.

### The provider

[`provider.ts`](provider.ts) is a separate process that outlives every engine crash. It serves a
Composio-shaped `execute(slug, arguments, account)` over HTTP on 127.0.0.1 with a fixed 20 ms
latency per request, and has no idempotency keys, because Composio's GitHub tools have none.
It writes an append-only ledger (one JSON line per event, fsynced): `start`, `effect_applied`,
`held`, `terminal` (with `delivered`), and `control` (a driver action). Repository state is
the fixture plus every `effect_applied` record; the oracle re-derives it from the file.

Faults: a lost response (effect applied, no reply), a held effect released later by the driver,
a pause that holds requests before or after the first write so the driver can kill at a fixed
ledger position, a call that ignores cancellation and applies after a delay, a reply whose body
trickles slowly, a connection reset after the effect, a maintainer change after the first read,
and account revocation or a switch of the GitHub login the account resolves to.

### codemode's provider contracts

The bench plan host in [`engines/codemode.ts`](engines/codemode.ts) implements the KTD9
contracts against the provider: label additions, label removals, and state changes are
`repeat: unsafe, reconcile: state` (desired state holds: `satisfied`; otherwise `unknown`);
comments are `repeat: unsafe, reconcile: receipt` by marker, counted only when authored by the
login the account resolved to at approval (absent: a probe error, never `not_applied`). Every
operation carries an `issue_state: open` precondition, checked before each attempt. Each session
re-resolves the account's login and the repository id and refuses on drift. Approval policy is
codemode's default (concurrency 4, 3 attempts, 3 probes) unless the equal-limits row says
otherwise.

## The competent script (R28)

[`engines/composio-script.ts`](engines/composio-script.ts):

1. resolves its own GitHub login, reads the open issues and their comments, makes the same
   decision as the triage program, and appends the decision (every operation and the issue
   state it was decided on) to an fsynced progress file before any write;
2. before each write, reads the issue's current state: records the operation done without
   writing if the desired state already holds, including its own marked comment authored by its
   own login; otherwise skips the write if the issue's state changed since the decision;
3. appends `started` before and `done` after each write, fsynced; an error reply is recorded as
   `failed`, no reply at all (reset, timeout) as `unknown`;
4. continues after a failure; running it again resumes from the progress file and repeats step 2
   for each unfinished operation. A failed read stops it before any further write.

It cannot tell "never landed" from "not landed yet" when its marked comment is absent, so it
posts again. It does not re-verify which GitHub login its connected account resolves to after the
first run. `--blind-retry` (skip step 2) exists only to check that the oracle catches duplicates.

## Advantage hypotheses (fixed before the first run)

Each is reported as demonstrated, tied, lost, or unsupported, with the result files that decide
it. README, help, and the walkthrough may claim only demonstrated ones.

- **H1 vs the Composio script.**
  - **H1a, approval binding (R8).** A change list widened after review cannot run without a new
    approval in codemode, while the script runs it; and after the connected account switches to
    another GitHub login, codemode sends no write while the script does. Decided by `widen` and
    `account-switched`.
  - **H1b, contract-based reconciliation of a lost comment reply (R11).** codemode settles a
    lost or late comment without a duplicate in every run; the script duplicates when its
    marked comment is absent at recovery and lands later. Decided by `lost-response`,
    `late-effect`, `probe-budget`, `broken-transport`, `slow-reply`. Expected: a tie on
    `lost-response` and `broken-transport` (the marker is already there), a win on `late-effect`
    and `probe-budget`.
- **H2 vs Pi Durable.** Pi journals intent and observations itself, so journaling is not the
  claimed difference. Claimed: exact approval binding (a widened change list, a revoked or
  switched account, a changed issue state: codemode sends nothing unapproved; Pi has no approval
  step) and contract-only reconciliation (codemode settles an interrupted write as applied or
  satisfied from provider evidence, where Pi reports it "interrupted and may have partially run"
  and leaves it to the program and a person). Expected tie on duplicate safety, since Pi does not
  rerun interrupted writes.
- **H3 vs OpenCode Code Mode.** Any durable recovery after interruption. OpenCode keeps no state,
  so durable recovery is unsupported; its cells also report what a rerun of the convergent
  program does, which is not the engine's property.
- **H0, latency (expected loss).** On the no-fault workflow codemode is expected to be slower than
  all three: it starts a bridge, a V8 worker, and three CLI processes, fsyncs every journal
  record, and reads issue state before each write. Pi and OpenCode run in one process; the script
  is a single process too.

## Protocol (fixed before the first run)

- **Repetitions.** At least 15 timed runs per latency cell (plus 2 untimed warm-ups), at least 5
  runs per fault scenario cell, run in rounds that interleave implementations.
- **Latency boundary.** From spawning the implementation's process to the exit of its last
  attempt, after the final ledger state; startup, package loading, journal and progress-file
  persistence, codemode's investigation and approval steps, and Pi's faux model turns are all
  charged. Reported as p25, p50, p75, p95 in milliseconds. Startup is reported separately as the
  time from spawn to the provider's first `start` record.
- **Tie rule.** Two latency cells tie when their p50s are within 10 percent and their p25–p75
  ranges overlap; otherwise the faster p50 wins.
- **Rows.** `host-default` passes no limits, so each engine uses its own defaults (codemode's
  bridge: 5 s, 32 calls, concurrency 4, 256 MiB heap; plan sessions: 300 s; Pi and OpenCode: none;
  the script: none). `equal-limits` passes the same budget everywhere: a 10 s deadline per attempt,
  a 64 MiB heap, 64 calls, and concurrency 1 (the three programs await writes one at a time).
  Scenario-specific deadlines (1.5 s for the cancellation and slow-reply cells, 3 s for the CPU and
  memory cells) replace the 10 s deadline in that row only. Every scenario runs in both rows
  except `widen`; the rows also separate codemode's default concurrency of 4 from sequential
  dispatch. OpenCode has no memory limit, so its
  equal-limits heap is the driver's `node --max-old-space-size` on its process.
- **Kill points.** The driver SIGKILLs the implementation's process group when the provider
  reports the fault armed: at the first write's arrival before the provider records it (before
  the request reached GitHub), after the first write's reply (after the first effect), or while a
  write is held or its reply lost. Every restart counts as one recovery request.
- **Recovery policy.** After a kill, the driver reruns the same command (resume for codemode,
  Pi, and the script; a rerun from scratch for OpenCode), up to the scenario's limit. After a
  normal exit, it reruns only when the implementation reports that work remains which another run
  may finish (codemode: an operation whose next step in the journal is dispatch or reconcile; the
  script: a failed or unknown write, or its deadline), except where a scenario says it recovers
  once regardless. Pi's rerun waits on the same submission, so a run Pi already settled is not
  submitted again. Nobody performs a human review during a run.
- **Wall clock.** A harness kill at 60 s per attempt, 20 s for the CPU and memory cells, recorded
  as "unbounded under this configuration".
- **Memory.** Peak RSS of the implementation's process group, sampled every 200 ms with `ps`;
  it is a sample, not a hard ceiling. The provider is not included.
- **Machine.** Timed batches start only when the 1-minute load average is at most 4; otherwise
  the driver waits 30 s and retries, and records every load reading.
- **Install effort.** Commands to install and the size of what is installed, from `setup.sh`.

## Scenarios and oracles

Every run is judged from the ledger and the implementation's final claims, which each adapter
normalizes to per-operation `applied`, `satisfied`, `not_applied`, `refused`, `unknown`, `failed`,
or `pending`, plus whether it reports the whole workflow as done. codemode's claims are the
journal's conclusions (`plan inspect --json` after each session). For Pi and OpenCode, the
program's own per-operation outcome line is the claim: Pi's "interrupted and may have partially
run" is `unknown`, and so is a transport failure, which the shared tool wrapper (the same for
both) reports as "outcome unknown"; any other error is `failed`. When a deadline kills the
program before it reports, the engine's own record of the in-flight call (Pi lists nested calls
with a status; OpenCode lists names only) is kept verbatim and not mapped to a claim.

| Flag | Meaning |
| --- | --- |
| unjustified success | claims the workflow done, or an operation applied, when the ledger state does not hold |
| unjustified non-effect | claims `not_applied` or `refused` at any attempt for an operation the ledger shows applied |
| failed but applied | final claim `failed` (an error with no uncertainty) for an operation the ledger shows applied |
| unsafe duplicate | more than one effect for a non-repeatable operation (a comment) |
| redundant write | more than one effect for a label or state write (harmless to state; reported, not failed) |
| unsafe send | a write sent after the account was revoked or switched, or to an issue whose state changed after the decision |
| human review | the implementation ends with an operation it reports as uncertain |

A run is `wrong` if any flag except redundant write and human review is set; `correct` when the
final state matches the expectation, `correct-needs-review` if it also needs a person; and
`safe-incomplete` when it does not match but nothing was claimed or sent wrongly. In the
account scenarios `safe-incomplete` is the best possible verdict.

| Scenario | Fault and steps | Expected oracle |
| --- | --- | --- |
| `no-fault` | none; both rows; latency cells | all eleven writes, once each |
| `lost-response` | first comment applied, reply never sent; kill; recover | one comment per issue; all writes |
| `kill-before-first-write` | kill when the first write arrives, before the provider records it | all writes; nothing duplicated |
| `kill-after-first-write` | kill after the first write's reply | all writes; nothing duplicated |
| `late-effect` | first comment held; kill; recovery 1; release (effect lands); recovery 2. Inspection is tried from another process while the comment is held, and a diagnosis is exported after the kill | one comment per issue; a non-effect conclusion at recovery 1 is wrong |
| `scope-changed` | a maintainer closes issue 1 right after the first issue listing (between proposal and run) | no write to issue 1; the rest applied |
| `account-revoked` | first comment's reply lost; kill; account revoked; recover | no write after revocation; no success claim |
| `account-switched` | same, but the account now resolves to another GitHub login | no write after the switch |
| `ignore-cancel` | first label addition ignores cancellation and applies after 3 s; recover once after it lands (1.5 s deadline in equal limits) | whether the in-flight call is reported unknown; final state; no duplicate |
| `slow-reply` | first comment applied, reply body trickles over 3 s (1.5 s deadline in equal limits) | one comment per issue |
| `broken-transport` | first comment applied, then the connection is reset | one comment per issue; no `failed` claim for it at the end |
| `repeated-crash` | the first write of every attempt is held; kill; release; recover, up to 12 times | all writes; nothing duplicated; recovery requests counted; budgets reported |
| `probe-budget` | first comment held through four recoveries, released at the end | one comment per issue; codemode expected to stop at review after 3 probes |
| `widen` | the change list gains one write (close issue 2) after review; one run per implementation, host-default row | whether the extra write runs without re-approval |
| `cpu-loop`, `memory-growth` | non-terminating programs; no script cell | bounded by the engine (status, time to stop, peak RSS), killed by the harness, or the host crashed |

Unsupported cells are recorded with a reason and not scored as failures: the script has no guest
program for CPU and memory cells; OpenCode has no durable recovery (its rerun is reported
alongside); no implementation but codemode has an approval step.

## Files

- `provider.ts`, `lib/github-state.ts`: the provider, fixture, and ledger state.
- `engines/*.ts`: one adapter per implementation; `lib/adapter.ts` their shared options and claims.
- `programs/`: the triage program core and the CPU and memory programs.
- `scenarios.json`: scenario inputs, rows, and repetitions.
- `oracle.ts`, `oracle.test.ts`: scoring, and checks that it catches what it should.
- `run.ts`: the driver; `results/<date>/`: `runs.jsonl`, `summary.json`, `summary.md`,
  `environment.json`. Per-run ledgers and logs stay in `.cache/runs/` (not committed).
