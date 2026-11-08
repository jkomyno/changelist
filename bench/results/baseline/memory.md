# Memory pass

Driver: bun 1.4.4. 5 fresh processes per engine and scenario (the slowest selected latency profile, so the sampler sees each run longer); values are medians (min-max) in MiB.

- **Driver peak RSS over baseline**: `process.resourceUsage().maxRSS` of the sample process minus the median of 5 fixture-only processes (18.1 MiB). Includes everything in that process: the engine, Pi's worker threads and QuickJS heaps, OpenCode's interpreter, and for codemode only the host client.
- **Driver ps peak over baseline**: the same process's RSS as sampled by `ps` from the parent, minus the median ps peak of the fixture-only processes (32.8 MiB). Under Bun on macOS, `maxRSS` reads 8-23 MiB lower than `ps` RSS for the same process (checked by hand; Node agrees with ps), so both are shown.
- **Bridge / Deno peak**: codemode only. RSS of the `codemode bridge` process and its `deno` child, sampled with `ps` by the parent; peaks can be missed between samples (see interval). Pi and OpenCode spawn no processes.
- RSS counts shared pages (e.g. the Deno and Bun binaries) in full; it is not a measure of unique memory.

| Scenario | Engine | Succeeded/N | Driver maxRSS over baseline | Driver ps peak over baseline | Bridge peak | Deno peak | Children total peak | ps interval p50 | ps samples p50 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| triage/mixed | codemode | 5/5 | 2.6 (2.5-3.0) | 6.0 (5.9-6.3) | 4.5 (4.4-4.5) | 46.0 (45.5-47.0) | 50.3 (49.9-51.4) | 10 ms | 88 |
| triage/mixed | codemode-c4 | 5/5 | 2.9 (2.1-3.5) | 6.2 (6.1-6.6) | 4.4 (4.3-4.5) | 45.8 (45.6-46.4) | 50.2 (49.9-50.9) | 10 ms | 107 |
| triage/mixed | pi | 5/5 | 54.5 (53.5-55.8) | 65.5 (64.2-67.1) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 70 |
| triage/mixed | opencode | 5/5 | 82.8 (79.0-83.7) | 105.5 (103.1-107.8) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 103 |
| triage-large/mixed | codemode | 0/5 | 2.6 (2.3-3.1) | 4.0 (3.5-4.3) | 3.4 (3.2-4.6) | 36.4 (36.2-36.9) | 39.8 (39.3-40.4) | 10 ms | 21 |
| triage-large/mixed | codemode-c4 | 0/5 | 2.2 (2.1-3.1) | 3.6 (3.4-4.5) | 3.4 (3.1-3.5) | 36.5 (35.9-36.6) | 39.9 (39.3-40.1) | 10 ms | 20 |
| triage-large/mixed | pi | 5/5 | 56.0 (54.3-58.6) | 65.8 (64.1-68.7) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 46 |
| triage-large/mixed | opencode | 5/5 | 82.1 (81.6-82.7) | 107.3 (105.0-112.1) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 79 |
| denied/mixed | codemode | 5/5 | 2.7 (2.4-2.9) | 6.3 (6.0-7.0) | 4.5 (4.4-4.7) | 46.0 (45.7-46.1) | 50.3 (50.1-50.5) | 10 ms | 93 |
| denied/mixed | codemode-c4 | 5/5 | 1.7 (1.5-2.3) | 5.9 (5.2-6.5) | 4.5 (4.4-4.5) | 45.9 (45.8-46.0) | 50.4 (50.2-50.5) | 10 ms | 118 |
| denied/mixed | pi | 5/5 | 55.4 (52.4-58.6) | 66.0 (62.7-69.0) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 72 |
| denied/mixed | opencode | 5/5 | 78.6 (77.9-81.3) | 102.6 (102.4-106.6) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 108 |
| deadline/mixed | codemode | 0/5 | 2.0 (1.6-2.4) | 3.8 (3.6-4.3) | 3.6 (3.5-3.8) | 43.5 (43.1-43.8) | 47.2 (46.6-47.4) | 10 ms | 36 |
| deadline/mixed | codemode-c4 | 0/5 | 1.7 (1.5-2.3) | 3.6 (3.5-4.1) | 3.8 (3.6-3.9) | 43.9 (43.3-44.3) | 47.8 (46.9-48.1) | 10 ms | 36 |
| deadline/mixed | pi | 0/5 | 56.0 (53.5-56.5) | 64.7 (62.5-65.3) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 36 |
| deadline/mixed | opencode | 0/5 | 74.8 (72.4-77.4) | 89.6 (86.2-92.0) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 53 |
| guest-exception/zero | codemode | 0/5 | 4.4 (4.0-4.8) | 6.1 (5.8-6.5) | 3.8 (3.7-3.9) | 44.0 (41.9-45.1) | 47.7 (45.6-49.0) | 10 ms | 15 |
| guest-exception/zero | codemode-c4 | 0/5 | 4.3 (3.8-4.5) | 5.8 (5.3-6.0) | 4.0 (3.9-4.0) | 44.4 (43.2-44.6) | 48.4 (47.1-48.6) | 10 ms | 15 |
| guest-exception/zero | pi | 0/5 | 57.4 (56.3-60.5) | 65.4 (64.7-68.7) | n/a | n/a | 0.0 (0.0-0.0) | 11 ms | 16 |
| guest-exception/zero | opencode | 0/5 | 91.4 (87.0-92.8) | 100.7 (96.3-102.9) | n/a | n/a | 0.0 (0.0-0.0) | 11 ms | 30 |
| lost-response/mixed | codemode | 0/5 | 1.5 (1.5-1.8) | 3.2 (3.1-3.7) | 3.5 (3.4-3.5) | 37.2 (37.0-37.3) | 40.6 (40.5-40.8) | 10 ms | 41 |
| lost-response/mixed | codemode-c4 | 0/5 | 1.7 (1.5-1.9) | 3.5 (3.3-3.6) | 3.4 (3.3-3.6) | 37.2 (36.9-37.5) | 40.7 (40.3-41.2) | 10 ms | 41 |
| lost-response/mixed | pi | 0/5 | 55.9 (54.7-56.1) | 64.3 (62.9-64.5) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 41 |
| lost-response/mixed | opencode | 0/5 | 74.6 (69.3-77.0) | 82.6 (77.0-85.8) | n/a | n/a | 0.0 (0.0-0.0) | 10 ms | 57 |
