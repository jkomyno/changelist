# Operator trial

A 30-minute script for a Composio developer who has not worked on codemode to
try the installed GitHub triage workflow on a test repository they own, and
report what happened.

**Status: no person has run this trial yet.** No feedback has been collected,
and nothing in this repository should be read as usability evidence. The
automated installation check (`scripts/install-check.sh`) proves that the
installed bundle's files, adapters, and launcher work with synthetic input; it
does not show that a person can follow the workflow. The four write contracts
have run live once, on the maintainer's own repository
([record](composio-live-validation.md)); use a repository you can discard.

## What the trial should answer

Can someone who knows the Composio CLI, but not codemode, go from an installed
bundle to a reviewed, approved, interrupted, resumed, and diagnosed change list
by following [the guide](composio-github.md) alone? Where do they hesitate,
misread output, or need to ask?

## Before the timed session

The first build compiles V8 and can take several minutes, so do this before the
30 minutes start.

- macOS or Linux, with Git, mise, and a C/C++ toolchain.
- The Composio CLI, version 0.6.0 (the verified version), logged in with
  `composio login`, and a GitHub connection made with `composio link github`.
- The commit SHA the maintainer asks you to try, installed as described in
  [installation](installation.md):

  ```sh
  scripts/install.sh --revision "$REVISION" --prefix "$HOME/.local/share/codemode"
  ```

- A new GitHub repository you own, for example `you/triage-sandbox`, with
  GitHub's default labels (which include `bug`) and these open issues, in order:
  1. "Login page crashes"
  2. "Login page crashes" (the same title, so the example proposes closing it
     as a duplicate with a comment)
  3. "CI fails on main" (no labels, so the example proposes `bug`)

  Optional: push a workflow whose job runs `exit 1` and let it fail once; the
  example then also proposes a comment on issue 3 naming the failed job.

Do not use a repository whose issues matter. Every write in the trial goes to
this repository only.

## The session (about 30 minutes)

Keep the guide open and follow it; this list only says what to do when, and
what to watch for. Note the time at each step and anything that surprised you.

| Minutes | Step | Guide section | Watch for |
| --- | --- | --- | --- |
| 0–5 | Set `INSTALL`, `ACCOUNT`, and `REPO`; find your `ca_...` id in `composio connections list`. | [Before you start](composio-github.md#before-you-start) | Was it clear which id to use, and why an alias is refused? |
| 5–10 | Investigate with the bundled `examples/triage.ts`. Open `plan.json`. | [1. Investigate](composio-github.md#1-investigate) | Did the proposal match what you expected from the three issues? Can you find the comment markers? |
| 10–15 | Write `declaration.json`, run `codemode plan check`, optionally `preflight`, then approve with the digest and `--concurrency 1`. | [2. Review and approve](composio-github.md#2-review-and-approve) | Could you tell exactly what would be written, as which account, to which repository? Did approval feel bound to what you read? |
| 15–20 | Start `run`. In a second terminal, run `codemode plan inspect --journal triage.db` while it works. After the first operation shows as applied, press Ctrl-C in the first terminal. Optionally, start a second `run` on the same journal before interrupting, and note what it says. | [3. Run, watch, interrupt](composio-github.md#3-run-watch-interrupt) | Could you follow progress from the second terminal? What did the run print when interrupted, and what exit status did it return (`echo $?`)? |
| 20–25 | Run `resume`. If an operation waits for review, check the issue on GitHub and record your finding with `codemode plan review`. | [4. Resume and review](composio-github.md#4-resume-and-review) | Did you understand why each operation was settled, continued, or left for you? Did any comment appear twice on GitHub? |
| 25–28 | Export `codemode plan inspect --journal triage.db --json > diagnosis.json`. Compare it with the issues on GitHub. | [5. Export a diagnosis](composio-github.md#5-export-a-diagnosis) | Does the diagnosis name each operation's outcome in a way that matches GitHub? |
| 28–30 | Clean up (below) and write your notes. | [What is stored](composio-github.md#what-is-stored-and-where) | Did you know which files held your data? |

If something fails, do not work around it silently: note the command, its full
output line, and the exit status, then continue only if the guide tells you how.

## Questions to answer

1. Which step took longest, and why?
2. Where did you have to guess, re-read, or look outside the guide?
3. Before approving, did you trust that the plan you read is the plan that
   would run? What would have made you more or less confident?
4. After the interrupt, could you say which changes had happened, which were
   uncertain, and what was safe to do next, before running `resume`?
5. Did `plan inspect` in the second terminal tell you what you wanted while the
   run was in progress?
6. Was any output wrong, misleading, or alarming? Quote it.
7. Compared with how you would do this today (for example, a script calling
   `composio execute`), what would you keep, and what would you drop?
8. Would you use this for a real repository? What is missing?

## Clean up

```sh
rm -f triage.db triage.db-wal triage.db-shm triage.db.lock
```

Remove the journal files only after every command has exited. They hold the
approved plan, including full comment bodies, your connected-account id and
GitHub login, and your approver and reviewer notes. `plan.json`,
`declaration.json`, and the investigation trace hold the proposal and the
scopes; delete them too unless you are sending them on purpose. The launcher
already removed the Composio CLI's temporary argument files. Delete the test
repository, or its issues, when you are done.

## How to report

Send the maintainer, through a GitHub issue on `jkomyno/codemode` or directly:

- your OS and architecture, `composio --version`, and the installed `REVISION`;
- the time each step took, and where you stopped if you did not finish;
- your answers to the questions above, in your own words;
- the JSON lines the commands printed, and their exit statuses;
- `diagnosis.json`, which holds no argument or result values.

Do not attach the journal, `plan.json`, or traces from a repository you would
not publish: they contain argument values and your account identifiers. Report
what happened, including failures and confusion; a trial that did not finish is
as useful as one that did.
