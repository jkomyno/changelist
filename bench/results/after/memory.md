# Memory pass

Driver: bun 1.4.4. 5 fresh processes per engine and scenario (the slowest selected latency profile, so the sampler sees each run longer); values are medians (min-max) in MiB.

- **Driver peak RSS over baseline**: `process.resourceUsage().maxRSS` of the sample process minus the median of 5 fixture-only processes (17.9 MiB). Includes everything in that process: the engine, Pi's worker threads and QuickJS heaps, OpenCode's interpreter, and for codemode only the host client.
- **Driver ps peak over baseline**: the same process's RSS as sampled by `ps` from the parent, minus the median ps peak of the fixture-only processes (32.7 MiB). Under Bun on macOS, `maxRSS` reads 8-23 MiB lower than `ps` RSS for the same process (checked by hand; Node agrees with ps), so both are shown.
- **Bridge / Deno peak**: codemode only. RSS of the `codemode bridge` process and its `deno` child, sampled with `ps` by the parent; peaks can be missed between samples (see interval). Pi and OpenCode spawn no processes.
- RSS counts shared pages (e.g. the Deno and Bun binaries) in full; it is not a measure of unique memory.

| Scenario | Engine | Succeeded/N | Driver maxRSS over baseline | Driver ps peak over baseline | Bridge peak | Deno peak | Children total peak | ps interval p50 | ps samples p50 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| triage/mixed | codemode | 5/5 | 3.1 (2.9-3.3) | 6.7 (6.4-6.8) | 4.8 (4.7-5.0) | 40.0 (39.8-40.4) | 44.7 (44.5-45.3) | 10 ms | 87 |
| triage/mixed | codemode-c4 | 5/5 | 3.6 (3.0-4.0) | 6.6 (6.1-6.9) | 4.7 (4.7-4.8) | 39.9 (39.8-40.2) | 44.7 (44.4-45.0) | 10 ms | 107 |
| triage/mixed | codemode-prewarm | 5/5 | 3.2 (2.9-3.6) | 6.4 (6.1-7.5) | 4.7 (4.7-4.8) | 39.7 (39.4-40.8) | 44.5 (44.1-45.6) | 10 ms | 87 |
| triage/mixed | pi | 5/5 | 55.5 (53.8-57.6) | 66.0 (64.1-68.3) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 70 |
| triage/mixed | opencode | 5/5 | 83.2 (82.2-84.9) | 109.1 (104.0-112.0) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 104 |
| triage-large/mixed | codemode | 5/5 | 4.7 (4.4-5.3) | 8.0 (7.6-8.3) | 7.0 (6.9-7.7) | 41.9 (41.6-42.2) | 48.8 (48.5-49.3) | 10 ms | 63 |
| triage-large/mixed | codemode-c4 | 5/5 | 3.9 (2.9-4.5) | 7.9 (7.7-8.7) | 7.0 (6.7-7.8) | 42.2 (41.9-42.5) | 49.2 (48.7-49.5) | 10 ms | 83 |
| triage-large/mixed | codemode-prewarm | 5/5 | 4.7 (4.2-4.8) | 8.0 (7.8-8.3) | 6.9 (6.8-7.0) | 42.0 (41.4-42.3) | 48.9 (48.3-49.2) | 10 ms | 63 |
| triage-large/mixed | pi | 5/5 | 55.2 (55.1-56.9) | 65.0 (64.7-67.3) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 47 |
| triage-large/mixed | opencode | 5/5 | 82.2 (81.4-83.2) | 106.2 (105.0-111.9) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 79 |
| denied/mixed | codemode | 5/5 | 3.2 (3.1-3.3) | 6.6 (6.3-6.9) | 4.7 (4.7-4.8) | 40.2 (39.8-40.3) | 44.9 (44.5-45.0) | 10 ms | 92 |
| denied/mixed | codemode-c4 | 5/5 | 2.5 (2.1-3.2) | 6.7 (6.5-6.8) | 4.7 (4.7-4.9) | 39.8 (39.7-40.3) | 44.7 (44.4-45.1) | 10 ms | 117 |
| denied/mixed | codemode-prewarm | 5/5 | 3.5 (3.3-3.6) | 6.8 (6.5-7.1) | 4.8 (4.6-4.8) | 39.8 (39.6-39.9) | 44.5 (44.2-44.7) | 10 ms | 92 |
| denied/mixed | pi | 5/5 | 54.6 (53.3-58.0) | 65.6 (63.7-68.6) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 72 |
| denied/mixed | opencode | 5/5 | 82.2 (79.4-84.0) | 105.2 (102.2-112.2) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 108 |
| deadline/mixed | codemode | 0/5 | 1.8 (1.4-2.5) | 4.1 (3.3-4.6) | 4.0 (3.8-4.1) | 36.5 (36.0-37.1) | 40.6 (40.0-41.1) | 10 ms | 36 |
| deadline/mixed | codemode-c4 | 0/5 | 1.8 (1.8-2.1) | 3.6 (3.6-4.1) | 4.0 (3.9-4.1) | 36.3 (35.5-36.9) | 40.2 (39.6-40.9) | 10 ms | 36 |
| deadline/mixed | codemode-prewarm | 0/5 | 2.0 (1.9-2.2) | 4.4 (4.1-4.7) | 4.0 (3.9-4.3) | 36.2 (35.0-36.6) | 40.3 (38.8-40.6) | 10 ms | 36 |
| deadline/mixed | pi | 0/5 | 55.4 (53.4-56.2) | 63.9 (61.9-64.8) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 36 |
| deadline/mixed | opencode | 0/5 | 73.8 (73.2-80.5) | 88.5 (87.8-95.5) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 53 |
| guest-exception/zero | codemode | 0/5 | 4.6 (4.4-5.0) | 6.5 (6.2-6.9) | 3.8 (3.4-4.4) | 36.0 (35.5-37.5) | 39.4 (39.1-41.9) | 10 ms | 14 |
| guest-exception/zero | codemode-c4 | 0/5 | 4.6 (4.4-4.9) | 6.4 (6.0-6.7) | 4.0 (3.6-4.9) | 35.8 (35.4-36.5) | 39.8 (38.8-40.5) | 10 ms | 14 |
| guest-exception/zero | codemode-prewarm | 0/5 | 5.0 (4.5-5.4) | 6.7 (6.3-6.9) | 3.9 (3.6-4.1) | 36.2 (35.4-37.0) | 40.2 (39.0-41.0) | 10 ms | 14 |
| guest-exception/zero | pi | 0/5 | 58.9 (56.9-60.1) | 67.0 (64.8-68.5) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 16 |
| guest-exception/zero | opencode | 0/5 | 88.4 (88.4-89.1) | 98.1 (97.5-101.5) | n/a | n/a | 0.0 (0.0-0.0) | 11 ms | 30 |
| lost-response/mixed | codemode | 0/5 | 2.1 (1.9-2.7) | 3.8 (3.6-4.3) | 3.7 (3.5-4.1) | 36.3 (35.7-36.4) | 39.9 (39.3-40.4) | 10 ms | 41 |
| lost-response/mixed | codemode-c4 | 0/5 | 2.2 (2.1-2.4) | 3.9 (3.8-4.3) | 3.8 (3.5-3.9) | 35.8 (35.7-36.1) | 39.5 (39.2-39.7) | 10 ms | 41 |
| lost-response/mixed | codemode-prewarm | 0/5 | 2.5 (2.0-2.6) | 4.4 (3.9-4.6) | 3.7 (3.5-3.9) | 36.1 (35.6-36.4) | 39.8 (39.1-40.3) | 10 ms | 41 |
| lost-response/mixed | pi | 0/5 | 55.9 (54.5-57.5) | 64.2 (62.8-66.2) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 41 |
| lost-response/mixed | opencode | 0/5 | 68.7 (66.4-75.4) | 77.2 (76.5-82.6) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 57 |
