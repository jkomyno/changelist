// Guest program for the Pi demo. Each tools.call becomes a Pi nested tool call,
// admitted and scheduled by Rust. Run from the repository root.
const [text, listing] = await Promise.all([
  tools.call("read", { path: "integrations/pi/demo/fixture.txt" }),
  tools.call("ls", { path: "integrations/pi/demo" }),
]);

// bash is authorized for codemode but blocked by Pi's approval gate;
// write is active in Pi but not authorized for codemode, so Rust rejects it.
const attempts = await Promise.allSettled([
  tools.call("bash", { command: "echo this-must-not-run" }),
  tools.call("write", { path: "integrations/pi/demo/never.txt", content: "no" }),
]);

let environment = "readable";
try {
  Deno.env.toObject();
} catch {
  environment = "denied";
}

return {
  title: String(text).split("\n")[0],
  entries: String(listing).split("\n").filter(Boolean).length,
  refused: attempts.map((a) => (a.status === "rejected" ? a.reason.code : "ran")),
  environment,
};
