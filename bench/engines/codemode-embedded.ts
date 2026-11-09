/** codemode with the opt-in embedded V8 worker; no Deno executable is needed. */
import { createEngine } from "./codemode.ts";

const engine = createEngine({ prewarm: false, backend: "embedded-v8" });
export const info = engine.info;
export const runEngine = engine.runEngine;
