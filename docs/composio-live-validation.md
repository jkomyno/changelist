# Live Composio checks

## 2026-11-10: four writes, a lost reply, a killed executor, and resume

Composio CLI 0.6.0, the installed bundle of commit `84e2812` (launcher
`bin/codemode-composio`), one active GitHub connection, repository
`jkomyno/codemode`. The owner authorized this exact run beforehand: create one
dedicated issue and run one four-operation plan on it. The dropped reply used the
uninstalled `integrations/composio/testing/host.ts` from the same commit.

1. A plain `composio execute` created issue
   [#3](https://github.com/jkomyno/codemode/issues/3), `[validation] codemode U10
   live write and recovery`. This was the only write outside the plan.
2. `investigate` wrote the plan from a read-only program: add the existing
   `documentation` label, post one marked comment, remove the label, close the
   issue as completed; each operation required the issue to be open. `plan check`
   showed every canonical argument, the scope `<account>:jkomyno` on
   `jkomyno/codemode:1449378892`, the four contracts, retention `summary/1`, and
   digest `sha256:93f2a76a4cbe5f401ea92ebf5e9e6a72311fd4f0c64c3073ff737923f81ed66c`.
   `preflight` reported ready. The plan was approved with that digest at
   concurrency 1, one attempt per operation, and two probes.
3. `run` applied the label and posted the comment; the testing host withheld the
   comment's observed reply. While the session waited, `plan inspect` in another
   process showed `label-doc` applied, `comment-1` unknown with one attempt, the
   last two operations pending, and the session open with the note that this does
   not show an executor is running. The Rust executor was then SIGKILLed.
4. `codemode-composio resume` probed `comment-1` by receipt: it found the marked
   comment by `jkomyno` and concluded applied, sent no second comment, then removed
   the label and closed the issue. Session 2 ended `completed`, exit 0.
5. Independent reads after the run: the issue was closed (`completed`) with no
   labels; it had exactly one comment, by `jkomyno`, carrying the marker; its
   events were `labeled`, `unlabeled`, `closed`, once each.
6. `plan inspect --json` (3.7 KiB) named all four operations applied, with the
   comment's probe verdict. The CLI credential was absent from the journal, plan,
   investigation trace, diagnosis, and captured host output. The only account id
   in the diagnosis is the approved scope. The launcher's private temporary
   directories were removed after each command.

The plan's own operations restored the label set and closed the issue; the comment
stays as evidence. The local journal, plan, and logs, which hold the account id and
comment body, were deleted after this record was written. This run shows recovery
from one injected local reply loss and one executor kill for these four tools; it
does not show behavior for every remote failure, interactive CLI approval prompts,
other CLI versions, or an exactly-once guarantee.

## 2026-11-10: installed GitHub host, read-only

Composio CLI 0.6.0 on macOS. The bundle was installed with `scripts/install.sh`
from outside the checkout into a temporary prefix, from snapshot commit
`207ba22`, whose code is identical to the commit that adds this record. Every
command used the installed `bin/codemode-composio` launcher with the user's
single active GitHub connected account (`--account ca_...`) and
`--repo jkomyno/codemode`. Only read tools ran; nothing was written to GitHub.

- **Investigation with [`examples/triage.ts`](../integrations/composio/examples/triage.ts):**
  status `success`, 4 reads (open issues, labels, failed workflow runs, and the
  failed run's jobs), all `succeeded`; 5.2 s inside the run, about 10 s for the
  whole command. The repository had 0 open issues, so the proposal was empty and
  no plan was written. The trace kept each result's size (13 to 11,763 bytes)
  and no values.
- **Log reads** with a one-off program: 4 reads, including the run-log archive
  and a 98,623-byte job log. The program confirmed the archive's signed URL had
  been replaced before it saw the result; the trace contained no signed URL and
  no log line.
- **Plan writing:** a four-operation proposal for a placeholder issue (add
  `documentation`, a marked comment, remove the label, close) was written and
  checked by `codemode plan check`, with the GitHub login and the numeric
  repository id resolved live in each scope. It was not approved or run; the
  live write waits for authorization of a dedicated issue.
- **Credential and identifiers:** the CLI credential was absent from the traces,
  summaries, and stderr (`testing/credential-check.ts`, values never printed),
  and no `ca_` id appeared in a trace.
- **Temporary files:** `composio run` honors `TMPDIR`. Its per-run
  `composio-run-artifacts-*` directory, with `run.log` and one plaintext
  `execute-data-<SLUG>-*.json` argument file per call, was created inside the
  launcher's private directory; the default `$TMPDIR` gained none, and the
  launcher removed its directory on exit.

## 2026-11-09: label write and recovery

Verified on 2026-11-09 with Composio CLI 0.6.0 and the Rust binary built from
`e1f8a35`. The provider was the real GitHub connection; all external calls used
the local Composio CLI with explicit connected-account selection.

This is a record of that run at `e1f8a35`. Since then, `--drop-reply` has moved
from the production plan host to the test-only
[`integrations/composio/testing/host.ts`](../integrations/composio/testing/host.ts)
(then `testing/plan-host.ts`), which repeats this step under `composio run`, and the `--durability` option has
been removed; journal commits always use storage durability.

The target was the dedicated [validation issue #2](https://github.com/jkomyno/codemode/issues/2)
in `jkomyno/codemode`. It started open with no labels. The `documentation` label
already existed in the repository. No repository or label was created.

### Approved operation

One `GITHUB_ADD_LABELS_TO_AN_ISSUE` operation, adding `documentation` to issue 2,
with concurrency 1, at most one dispatch attempt, two available probes, and
storage durability. The exact plan digest, account, declaration, and limits were
approved before the run. The declaration used `repeat: unsafe`, `reconcile: state`,
and `summary/1` retention. The adapter's read-only preflight returned `ready` and
confirmed the issue did not have the label.

### Interruption and recovery

1. The adapter received a successful live mutation response. Its explicit
   `--drop-reply live-label` option (now in
   `integrations/composio/testing/host.ts`) withheld that response from Rust.
2. The validation harness sent SIGKILL to the Rust executor owning this journal.
   The stopped journal contained one attempt and zero observations: unknown.
3. A fresh host resumed the same journal. A live read found the label, producing
   `satisfied` with attribution unknown. The session completed with one probe;
   the journal still contained exactly one dispatch attempt and zero observations.
4. An independent GitHub API read through Composio confirmed the label was on
   the issue and other labels were unchanged. The issue's event history contained
   one `labeled` event for `documentation`.

An initial resume was refused because the validation harness had kept its own
SQLite reader open. After that reader closed, resume succeeded. The refused
startup created no session, attempt, or probe. This was a harness correction,
not a runtime change or a repeated mutation.

The lost observation was deliberately injected after a real success reached the
host. This verifies recovery from that local response loss and a killed executor;
it does not establish behavior for every remote failure or an exactly-once
guarantee. The state probe establishes satisfaction, not which actor added the
label. One journaled attempt and one GitHub label event are the measured evidence.

### Cleanup and evidence

The added label was removed and the dedicated issue closed. An independent final
read confirmed the issue was closed and its original empty label set restored.
Cleanup used separate, explicit Composio executions.

Account identifiers, raw API responses, the approved plan, journal, and harness
logs remain outside the repository. The live journal and captured process output
were checked for the locally available CLI credential without printing its value.
This check supplements the existing synthetic retention tests; it is not a
general secret scanner.

Interactive CLI approval prompts, other mutation families, and other CLI versions
remain unverified. The helper supplies no remote receipts, fencing, execution
version pin, or proof that an interrupted request cannot apply later.
