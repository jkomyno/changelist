// Fixed launcher. All stdout, including worker writes, is untrusted host input.
const MAX_INPUT_FRAME = 1024 * 1024;
const encoder = new TextEncoder();
let worker: Worker | undefined;

function write(stream: { writeSync(bytes: Uint8Array): number }, text: string): void {
  const bytes = encoder.encode(text);
  let offset = 0;
  while (offset < bytes.length) offset += stream.writeSync(bytes.subarray(offset));
}

function finish(frame: Record<string, unknown>, failed = false): never {
  worker?.terminate();
  write(Deno.stdout, JSON.stringify(frame) + "\n");
  Deno.exit(failed ? 1 : 0);
}

function fail(error: unknown): never {
  return finish({ type: "error", message: error instanceof Error ? error.message : String(error) }, true);
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validId(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

async function* inputFrames(): AsyncGenerator<unknown> {
  const reader = Deno.stdin.readable.getReader();
  const buffer = new Uint8Array(MAX_INPUT_FRAME);
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let length = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      for (const byte of value) {
        if (byte === 10) {
          if (length === 0) throw new Error("Empty input frame");
          try {
            yield JSON.parse(decoder.decode(buffer.subarray(0, length)));
          } catch (error) {
            throw new Error(`Malformed input frame: ${error instanceof Error ? error.message : error}`);
          }
          length = 0;
        } else {
          if (length === MAX_INPUT_FRAME) throw new Error("Input frame exceeds 1 MiB");
          buffer[length++] = byte;
        }
      }
    }
    if (length !== 0) throw new Error("Unterminated input frame");
  } finally {
    reader.releaseLock();
  }
}

function launch(code: string): Worker {
  const source = `
let nextId = 0;
const pending = new Map();
globalThis.onmessage = ({ data }) => {
  const call = pending.get(data.id);
  if (!call) return;
  pending.delete(data.id);
  if (data.ok) call.resolve(data.value);
  else {
    const error = new Error(data.error.message);
    Object.assign(error, { code: data.error.code });
    call.reject(error);
  }
};
const tools = {
  call(tool: string, args: unknown = null): Promise<unknown> {
    if (!Number.isSafeInteger(nextId)) return Promise.reject(new Error("Call IDs exhausted"));
    const id = nextId++;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      try { postMessage({ type: "call", id, tool, arguments: args }); }
      catch (error) { pending.delete(id); reject(error); }
    });
  },
};
for (const level of ["log", "info", "warn", "error"]) {
  console[level] = (...args) => postMessage({ type: "log", message: args.map(String).join(" ") });
}
(async () => {
${code}
})().then(
  value => postMessage({ type: "output", value: value ?? null }),
  error => postMessage({ type: "error", message: error instanceof Error ? error.message : String(error) }),
);
`;
  const child = new Worker("data:application/typescript," + encodeURIComponent(source), {
    type: "module",
    deno: { permissions: "none" },
  });
  child.onerror = (event) => { event.preventDefault(); fail(new Error(event.message)); };
  child.onmessage = ({ data }) => {
    try {
      if (!record(data)) throw new Error("Invalid worker frame");
      if (data.type === "log" && typeof data.message === "string") {
        write(Deno.stderr, data.message + "\n");
      } else if (data.type === "call" && validId(data.id) && typeof data.tool === "string" && data.arguments !== undefined) {
        write(Deno.stdout, JSON.stringify({ type: "call", id: data.id, tool: data.tool, arguments: data.arguments }) + "\n");
      } else if (data.type === "output" && "value" in data) {
        finish({ type: "output", value: data.value ?? null });
      } else if (data.type === "error" && typeof data.message === "string") {
        fail(new Error(data.message));
      } else throw new Error("Invalid worker frame");
    } catch (error) { fail(error); }
  };
  return child;
}

try {
  for await (const frame of inputFrames()) {
    if (!record(frame)) throw new Error("Input frame must be an object");
    if (!worker) {
      if (frame.type !== "run" || typeof frame.code !== "string") throw new Error("Expected run frame with string code");
      worker = launch(frame.code);
    } else {
      if (frame.type !== "result" || !validId(frame.id) ||
        !((frame.ok === true && "value" in frame) ||
          (frame.ok === false && record(frame.error) && typeof frame.error.code === "string" && typeof frame.error.message === "string"))) {
        throw new Error("Invalid host result frame");
      }
      worker.postMessage(frame);
    }
  }
  if (!worker) throw new Error("Missing run frame");
} catch (error) { fail(error); }
