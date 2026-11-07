/**
 * Report whether the Composio CLI credential appears in the given files, without
 * printing it. The key comes from COMPOSIO_USER_API_KEY or ~/.composio/user_data.json,
 * where Composio CLI 0.6.0 keeps it when no OS keyring is used.
 */
const candidates: string[] = [];
const fromEnv = Deno.env.get("COMPOSIO_USER_API_KEY");
if (fromEnv) candidates.push(fromEnv);
try {
  const data = JSON.parse(Deno.readTextFileSync(`${Deno.env.get("HOME")}/.composio/user_data.json`));
  for (const [field, value] of Object.entries(data)) {
    if (/key/i.test(field) && typeof value === "string" && value.length > 8) candidates.push(value);
  }
} catch {
  // No readable file: the CLI may use the OS keyring.
}
if (candidates.length === 0) {
  // The check cannot pass without a credential to look for.
  console.log("not checked: no credential found in COMPOSIO_USER_API_KEY or ~/.composio/user_data.json");
  Deno.exit(1);
} else {
  let leaked = false;
  for (const path of Deno.args) {
    const found = candidates.some((key) => Deno.readTextFileSync(path).includes(key));
    leaked ||= found;
    console.log(`${path.split("/").pop()}: credential ${found ? "FOUND" : "absent"}`);
  }
  if (leaked) Deno.exit(1);
}
