// Only the captured protocol operations reach Rust. Rust builds every stdout frame
// from the payloads passed here, so guest overrides of built-ins cannot forge one.
(() => {
  const core = Deno.core;
  const { op_guest_calls: writeCalls, op_guest_receive: receive, op_guest_output: output,
    op_guest_error: raise, op_guest_log: log } = core.ops;
  const clone = core.structuredClone;
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const text = String;
  const enqueue = queueMicrotask;
  let nextId = 0;
  let outgoing = [];
  const pending = new Map();

  // A value without a JSON encoding fails like a value Rust cannot parse.
  const encode = value => {
    const json = stringify(value);
    if (typeof json !== "string") throw new TypeError("Value is not JSON-encodable");
    return json;
  };
  const flush = () => {
    const calls = outgoing;
    if (calls.length === 0) return;
    outgoing = [];
    const batch = [];
    for (let index = 0; index < calls.length; index++) {
      const call = calls[index];
      batch[index] = { id: call.id, tool: call.tool, arguments: encode(call.arguments) };
    }
    writeCalls(batch);
  };
  const fail = error => {
    flush();
    const message = error instanceof Error ? error.message : error;
    raise(typeof message === "string" ? message : text(message));
  };

  Object.defineProperty(globalThis, "structuredClone", { value: clone, writable: true, configurable: true });
  globalThis.tools = {
    call(tool, args = null) {
      if (typeof tool !== "string") return Promise.reject(new TypeError("Tool name must be a string"));
      if (!Number.isSafeInteger(nextId)) return Promise.reject(new Error("Call IDs exhausted"));
      const id = nextId++;
      return new Promise((resolve, reject) => {
        let call;
        try { call = { id, tool, arguments: clone(args) }; }
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
      output(encode(value ?? null));
    }, fail).catch(fail);
  };
  delete globalThis.Deno;
  delete globalThis.__bootstrap;
  // Not needed by the helper contract, and their memory is outside the V8 heap budget.
  delete globalThis.WebAssembly;
  delete globalThis.SharedArrayBuffer;
  delete globalThis.Atomics;
})();
