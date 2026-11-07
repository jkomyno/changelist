// Six calls submitted together, one of which fails immediately. Promise.allSettled
// keeps every result, so the run succeeds and returns a deterministic summary.
const steps = [
  { tool: "mock.wait", args: { ms: 120, value: "a" } },
  { tool: "mock.wait", args: { ms: 30, value: "b" } },
  { tool: "mock.fail", args: { message: "controlled failure" } },
  { tool: "mock.wait", args: { ms: 30, value: "d" } },
  { tool: "mock.wait", args: { ms: 120, value: "e" } },
  { tool: "mock.wait", args: { ms: 30, value: "f" } },
];
const settled = await Promise.allSettled(steps.map((step) => tools.call(step.tool, step.args)));
return settled.map((result) =>
  result.status === "fulfilled"
    ? { status: "fulfilled", value: result.value }
    : { status: "rejected", code: (result.reason as { code?: string }).code ?? null }
);
