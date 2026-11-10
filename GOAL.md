# Goal

## Current product direction (2026-11-10)

The goal has narrowed to one workflow: a Composio developer uses the logged-in
Composio CLI to investigate a GitHub problem with authorized reads, reviews and
approves an exact change list, executes it under host-owned limits, watches it
progress, and recovers an interrupted batch without guessing what happened.
Pi and OpenCode remain secondary hosts that reach the same executor. Programs
run in one embedded V8 worker; `codemode run`, the Deno backend, mock CLI tools,
and other compatibility surfaces were removed. [STRATEGY.md](STRATEGY.md) holds
the current direction, and the
[workflow plan](docs/plans/2026-11-09-2345-refactor-composio-github-workflow-plan.md)
holds the requirements and decisions behind it.

The original brief follows unchanged except for notes on removed surfaces.

## Original brief

You are building a minimal Rust prototype called `codemode` — a local, sandboxed code-mode runtime for AI agents.

The reusable execution core and CLI must be written in Rust and designed for use
from **Pi, OpenCode, Codex, and the Composio CLI**. These are host integration
targets, not four different runtimes. A host can launch the CLI with an explicitly
configured tool provider; reusing that host's native tools additionally requires
a verified adapter. Shell invocation alone does not establish native-tool access.

Keep the public technical story small enough to explain and demonstrate in a
**30-minute technical presentation**: one run-level control contract, a bounded
rolling scheduler, an inspectable execution record, and bounded recovery of an
approved batch of operations. Demonstrate those properties on a small composed
workflow, a controlled failure, and an interrupted batch. Specific event names,
submissions, and presentation logistics stay private.

Core idea:
The product bet is that agents can express useful multi-tool workflows as code.
Rust owns admission, concurrency, deadlines, cancellation, output accounting, and
typed outcomes around that code. The runtime executes TypeScript/JavaScript in a
restricted environment, exposes a small helper surface, keeps provider
credentials outside the guest, and emits structured traces for evaluation and
debugging. Measure the value of these mechanisms; neither Rust nor code mode
implies an automatic performance or security advantage.

This project should be interesting to people who care about:
- Practical, production-grade Rust (patterns, isolation, safety, performance, clear boundaries)
- Systems-level thinking around sandboxes and capability-based execution
- Developer tooling that makes AI agents more reliable and inspectable
- Clean interop between Rust and TypeScript
- Evidence-based evaluation of agent behavior (traces, tool calls, outcomes)
- Open-source, agent-friendly infrastructure that is easy to extend

You're already starting from a solid Rust project template that includes agent-friendly conventions and a dual lib/bin layout, whose structure and conventions you should preserve while building the `codemode` prototype.

Goals:
- A Rust CLI/binary that runs TypeScript/JavaScript code in a sandboxed process (subprocess isolation or a lightweight embed is fine for v0).
- The code can call external tools via a small injected helper surface. Secrets and credentials never enter the sandbox.
- Every run produces a structured, machine-readable trace (JSON) of the code executed, every tool call (name, args, result summary, timing, error), and final outcome. The trace should be easy to feed into LangSmith, Langfuse, Gauge-style evals, or simple scripts.
- Ship a tiny TypeScript type definition / SDK so agents can generate well-typed code against the helper surface.
- Keep the first version small and shippable. Do not implement microVMs, full persistent Jupyter state, a web UI, or multi-language support yet.
- Use one provider-independent execution contract across the target hosts. Keep
  host registration, model choice, authentication, and provider-specific behavior
  in adapters. Prove each named host integration separately before claiming it
  works.
- Enforce a shared absolute deadline, call allowance, concurrency limit,
  cancellation state, and serialized-output byte budget outside guest code.
  Refill a concurrency slot when a call completes. Preserve call identity and
  distinguish rejected, failed, completed, and genuinely unknown remote outcomes.
- Recover approved operation lists: "Review a batch of external changes, execute
  it, and know how to continue after an interruption." Accept a finite, versioned
  plan of operations; bind approval to its digest, provider scope, recovery
  contracts, and budgets; journal dispatch intent and observed results durably;
  and after a crash reconcile uncertain outcomes only as the trusted provider's
  declared contract allows. Never claim exactly-once execution, and never let
  plan approval bypass a host's permission gates.
- Keep the story within the concepts above. Recovery covers only approved
  operation lists: a general workflow engine, replay of arbitrary programs,
  persistent notebook state, a general dependency planner, billing, provider
  catalog infrastructure, and a UI remain out of scope.

Composio is a first-class integration, not built-in:
- Learn from its backend ToolRouter's scheduling, data movement, limits, and
  outcome handling. Evaluate architectural improvements in a new Rust core;
  avoid copying its product surface or treating its backend as lacking safeguards.
- Design the helper surface and trace format so that Composio (or any other tool provider) can be plugged in cleanly.
- Before implementing the Composio adapter, familiarize yourself with the relevant code in the relevant provider backend repositories, when source access is available. Look at how local sandbox / workbench sessions, helper contracts, and tool execution work so the integration matches the real surface.
- The Composio adapter should be a separate module or feature flag. The core runtime must work without it (e.g. with a simple mock tool provider). (The mock provider now exists only in the test-only `testkit` feature; the production binary has no built-in tools.)

Inspiration to borrow tastefully (do not copy code or UI):
- Gauge’s full session traces and evidence of real agent behavior.
- Treg’s agent-native style: one clean interface, secrets never handed to the agent.
- Glen’s idea of session/trajectory recall — make the traces structured enough to be useful later as memory.
- Cloudflare’s Code Mode pattern.
- The codemode approaches in OpenCode and Pi (see resources).

Workflow requirements:
- Commit often with clear, focused commit messages. Prefer small, reviewable commits.
- Keep any references to specific events completely private. Do not mention them in code, comments, commit messages, READMEs, AGENTS.md, or any committed file. The work should stand on its own technical merits.
- Explicitly use subagents for parallel work (e.g. one on the runner/isolation, one on the TypeScript types + example, one on the trace schema + CLI, one on the Composio adapter after reading the private repos).
- Double-review important pieces (trace schema, sandbox boundary, helper surface, Composio adapter) with Codex’s default model before considering them done.

Concrete deliverables:
1. Rust project structure (Cargo.toml, main binary, library crate if useful) following the jkomyno/rust-template conventions and agent-friendly layout.
2. Sandbox runner that can execute a .ts/.js file (or inline code) with the helper surface injected.
3. Trace schema + emitter (JSON file or stdout).
4. Minimal TypeScript types for the helper API.
5. CLI interface: `codemode run --code path/to/file.ts` and a way to print/export the trace. (`codemode run` has since been removed; programs run through `codemode bridge`, and `codemode inspect` reads traces.)
6. A short README explaining the architecture, how to add a tool provider (with Composio as the worked example), and how the traces are meant to be used for evals.
7. At least one example: a small TypeScript snippet that calls tools (via the mock provider or Composio) and the resulting trace. (The current example is the Composio investigation program `integrations/composio/examples/triage.ts`.)
8. AGENTS.md (and a minimal skills layout) so the repo is ready for further agent work.

Constraints:
- Prefer simple, reliable isolation over perfect security for v0.
- Make the code clear and well-commented so a human can extend it.
- Prefer existing crates (tokio, serde, etc.) over inventing new infrastructure.
- If you need to mock a tool provider for the prototype, do so cleanly behind a trait so real integrations are easy to swap in.

Start by proposing the project layout and the trace schema, then implement the core runner + CLI + example. Work in parallel with subagents where possible.

<resources>
https://github.com/jkomyno/rust-template
https://github.com/leonardomso/rust-skills
https://github.com/morganlinton/BeautifulRust
https://www.withgauge.com/
https://www.withgauge.com/agents/
https://github.com/gauge-sh/gauge-cli
https://treg.to/
https://github.com/superdesigndev/treg
https://tryglen.com/
https://blog.cloudflare.com/code-mode/
https://blog.cloudflare.com/code-mode-mcp/
https://developers.cloudflare.com/agents/tools/codemode/
https://github.com/anomalyco/opencode/blob/dev/packages/codemode/README.md
https://github.com/earendil-works/pi/blob/main/packages/codemode/README.md
</resources>
