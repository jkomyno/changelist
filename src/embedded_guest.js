// Only the captured protocol operations reach Rust. The parent validates all output.
(() => {
  const core = Deno.core;
  const { op_guest_write: write, op_guest_receive: receive, op_guest_finish: finish,
    op_guest_log: log } = core.ops;
  const clone = core.structuredClone;
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const enqueue = queueMicrotask;
  let nextId = 0;
  let outgoing = [];
  const pending = new Map();

  const flush = () => {
    if (outgoing.length === 0) return;
    const calls = outgoing;
    outgoing = [];
    write(calls.map(call => stringify(call) + "\n").join(""));
  };
  const fail = error => {
    flush();
    finish(stringify({ type: "error", message: error instanceof Error ? error.message : String(error) }), true);
  };

  Object.defineProperty(globalThis, "structuredClone", { value: clone, writable: true, configurable: true });
  globalThis.tools = {
    call(tool, args = null) {
      if (typeof tool !== "string") return Promise.reject(new TypeError("Tool name must be a string"));
      if (!Number.isSafeInteger(nextId)) return Promise.reject(new Error("Call IDs exhausted"));
      const id = nextId++;
      return new Promise((resolve, reject) => {
        let call;
        try { call = clone({ type: "call", id, tool, arguments: args }); }
        catch (error) { reject(error); return; }
        pending.set(id, { resolve, reject });
        outgoing.push(call);
        if (outgoing.length === 1) enqueue(flush);
      });
    },
  };
  globalThis.console = Object.fromEntries(["log", "info", "warn", "error", "debug"].map(
    level => [level, (...args) => log(args.map(String).join(" ") + "\n")],
  ));

  const replies = async () => {
    for (;;) {
      const frame = parse(await receive());
      if (frame === null || typeof frame !== "object" || Array.isArray(frame) ||
          frame.type !== "result" || !Number.isSafeInteger(frame.id) || frame.id < 0 ||
          !((frame.ok === true && "value" in frame) ||
            (frame.ok === false && frame.error !== null && typeof frame.error === "object" &&
              typeof frame.error.code === "string" && typeof frame.error.message === "string"))) {
        throw new Error("Invalid host result frame");
      }
      const call = pending.get(frame.id);
      if (!call) continue;
      pending.delete(frame.id);
      if (frame.ok) call.resolve(frame.value);
      else {
        const error = new Error(frame.error.message);
        error.code = frame.error.code;
        call.reject(error);
      }
    }
  };

  globalThis.__codemode_run = program => {
    delete globalThis.__codemode_run;
    replies().catch(fail);
    Promise.resolve().then(program).then(value => {
      flush();
      finish(stringify({ type: "output", value: value ?? null }), false);
    }, fail).catch(fail);
  };
  delete globalThis.Deno;
  delete globalThis.__bootstrap;
})();
