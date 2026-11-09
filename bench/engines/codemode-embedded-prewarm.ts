/** One fresh embedded V8 worker prepared ahead of each run; see `createEngine`. */
import { createEngine } from "./codemode.ts";

const engine = createEngine({ prewarm: true, backend: "embedded-v8" });
export const info = engine.info;
export const runEngine = engine.runEngine;
