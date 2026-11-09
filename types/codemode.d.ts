/** JSON values crossing the guest/provider boundary. */
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

/** Host failures reject with a machine-readable code; no automatic retry occurs. */
interface ToolError extends Error {
  code: string;
}

/**
 * Tool contracts available to the guest. Augment this interface with a host's
 * tools, as `integrations/pi/pi-tools.d.ts` does; the host still decides which
 * names are authorized for a run.
 */
interface CodemodeTools {}

/** Helper declarations guide authoring; Rust validates and authorizes actual calls. */
declare const tools: {
  call<K extends keyof CodemodeTools>(
    name: K,
    args: CodemodeTools[K]["arguments"],
  ): Promise<CodemodeTools[K]["result"]>;
};
