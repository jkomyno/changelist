/**
 * codemode with a prewarmed guest: the same adapter as `codemode.ts`, keeping one
 * started bridge and Deno guest ready for the next run. See `createEngine`.
 */
import { createEngine } from "./codemode.ts";

const engine = createEngine({ prewarm: true });
export const info = engine.info;
export const runEngine = engine.runEngine;
