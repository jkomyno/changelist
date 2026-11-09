# Recovery evaluation

An offline, reproducible comparison of what happens after an executor is killed
in the middle of an approved plan, and of what durable accounting costs when
nothing fails. It answers two questions:

1. From one identical crashed state, which recovery strategy reaches the intended
   final state without duplicate effects, what does it leave unresolved, and how
   long does it take?
2. With no faults, what does journaling every attempt add to a dispatch run, per
   operation and per commit, under each durability setting?

It makes no claim that Rust is faster than anything. Every strategy and mode runs
the same Rust runtime, the same Deno guest program shape, and the same fixture.

## Run it

```sh
cargo build --release --locked --bin codemode
cargo run --release --locked --bin codemode-recovery-eval -- --reps 15 --out eval/recovery \
    > eval/recovery/summary.md      # about 80 s
```

`codemode-recovery-eval` ([source](../src/bin/codemode-recovery-eval.rs)) needs the
release `codemode` binary next to it (or `--bin PATH`), because part A kills a real
`codemode plan run` process. It prints a Markdown summary and, with `--out`, writes
`summary.json`: every run's metrics, the aggregates, the configuration, and a build
fingerprint (commit, rustc, OS, CPU, Deno). It exits 1 if a check that should hold
fails and 2 on a setup error. `tests/recovery_eval.rs` runs one repetition at 0 ms
latency in `mise run check` (under a second of evaluation time).

## Method

### Matching parameters

| Parameter | Value, for every strategy and mode |
| --- | --- |
| Plan | [`examples/recovery/plan.json`](../examples/recovery/plan.json): 8 `add_label`, 2 `remove_label`, 1 `add_comment` (part B also uses a 64-operation `add_label` plan) |
| Concurrency | 4 operations in flight |
| Fixture latency | 0 ms and 25 ms per request, one profile at a time |
| Provider | the offline issue tracker in [`src/fixture.rs`](../src/fixture.rs), one SQLite file per run |
| Dispatch | the runtime's rolling dispatch, with a trusted program that names operations only and submits them together |
| Repetitions | 15 measured after 2 discarded warmups; strategies (A) and modes (B) run round-robin, and the starting one rotates every repetition |
| Statistics | nearest-rank percentiles; totals measured in µs and shown in ms; phases from the session's millisecond timings and the trace |

### A. How the crash is produced

For each repetition and latency profile, the harness creates a fresh fixture and
journal, approves the plan (concurrency 4, otherwise default policy, `storage`
durability), and arms four faults: lost responses after the effect commits for
`label-104`, `label-107`, and `comment-105`, and a held request (ledgered, never
applied) for `label-109`. It then starts the release `codemode plan run` as a
subprocess, polls the fixture ledger until the held request appears, and sends
SIGKILL (`Child::kill`). By then all four slots are stuck, so `label-110` was never
dispatched.

After the process is reaped, the harness copies each database and its write-ahead
log into three directories and checks the original: the journal must show
`label-101`, `-102`, `-103`, `-105`, `-106`, `-108` observed applied, the four faulted
operations unknown, and `label-110` pending; the ledger must hold 10 rows, one held.
Each strategy then recovers its own copy from that identical state:

- **Stop on uncertainty** reads the journal and assesses it. Any unknown operation
  stops it: it probes and sends nothing.
- **Blind retry** reads the journal once to learn which operations were not observed
  applied (the 4 unknown and the 1 pending), then sends all 5 again through the same
  runtime with a generated dispatcher and a plain provider that calls the fixture
  directly. It ignores recovery contracts, does no reconciliation, and writes no
  journal. Each request carries a fresh attempt key, so the fixture's receipts and
  fences do not deduplicate anything: this baseline has no idempotency keys. Reading
  the journal at all is generous; a baseline without durable accounting would know
  less.
- **Codemode resume** opens the journal with `storage` durability and calls
  `run_session` with `SessionKind::Resume` in process, against the copied fixture.

Metrics per strategy:

- *Final state correct*: `triage` on 101, 102, 104, 105, 106, 108, 109, 110;
  `needs-info` absent on 103 and 107; exactly one comment, on 105, with the planned
  body; no duplicate effect in the ledger.
- *Claims complete*: whether the strategy itself considers the plan finished.
- *Unresolved ops*, from the strategy's own knowledge. Stop: operations unknown or
  pending. Blind retry: operations it re-sent while an earlier attempt's outcome was
  unknown, since it cannot vouch that their effect happened once, plus any re-send
  not observed succeeding. Codemode: operations whose next step is not `done`.
- *Requests sent*: ledger rows added by the strategy.
- *Duplicate effects*: effects beyond the first for one logical change (same tool,
  issue, and label or body), over the whole ledger.
- *Repeats without effect*: requests beyond the first for one logical change that
  added no effect.
- *Recovery time*: wall time from the strategy's start (before opening the journal)
  to done, including closing the journal.
- *Guest startup*: `guest_spawned` to the first `admitted` event in the dispatch
  trace. *Dispatch execution*: first `admitted` to the end of the run.

Hard checks (exit 1): the crashed state above; codemode resume ends `needs_review`
with exactly `comment-105` open, a correct final state, and no duplicate; stop sends
no request. Blind retry's duplicate and stop's incomplete state are expected and
recorded, not failed.

### B. Persistence overhead

Each run gets a fresh fixture with no faults, and runs the whole plan once, in one
of three modes:

- **Runtime only**: `codemode::run` with the same dispatcher and plain fixture
  provider as blind retry; no journal.
- **Journal, storage**: `run_session` with `Durability::Storage`
  (`synchronous=FULL`, and `F_FULLFSYNC` on macOS).
- **Journal, process**: `run_session` with `Durability::Process`
  (`synchronous=NORMAL`).

The journal is created and approved before the clock starts. *Total* is journal
open plus the session (or the run). *Commits* are counted from the journal after
the run: session begin and end, plus an intent and an observation per attempt.
The journal cost is paired by repetition: each mode's total minus the runtime-only
total of the same repetition, workload, and latency. The tables give the median of
those differences, divided by operations and by commits.

## Results

Apple M1 Pro (10 logical CPUs), macOS, Deno 2.9.8, rustc 1.99.1, release build.
Measured code: commit `ee34e8b`; the evaluation harness itself was not yet committed
and the library was unchanged. Full output:
[`eval/recovery/summary.md`](../eval/recovery/summary.md); every run:
[`eval/recovery/summary.json`](../eval/recovery/summary.json).

### A. Correctness after the kill

Identical in all 15 runs of every cell, at both latencies:

| Strategy | Final state correct | Claims complete | Unresolved ops | Requests sent | Duplicate effects | Repeats without effect |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| stop on uncertainty | 0/15 | 0/15 | 5 | 0 | 0 | 0 |
| blind retry | 0/15 | 15/15 | 4 | 5 | 1 | 3 |
| codemode resume | 15/15 | 0/15 | 1 | 2 | 0 | 1 |

- Stop leaves `label-109` and `label-110` unapplied and five operations it cannot
  account for.
- Blind retry posts the comment on 105 a second time in every run, while reporting
  success for every request. Its three harmless repeats are `label-104` and
  `label-107` (no change) and `label-109` (held, then applied).
- Codemode resume probed 3 operations (receipt for 104: applied; state for 107:
  satisfied; receipt for 109: not applied, and its old key fenced), dispatched 109
  and 110, and left `comment-105` for review, because the provider declares no
  reconciliation for comments and repeating one is unsafe. Its one repeat is the
  re-send of the held `label-109`.

### A. Recovery time (ms)

| Latency | Strategy | min | p50 | p90 | max |
| ---: | --- | ---: | ---: | ---: | ---: |
| 0 ms | stop on uncertainty | 2.4 | 2.7 | 3.3 | 6.5 |
| 0 ms | blind retry | 31.1 | 33.6 | 38.4 | 41.3 |
| 0 ms | codemode resume | 89.9 | 93.8 | 98.8 | 100.1 |
| 25 ms | stop on uncertainty | 2.8 | 3.1 | 3.3 | 3.4 |
| 25 ms | blind retry | 83.7 | 86.8 | 87.5 | 88.8 |
| 25 ms | codemode resume | 112.1 | 120.0 | 123.2 | 124.0 |

Phases, p50 in ms (startup columns first):

| Latency | Strategy | Journal open/read | Checks | Reconcile | Guest startup | Dispatch execution | Dispatch phase | Total |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 ms | stop on uncertainty | 2.7 | — | — | — | — | — | 2.7 |
| 0 ms | blind retry | 2.7 | — | — | 22 | 7 | 29 | 33.6 |
| 0 ms | codemode resume | 2.2 | 9 | 19 | 22 | 27 | 49 | 93.8 |
| 25 ms | stop on uncertainty | 3.1 | — | — | — | — | — | 3.1 |
| 25 ms | blind retry | 2.9 | — | — | 25 | 58 | 82 | 86.8 |
| 25 ms | codemode resume | 1.8 | 10 | 20 | 24 | 50 | 74 | 120.0 |

### B. No-fault runs (ms)

| Workload | Latency | Mode | Total p50 / p90 | Dispatch p50 / p90 | Journal open p50 | Checks p50 | Guest startup p50 | Execution p50 / p90 | Commits |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 11 ops | 0 ms | runtime only | 46.2 / 52.9 | 45 / 52 | — | — | 22 | 14 / 25 | — |
| 11 ops | 0 ms | journal, storage | 183.4 / 198.5 | 157 / 168 | 0.6 | 12 | 24 | 131 / 137 | 24 |
| 11 ops | 0 ms | journal, process | 45.3 / 60.9 | 42 / 57 | 0.4 | 1 | 26 | 13 / 29 | 24 |
| 11 ops | 25 ms | runtime only | 113.5 / 115.5 | 112 / 114 | — | — | 24 | 88 / 89 | — |
| 11 ops | 25 ms | journal, storage | 202.0 / 214.3 | 175 / 186 | 0.6 | 12 | 24 | 150 / 161 | 24 |
| 11 ops | 25 ms | journal, process | 116.9 / 121.1 | 114 / 118 | 0.3 | 0 | 25 | 89 / 90 | 24 |
| 64 ops | 0 ms | runtime only | 52.1 / 67.0 | 51 / 66 | — | — | 22 | 29 / 44 | — |
| 64 ops | 0 ms | journal, storage | 865.6 / 884.2 | 837 / 849 | 0.7 | 12 | 24 | 814 / 826 | 130 |
| 64 ops | 0 ms | journal, process | 63.0 / 75.1 | 59 / 71 | 0.6 | 1 | 27 | 28 / 45 | 130 |
| 64 ops | 25 ms | runtime only | 490.5 / 505.1 | 489 / 504 | — | — | 28 | 462 / 474 | — |
| 64 ops | 25 ms | journal, storage | 864.8 / 921.9 | 829 / 883 | 0.8 | 13 | 31 | 797 / 843 | 130 |
| 64 ops | 25 ms | journal, process | 499.6 / 522.2 | 495 / 517 | 0.6 | 1 | 29 | 464 / 490 | 130 |

Journal cost against the runtime alone, paired by repetition:

| Workload | Latency | Durability | Δ total p50 / p90 (ms) | Per op (ms) | Per commit (ms) | Δ execution p50 / p90 (ms) |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 11 ops | 0 ms | storage | 141.4 / 164.0 | 12.85 | 5.89 | 118 / 124 |
| 11 ops | 0 ms | process | 6.9 / 16.4 | 0.63 | 0.29 | 0 / 13 |
| 11 ops | 25 ms | storage | 91.3 / 102.5 | 8.30 | 3.80 | 63 / 72 |
| 11 ops | 25 ms | process | 4.5 / 7.6 | 0.41 | 0.19 | 1 / 2 |
| 64 ops | 0 ms | storage | 813.2 / 829.2 | 12.71 | 6.26 | 780 / 794 |
| 64 ops | 0 ms | process | 8.4 / 28.9 | 0.13 | 0.06 | −1 / 21 |
| 64 ops | 25 ms | storage | 372.8 / 445.5 | 5.83 | 2.87 | 331 / 390 |
| 64 ops | 25 ms | process | 11.8 / 25.9 | 0.18 | 0.09 | 3 / 25 |

All 90 recovery runs and 180 no-fault runs passed their checks.

## What the measurements show

- **Only codemode resume reached the intended state without a duplicate.** It did
  so by sending 2 requests where blind retry sent 5, and by stopping short of the
  one operation it could not establish. That leaves work for a person: the comment
  did apply, but nothing the provider offers can show it, so codemode reports
  `needs_review` rather than `completed`.
- **Blind retry is faster and wrong in a way it cannot see.** It finished 60 ms
  sooner at 0 ms latency and 33 ms sooner at 25 ms, claimed success in every run, and
  duplicated the comment in every run. Label operations survived repetition only
  because the fixture's labels are a set.
- **Stop is cheap and incomplete.** It took about 3 ms and changed nothing, which is
  safe for the comment and leaves two operations that could safely have been sent.
- **Codemode resume is the slowest strategy here (a regression against blind
  retry).** At 0 ms latency, its 94 ms split into journal open (2 ms), checks (9 ms:
  a durable session-start commit and authorization), reconciliation (19 ms: three
  probes, each recorded with a storage-durable commit), and dispatch (49 ms, of which
  22 ms was guest startup). The rest is assessment and the durable session end.
  At about 6 ms per storage commit (part B), the session-start commit, three probe
  records, two intents, two observations, and the session end account for most of
  the gap; each fixture probe is a single small SQLite transaction.
- **Guest startup dominates small batches.** Starting the Deno guest took 22–31 ms
  in every dispatch run, two thirds of blind retry's recovery at 0 ms and close to
  half of the 11-operation runtime-only run.
- **Storage durability costs about 6 ms per commit on this machine.** Without
  latency, the 11-operation plan took 183 ms instead of 46 ms (24 commits), and the
  64-operation plan 866 ms instead of 52 ms (130 commits). Commits go through one
  connection in order, so with `storage` the journal, not the provider, limits
  throughput: the 64-operation run took about 865 ms at both 0 and 25 ms latency.
  With latency, some commit time overlaps calls that are waiting, so the measured
  cost per commit falls to 2.9–3.8 ms.
- **Process durability costs little here.** The paired median was 4.5–11.8 ms per
  run (at most 0.63 ms per operation), within the run-to-run spread: paired p90
  reached 29 ms, and execution deltas were 3 ms or less at p50.
- **Startup is separate from execution.** Opening and locking the journal took under
  1 ms. The opening checks took 12 ms with `storage` and 0–1 ms with `process`,
  mostly the durable session-start commit.

## Limits

- One machine (Apple M1 Pro, macOS), one Deno version, 15 repetitions.
  Timestamps in the phase columns are whole milliseconds.
- An offline fixture with synthetic, fixed latency. Real providers add variable
  latency, rate limits, and failures this harness does not model.
- One crash shape: a process killed with SIGKILL while four requests pend. Other
  interleavings (a kill between intent and request, during reconciliation, or during
  the journal's own commit) are not measured here.
- Process crashes only. `storage` asks the OS to flush (on macOS with
  `F_FULLFSYNC`); whether that survives power loss depends on the device and is not
  tested. These numbers measure the cost of the settings, not power-loss behavior.
  The fixture uses `synchronous=FULL` without `fullfsync` for its own commits in
  every mode.
- `applied` and `not_applied` are decidable only because the fixture keeps
  receipts per attempt key and fences keys it reports as not applied. A provider
  without receipts or fences leaves those operations for review, as the comment
  was here.
- Blind retry is one baseline, without idempotency keys. A retry that reused stable
  idempotency keys with a provider that honors them would avoid the duplicate
  comment; it would still need a way to know which operations to send.
- The 64-operation plan has only label additions, which are repeat-safe and have
  receipts; it measures journaling cost, not recovery.
