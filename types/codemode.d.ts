/** JSON values crossing the guest/provider boundary. */
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

/** The explicit capabilities supplied by the prototype's offline provider. */
interface MockTools {
  "mock.echo": { arguments: JsonValue; result: JsonValue };
  "mock.wait": { arguments: { ms: number; value?: JsonValue }; result: JsonValue };
  "mock.fail": { arguments: { message?: string }; result: never };
}

/** Host failures reject with a machine-readable code; no automatic retry occurs. */
interface ToolError extends Error {
  code: string;
}

/**
 * Tool contracts available to the guest. Augment this interface with a host's
 * tools, as `integrations/pi/pi-tools.d.ts` does; the host still decides which
 * names are authorized for a run.
 */
interface CodemodeTools extends MockTools {}

/** Helper declarations guide authoring; Rust validates and authorizes actual calls. */
declare const tools: {
  call<K extends keyof CodemodeTools>(
    name: K,
    args: CodemodeTools[K]["arguments"],
  ): Promise<CodemodeTools[K]["result"]>;
};
