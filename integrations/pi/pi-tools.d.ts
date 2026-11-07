/// <reference path="../../types/codemode.d.ts" />
// Pi 1.4.0 built-in tools as seen by guest programs through the bridge.
// Tools with an output schema resolve to `structuredContent`; others to their text.

/** A Pi image block, as returned by `read` for image files. */
interface PiImage {
  type: "image";
  data: string;
  mimeType: string;
  note: string;
}

interface CodemodeTools {
  read: { arguments: { path: string; offset?: number; limit?: number }; result: string | PiImage };
  ls: { arguments: { path?: string; limit?: number }; result: string };
  grep: {
    arguments: {
      pattern: string;
      path?: string;
      glob?: string;
      ignoreCase?: boolean;
      literal?: boolean;
      context?: number;
      limit?: number;
    };
    result: string;
  };
  find: { arguments: { pattern: string; path?: string; limit?: number }; result: string };
  bash: {
    arguments: { command: string; timeout?: number };
    result: { output: string; truncated: boolean; full_output_path?: string; exit_code: number; wall_time_seconds: number };
  };
}
