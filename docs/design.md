# A small Rust execution core

The design should fit a **30-minute technical explanation and demonstration**.
Its three concepts are run-level control, rolling scheduling, and execution
evidence. A chosen guest backend and provider adapter attach to that core.

```mermaid
flowchart LR
    Agent[Pi / OpenCode / Codex / Composio CLI] --> CLI[Rust library or CLI]
    CLI --> Guest[Restricted TS/JS guest]
    Guest -->|untrusted call request| Control[Rust admission and scheduler]
    Control --> Provider[Trusted provider: mock, or a parent host via codemode bridge]
    Provider -->|observed result| Control
    Control -->|bounded reply| Guest
    Control --> Trace[Host-written execution record]
```

## Run-level control

One host-owned deadline, call allowance, concurrency cap, cancellation signal,
and serialized-output allowance govern the run. Guest code cannot enlarge them.
Provider calls receive the shared deadline and cancellation context.

A stop request cannot reverse an external side effect. Guest termination also
does not automatically make a still-observable provider result unknown. Collect
already-dispatched results while the run budget allows; prevent queued work from
starting after the guest finishes. A provider future that loses observation is
recorded as unknown. This prototype offers no recovery guarantee after the Rust
host itself crashes.

## Rolling scheduling

The guest program expresses dependencies with ordinary awaits and independent
work with promises. Rust starts a queued call when an active slot becomes free.
There is no separate dependency-graph planner.

`RunConfig::dispatch` (`--dispatch rolling|batch`) selects the policy; rolling
is the default. The batch policy is a baseline: it starts up to the cap only when
no call is active and waits for the whole batch, as chunked parallel execution
does. Calls the guest submits in one synchronous turn reach Rust in one write, so
they share a batch. A sequential baseline is either policy with a cap of 1. The
report records the policy and limits it ran under.

Compare the same workload and concurrency cap under batch and rolling policies
before claiming an improvement. Mixed-latency calls can expose idle slots at a
batch barrier. Equal-duration calls, provider rate limits, startup cost, or very
small workflows can remove the advantage. That is an algorithmic hypothesis,
not an inherent Rust speed claim; [the evaluation](evaluation.md) measures it.

## Execution evidence

A versioned JSON record (schema version 2) contains source, host-assigned call
identities, arguments, observed results or errors, event ordering, timing, final
output, and run status. Queued, dispatched, completed, rejected, and unknown work
remain separate. The CLI can keep the full record in a file while returning
compact status and output to its caller.

Every time in the record comes from one clock: Rust's monotonic clock, in whole
milliseconds since run start. Each call records when it was admitted, dispatched,
finished, and when its reply was completely written to the guest's input, so
queue waiting, provider time, and delivery are separate. Run-level events mark the
guest's spawn, its finish (with output, an error, or by exiting), a deadline or
cancellation observed by Rust, and guest termination. An `unknown` or
`cancelled_before_dispatch` event names the run status that caused it. Byte
counts cover arguments, observed values, reply frames, bytes exchanged with the
guest and with a bridge host, and observed values omitted as too large.

Provider-reported facts are separate from these observations: a provider such
as a bridge host may say it received a call, waited for permission, decided, or
entered its executor. Rust stamps each fact with its arrival time and keeps the
provider's own timestamp apart. A missing fact never proves a call did not run.

`codemode inspect <trace.json>` and `codemode::inspect` answer three questions
from a record, within a call limit and with counted omissions:

- **Why was this slow?** Exclusive phases that sum to the elapsed time: startup,
  guest-only time, tools running, waiting at the concurrency cap, waiting while a
  slot was free (a batch barrier), draining after the guest finished, and
  shutdown. Guest-only time is not split further: the record cannot tell guest
  computation from the gap between dependent steps, and inspection does not infer
  dependencies from call order.
- **Which calls ran?** Each call's evidence: result observed, provider error
  (after executor entry, after a permission denial, or unspecified), rejected or
  never dispatched, or unknown after executor entry, after receipt only, or
  without acknowledgment.
- **Which outcomes require reconciliation?** The unknown calls, with the status
  that ended them and the last provider fact, to check with the provider before
  any retry.

Inspection prints tool names, error codes, top-level argument keys, and sizes. It
leaves out argument values, results, and messages unless `--messages` is given,
and replaces control characters.

The JSON `output` field is present only when a guest value was observed within
limits, including an explicit JSON `null`. A provider task panic has its own run
status and call event; any unobserved external effect remains unknown.

Traces can contain private code or task data. An offline reproducer needs
explicit reviewed fixture values; ordinary summaries do not reconstruct omitted
data. Replay of live effects, opaque result handles, durable journals, billing,
provider catalogs, a UI, and persistent notebook state are deferred.

## Presentation shape

- **5 minutes:** the problem, one composed task, and the host/guest/provider boundary.
- **10 minutes:** Rust ownership of admission, bounded scheduling, and cancellation.
- **10 minutes:** run the task, expose a controlled failure, and inspect its trace.
- **5 minutes:** measured tradeoffs, limits of the evidence, and host-adapter scope.

Use public synthetic fixtures for the demonstration. A live provider can later
establish integration separately. Keep private backend code, data, event names,
and presentation logistics out of public examples.
