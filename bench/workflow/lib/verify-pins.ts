// Check installed package trees against pins.json: version and lockfile integrity of every
// pinned package. Usage: node lib/verify-pins.ts <pins.json> <pkgs dir>
import { readFileSync } from "node:fs";
import { join } from "node:path";

const [pinsPath, pkgs] = process.argv.slice(2);
if (!pinsPath || !pkgs) throw new Error("usage: verify-pins.ts <pins.json> <pkgs dir>");
const pins = JSON.parse(readFileSync(pinsPath, "utf8"));

const failures: string[] = [];
for (const name of ["pi-durable", "opencode"]) {
  const pin = pins.implementations[name];
  const lock = JSON.parse(readFileSync(join(pkgs, name, "package-lock.json"), "utf8"));
  const checked: Record<string, { version: string; integrity: string }> = {
    [pin.package]: { version: pin.version, integrity: pin.integrity },
    ...pin.dependencies_checked,
  };
  for (const [pkg, want] of Object.entries(checked)) {
    const locked = lock.packages[`node_modules/${pkg}`];
    const installed = JSON.parse(readFileSync(join(pkgs, name, "node_modules", pkg, "package.json"), "utf8"));
    if (locked?.version !== want.version || locked?.integrity !== want.integrity) failures.push(`${name}: ${pkg} lockfile differs from pins.json`);
    if (installed.version !== want.version) failures.push(`${name}: ${pkg} installed ${installed.version}, pinned ${want.version}`);
  }
}
if (failures.length > 0) {
  process.stderr.write(`${failures.join("\n")}\n`);
  process.exit(1);
}
process.stdout.write("pins verified\n");
