/**
 * Write both datasets (and the fixed constants) as JSON for bench/check_expected.py.
 *
 *   bun bench/dump-datasets.ts <out.json>
 */
import { writeFileSync } from "node:fs";
import { NOW, RELEASE_WINDOW, STALE_CUTOFF, datasetA, datasetB } from "./fixtures/datasets.ts";

const out = process.argv[2];
if (!out) {
  console.error("usage: dump-datasets.ts <out.json>");
  process.exit(2);
}
writeFileSync(
  out,
  JSON.stringify({ now: NOW, stale_cutoff: STALE_CUTOFF, release_window: RELEASE_WINDOW, A: datasetA(), B: datasetB() }),
);
