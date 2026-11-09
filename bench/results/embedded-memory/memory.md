# Memory pass

Driver: bun 1.4.4. 3 fresh processes per engine and scenario (the slowest selected latency profile, so the sampler sees each run longer); values are medians (min-max) in MiB.

- **Driver peak RSS over baseline**: `process.resourceUsage().maxRSS` of the sample process minus the median of 3 fixture-only processes (18.2 MiB). Includes everything in that process: the engine, Pi's worker threads and QuickJS heaps, OpenCode's interpreter, and for codemode only the host client.
- **Driver ps peak over baseline**: the same process's RSS as sampled by `ps` from the parent, minus the median ps peak of the fixture-only processes (33.1 MiB). Under Bun on macOS, `maxRSS` reads 8-23 MiB lower than `ps` RSS for the same process (checked by hand; Node agrees with ps), so both are shown.
- **Bridge / worker / Deno peak**: codemode controller directly under the driver, embedded codemode worker directly under a controller, and Deno guest RSS, distinguished by process ancestry. Pi and OpenCode spawn no processes.
- **Whole tree peak**: maximum simultaneous driver plus all descendant RSS in one ps snapshot. Children total peak likewise sums descendants in each snapshot; neither is a sum of independent process peaks.
- ps sampling starts every 10 ms; short-lived processes and peaks between snapshots can be missed (see observed interval). Fresh-process memory samples include the 100 ms post-run observation, which can include prewarming the next guest.
- RSS counts shared pages (e.g. the Deno and Bun binaries) in full; it is not a measure of unique memory.

| Scenario | Engine | Succeeded/N | Driver maxRSS over baseline | Driver ps peak over baseline | Bridge peak | Worker peak | Deno peak | Children total peak | Whole tree peak | ps interval p50 | ps samples p50 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| triage/mixed | codemode | 3/3 | 2.8 (2.8-3.0) | 6.0 (6.0-6.1) | 7.0 (7.0-7.1) | 0.0 (0.0-0.0) | 40.3 (40.1-40.4) | 47.4 (47.1-47.5) | 84.3 (84.0-84.5) | 11 ms | 83 |
| triage/mixed | codemode-prewarm | 3/3 | 3.0 (3.0-3.2) | 6.3 (6.0-6.6) | 7.0 (6.9-7.3) | 0.0 (0.0-0.0) | 39.8 (39.7-40.2) | 46.7 (46.7-47.5) | 84.4 (84.0-84.5) | 11 ms | 83 |
| triage/mixed | codemode-embedded | 3/3 | 2.8 (2.7-3.0) | 6.1 (5.6-6.2) | 6.8 (6.7-6.9) | 24.0 (23.9-24.2) | 0.0 (0.0-0.0) | 30.8 (30.6-31.2) | 67.7 (67.6-67.8) | 11 ms | 81 |
| triage/mixed | codemode-embedded-prewarm | 3/3 | 3.0 (2.9-3.2) | 6.4 (6.0-6.9) | 6.7 (6.6-6.8) | 24.2 (23.9-24.2) | 0.0 (0.0-0.0) | 30.8 (30.7-31.0) | 68.2 (67.8-68.7) | 11 ms | 81 |
| triage-large/mixed | codemode | 3/3 | 4.3 (4.0-4.4) | 7.8 (7.8-8.2) | 9.4 (9.3-9.5) | 0.0 (0.0-0.0) | 42.3 (42.0-42.4) | 51.6 (51.4-51.8) | 90.4 (90.4-91.0) | 11 ms | 61 |
| triage-large/mixed | codemode-prewarm | 3/3 | 3.7 (3.5-4.0) | 7.3 (7.2-8.7) | 9.4 (9.3-9.4) | 0.0 (0.0-0.0) | 42.3 (41.5-42.5) | 51.7 (50.8-51.9) | 90.4 (89.7-90.9) | 11 ms | 61 |
| triage-large/mixed | codemode-embedded | 3/3 | 4.4 (4.3-4.6) | 7.9 (7.4-8.1) | 9.5 (9.0-9.8) | 24.8 (24.5-24.8) | 0.0 (0.0-0.0) | 33.6 (33.5-34.3) | 73.0 (72.7-73.1) | 11 ms | 59 |
| triage-large/mixed | codemode-embedded-prewarm | 3/3 | 4.6 (4.2-5.0) | 7.7 (7.4-7.9) | 8.9 (8.7-9.1) | 24.8 (24.8-25.0) | 0.0 (0.0-0.0) | 33.7 (33.5-34.2) | 72.9 (72.4-73.5) | 11 ms | 58 |
