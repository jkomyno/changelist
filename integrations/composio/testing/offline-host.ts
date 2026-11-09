/**
 * Test-only entry: the production program host (`../host.ts`) with the offline
 * stand-in (`fake-composio.ts`) in place of the `execute` helper that
 * `composio run` injects. Accepts the same options as `host.ts`:
 *
 *     deno run --allow-run --allow-read --allow-write --allow-env \
 *       integrations/composio/testing/offline-host.ts --program ... --tools ...
 *
 * No network, account, or credential is used. Not installed.
 */
import { fakeExecute } from "./fake-composio.ts";

// Inject the helper as `composio run` does, then start the unchanged production host.
(globalThis as { execute?: typeof fakeExecute }).execute = fakeExecute;
await import("../host.ts");
