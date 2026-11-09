// Composio-shaped client for the offline provider: `execute(slug, arguments, { account })`
// resolves with `{ successful, data, error }`, like the `execute` helper of `composio run`.
// A transport failure (reset, abort, refused connection) rejects: nothing was observed.

export interface Envelope {
  successful: boolean;
  data: unknown;
  error: string | null;
}

export type Execute = (slug: string, args: Record<string, unknown>, options: { account: string; signal?: AbortSignal }) => Promise<Envelope>;

export function providerClient(url: string): Execute {
  return async (slug, args, { account, signal }) => {
    const response = await fetch(`${url}/execute`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ slug, arguments: args, account }),
      signal,
    });
    if (!response.ok) throw new Error(`provider HTTP ${response.status}`);
    return (await response.json()) as Envelope;
  };
}

/**
 * Unwrap an envelope for the program engines' tools: data on success, a thrown error otherwise.
 * A transport failure is reported as an unknown outcome, never as a plain failure, because the
 * request may have reached GitHub; this is the tool wrapper a careful host author would write,
 * and the same for Pi and OpenCode.
 */
export async function call(execute: Execute, slug: string, args: Record<string, unknown>, account: string, signal?: AbortSignal): Promise<unknown> {
  let envelope: Envelope;
  try {
    envelope = await execute(slug, args, { account, signal });
  } catch (error) {
    const cause = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
    throw new Error(`outcome unknown: no reply, the request may have reached GitHub (${cause})`);
  }
  if (!envelope.successful) throw new Error(envelope.error ?? `${slug} failed`);
  return envelope.data;
}
