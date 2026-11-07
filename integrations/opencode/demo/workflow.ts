// Guest program for the OpenCode demo. Each tools.call is admitted and scheduled by
// Rust, then gated by OpenCode's permission service before a host tool runs.
const [first, second] = await Promise.all([
  tools.call("lookup", { sku: "A-1" }),
  tools.call("lookup", { sku: "B-2" }),
]);

// purge is denied by an OpenCode rule; bash is an OpenCode built-in that the
// adapter cannot call, so Rust rejects it before OpenCode sees it.
const attempts = await Promise.allSettled([
  tools.call("purge", {}),
  tools.call("bash", { command: "echo this-must-not-run" }),
]);

let environment = "readable";
try {
  Deno.env.toObject();
} catch {
  environment = "denied";
}

return {
  stock: [first, second].map((text) => JSON.parse(String(text)).stock),
  refused: attempts.map((a) => (a.status === "rejected" ? a.reason.code : "ran")),
  environment,
};
