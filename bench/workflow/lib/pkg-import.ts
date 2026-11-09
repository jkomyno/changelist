// Import an ESM package entry from an install root without placing benchmark scripts inside it.
// Node's ESM resolver ignores NODE_PATH, so resolve the top-level `exports` map here; imports
// inside the package resolve normally from its own node_modules tree.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

type ExportTarget = string | { import?: string; default?: string };

function target(entry: ExportTarget | undefined): string | undefined {
  if (typeof entry === "string") return entry;
  return entry?.import ?? entry?.default;
}

export function resolvePkg(root: string, specifier: string): string {
  const parts = specifier.split("/");
  const nameLength = specifier.startsWith("@") ? 2 : 1;
  const name = parts.slice(0, nameLength).join("/");
  const sub = parts.length > nameLength ? `./${parts.slice(nameLength).join("/")}` : ".";
  const dir = join(root, "node_modules", name);
  const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
  const exports: Record<string, ExportTarget> = pkg.exports ?? {};
  let resolved = target(exports[sub]) ?? (sub === "." ? (pkg.module ?? pkg.main) : undefined);
  if (!resolved) {
    for (const [key, value] of Object.entries(exports)) {
      if (!key.includes("*")) continue;
      const [pre, post] = key.split("*");
      if (sub.startsWith(pre) && sub.endsWith(post)) {
        resolved = target(value)?.replace("*", sub.slice(pre.length, sub.length - post.length));
        break;
      }
    }
  }
  if (!resolved) throw new Error(`cannot resolve ${specifier} in ${dir}`);
  return pathToFileURL(join(dir, resolved)).href;
}

export const pkgImport = (root: string, specifier: string): Promise<any> => import(resolvePkg(root, specifier));
