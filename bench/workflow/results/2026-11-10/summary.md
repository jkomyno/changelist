# Workflow comparison results, 2026-11-10

codemode `077dcab`; Pi Durable 1.5.0; OpenCode codemode 2.2.6; Node v24.22.1; Apple M1 Pro, 10 CPUs, 32 GiB, macOS 14.4.1 (Darwin 23.4.0). Load at start: 1.86 2.25 2.14. Protocol and hypotheses: [README](../../README.md) (SHA-256 `7e164234466c`).

## Latency, no-fault workflow (ms, process start to exit of the last attempt)

| Row | Implementation | n | p25 | p50 | p75 | p95 | startup p50 | peak RSS MiB (sampled) | durable records |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| host-default | opencode | 15 | 767.2 | 774.4 | 776.5 | 782.2 | 249 | 174 | 0 |
| host-default | composio-script | 15 | 1217.7 | 1221.6 | 1226.5 | 1237.4 | 94 | 121 | 23 |
| host-default | codemode | 15 | 861 | 868.1 | 872.8 | 895.1 | 98 | 152 | 25 |
| host-default | pi-durable | 15 | 812.2 | 814.3 | 820.5 | 833.9 | 230 | 191 | 104 |
| equal-limits | opencode | 15 | 766.2 | 771.1 | 777.5 | 783.3 | 250 | 168 | 0 |
| equal-limits | composio-script | 15 | 1218.4 | 1221.3 | 1227.4 | 1230.8 | 94 | 120 | 23 |
| equal-limits | codemode | 15 | 1307.6 | 1315.1 | 1322.1 | 1336.6 | 98 | 152 | 25 |
| equal-limits | pi-durable | 15 | 802 | 812.5 | 816.5 | 819.1 | 230 | 191 | 104 |

| Row | codemode vs | Result (tie rule) |
| --- | --- | --- |
| host-default | pi-durable | pi-durable faster |
| host-default | opencode | opencode faster |
| host-default | composio-script | codemode faster |
| equal-limits | pi-durable | pi-durable faster |
| equal-limits | opencode | opencode faster |
| equal-limits | composio-script | composio-script faster |

## Fault scenarios

Verdict counts per cell; flags count runs with that flag. `rec` is recovery requests (median/max).

| Scenario | Row | Implementation | n | Verdicts | Unsafe dup | Unjust. success | Unjust. non-effect | Failed but applied | Unsafe send | Review | Fault op: first reported claim (attempt:claim) | Fault op: final claim | rec | p50 ms |
| --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- | ---: |
| lost-response | host-default | codemode | 5 | safe-incomplete 4, correct 1 | 0 | 0 | 0 | 0 | 0 | 4 | 1:applied 5 | applied 5 | 3/3 | 1533.9 |
| lost-response | host-default | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 1/1 | 1007.2 |
| lost-response | host-default | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 1:not reported 5 | not reported 5 | 1/1 | 1121.7 |
| lost-response | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1358.9 |
| lost-response | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1515.4 |
| lost-response | equal-limits | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 1/1 | 1008.2 |
| lost-response | equal-limits | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 1:not reported 5 | not reported 5 | 1/1 | 1124 |
| lost-response | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1355.9 |
| kill-before-first-write | host-default | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 3/3 | 1640.7 |
| kill-before-first-write | host-default | pi-durable | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 997.8 |
| kill-before-first-write | host-default | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1167.3 |
| kill-before-first-write | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1353.1 |
| kill-before-first-write | equal-limits | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 3/3 | 1972.5 |
| kill-before-first-write | equal-limits | pi-durable | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 997.5 |
| kill-before-first-write | equal-limits | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1158.7 |
| kill-before-first-write | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1353.4 |
| kill-after-first-write | host-default | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 3/3 | 1587.7 |
| kill-after-first-write | host-default | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 1021.3 |
| kill-after-first-write | host-default | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1143.9 |
| kill-after-first-write | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1293.3 |
| kill-after-first-write | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1502.4 |
| kill-after-first-write | equal-limits | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 1017.6 |
| kill-after-first-write | equal-limits | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1133.9 |
| kill-after-first-write | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 1297.1 |
| late-effect | host-default | codemode | 5 | safe-incomplete 4, correct 1 | 0 | 0 | 0 | 0 | 0 | 4 | 1:unknown 5 | applied 5 | 3/3 | 1533 |
| late-effect | host-default | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 1/1 | 1008.1 |
| late-effect | host-default | opencode (rerun) | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1163.2 |
| late-effect | host-default | composio-script | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1395.4 |
| late-effect | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 1:unknown 5 | applied 5 | 2/2 | 1758.6 |
| late-effect | equal-limits | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 1/1 | 1004.2 |
| late-effect | equal-limits | opencode (rerun) | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1165.2 |
| late-effect | equal-limits | composio-script | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1403.9 |
| scope-changed | host-default | codemode | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 0/0 | 821.5 |
| scope-changed | host-default | pi-durable | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 0/0 | 815.6 |
| scope-changed | host-default | opencode (rerun) | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 0/0 | 776.1 |
| scope-changed | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 0/0 | 1102.9 |
| scope-changed | equal-limits | codemode | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 0/0 | 1220.6 |
| scope-changed | equal-limits | pi-durable | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 0/0 | 812.8 |
| scope-changed | equal-limits | opencode (rerun) | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 0/0 | 772.3 |
| scope-changed | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 0/0 | 1096.4 |
| account-revoked | host-default | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 702.7 |
| account-revoked | host-default | pi-durable | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 5 | - | - | 1/1 | 970.2 |
| account-revoked | host-default | opencode (rerun) | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 737.7 |
| account-revoked | host-default | composio-script | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 620.2 |
| account-revoked | equal-limits | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 786.6 |
| account-revoked | equal-limits | pi-durable | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 5 | - | - | 1/1 | 969.9 |
| account-revoked | equal-limits | opencode (rerun) | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 1/1 | 733.7 |
| account-revoked | equal-limits | composio-script | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 620.4 |
| account-switched | host-default | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 672.2 |
| account-switched | host-default | pi-durable | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 5 | - | - | 1/1 | 1007.8 |
| account-switched | host-default | opencode (rerun) | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 1/1 | 1128.9 |
| account-switched | host-default | composio-script | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 1/1 | 1353.6 |
| account-switched | equal-limits | codemode | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 5 | - | - | 1/1 | 787.2 |
| account-switched | equal-limits | pi-durable | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 5 | - | - | 1/1 | 1014.4 |
| account-switched | equal-limits | opencode (rerun) | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 1/1 | 1124.1 |
| account-switched | equal-limits | composio-script | 5 | wrong 5 | 0 | 0 | 0 | 0 | 5 | 0 | - | - | 1/1 | 1372.9 |
| ignore-cancel | host-default | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 3536.9 |
| ignore-cancel | host-default | pi-durable | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 3831.9 |
| ignore-cancel | host-default | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 3791.4 |
| ignore-cancel | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 4233.1 |
| ignore-cancel | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | satisfied 5 | 1/1 | 2916 |
| ignore-cancel | equal-limits | pi-durable | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:(nested call cancelled) 5 | (nested call cancelled) 5 | 1/1 | 1823 |
| ignore-cancel | equal-limits | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:not reported 5 | not reported 5 | 1/1 | 2498.8 |
| ignore-cancel | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 2612.8 |
| slow-reply | host-default | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 3479.7 |
| slow-reply | host-default | pi-durable | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 3697.9 |
| slow-reply | host-default | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 3662 |
| slow-reply | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:applied 5 | applied 5 | 0/0 | 4111 |
| slow-reply | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 2746.2 |
| slow-reply | equal-limits | pi-durable | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:(nested call cancelled) 5 | (nested call cancelled) 5 | 0/0 | 1665.2 |
| slow-reply | equal-limits | opencode (rerun) | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:not reported 5 | not reported 5 | 0/0 | 1739 |
| slow-reply | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 2437.8 |
| broken-transport | host-default | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 1102 |
| broken-transport | host-default | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 0:unknown 5 | unknown 5 | 0/0 | 820.7 |
| broken-transport | host-default | opencode (rerun) | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 0:unknown 5 | unknown 5 | 0/0 | 774.2 |
| broken-transport | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 1398.7 |
| broken-transport | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 1557 |
| broken-transport | equal-limits | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 0:unknown 5 | unknown 5 | 0/0 | 814.8 |
| broken-transport | equal-limits | opencode (rerun) | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 0:unknown 5 | unknown 5 | 0/0 | 778.2 |
| broken-transport | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0:unknown 5 | applied 5 | 1/1 | 1402.6 |
| repeated-crash | host-default | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 3/3 | 1460.3 |
| repeated-crash | host-default | pi-durable | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 5/5 | 1505.7 |
| repeated-crash | host-default | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 11/11 | 4743.5 |
| repeated-crash | host-default | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 11/11 | 2434.2 |
| repeated-crash | equal-limits | codemode | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 11/11 | 3486.4 |
| repeated-crash | equal-limits | pi-durable | 5 | safe-incomplete 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 5/5 | 1507.4 |
| repeated-crash | equal-limits | opencode (rerun) | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 11/11 | 4722.9 |
| repeated-crash | equal-limits | composio-script | 5 | correct 5 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 11/11 | 2438 |
| probe-budget | host-default | codemode | 5 | safe-incomplete 4, correct-needs-review 1 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 3/3 | 1582.3 |
| probe-budget | host-default | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 1/1 | 1009.7 |
| probe-budget | host-default | opencode (rerun) | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1165.7 |
| probe-budget | host-default | composio-script | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1403.1 |
| probe-budget | equal-limits | codemode | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 3/3 | 1989.2 |
| probe-budget | equal-limits | pi-durable | 5 | correct-needs-review 5 | 0 | 0 | 0 | 0 | 0 | 5 | 1:unknown 5 | unknown 5 | 1/1 | 1010.5 |
| probe-budget | equal-limits | opencode (rerun) | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1166.9 |
| probe-budget | equal-limits | composio-script | 5 | wrong 5 | 5 | 0 | 0 | 0 | 0 | 0 | 1:applied 5 | applied 5 | 1/1 | 1394.1 |
| widen | host-default | codemode | 1 | correct 1 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 0/0 | 884.6 |
| widen | host-default | pi-durable | 1 | correct 1 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 0/0 | 850.7 |
| widen | host-default | opencode | 1 | correct 1 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 0/0 | 817.9 |
| widen | host-default | composio-script | 1 | correct 1 | 0 | 0 | 0 | 0 | 0 | 0 | - | - | 0/0 | 1301 |

(rerun): OpenCode keeps no durable state; after a kill the driver reran the convergent program from scratch.

## CPU loop and memory growth

| Scenario | Row | Implementation | n | Outcome | Time to stop p50 ms | Peak RSS MiB p50 (sampled) |
| --- | --- | --- | ---: | --- | ---: | ---: |
| cpu-loop | host-default | composio-script | 0 | unsupported: no guest program: the script is the workflow itself | | |
| cpu-loop | host-default | codemode | 5 | stopped by the engine: deadline (5) | 5070.4 | 118 |
| cpu-loop | host-default | pi-durable | 5 | unbounded: harness kill at 20 s (5) | 20004.2 | 151 |
| cpu-loop | host-default | opencode | 5 | unbounded: harness kill at 20 s (5) | 20003.8 | 169 |
| cpu-loop | equal-limits | composio-script | 0 | unsupported: no guest program: the script is the workflow itself | | |
| cpu-loop | equal-limits | codemode | 5 | stopped by the engine: deadline (5) | 3069.4 | 118 |
| cpu-loop | equal-limits | pi-durable | 5 | stopped by the engine: done codemode-error: Script failed Wall time 3.0 seconds Output: Script error: (5) | 3157.9 | 154 |
| cpu-loop | equal-limits | opencode | 5 | stopped by the engine: error TimeoutExceeded: Execution timed out after 3000ms. (5) | 3210 | 139 |
| memory-growth | host-default | composio-script | 0 | unsupported: no guest program: the script is the workflow itself | | |
| memory-growth | host-default | codemode | 5 | stopped by the engine: memory_limit (5) | 777.4 | 361 |
| memory-growth | host-default | pi-durable | 5 | unbounded: harness kill at 20 s (5) | 20037.7 | 1341 |
| memory-growth | host-default | opencode | 5 | unbounded: harness kill at 20 s (5) | 20057.7 | 3566 |
| memory-growth | equal-limits | composio-script | 0 | unsupported: no guest program: the script is the workflow itself | | |
| memory-growth | equal-limits | codemode | 5 | stopped by the engine: memory_limit (5) | 236.3 | 180 |
| memory-growth | equal-limits | pi-durable | 5 | stopped by the engine: done codemode-error: Script failed Wall time 0.3 seconds Output: Script error: (5) | 416.8 | 162 |
| memory-growth | equal-limits | opencode | 5 | host process died (SIGABRT) (5) | 779.7 | 194 |

## Properties

| Implementation | Widened change list runs without re-approval | Inspectable from another process mid-run | Exportable diagnosis after interrupt |
| --- | --- | --- | --- |
| codemode | no | yes 10 | yes 10 |
| pi-durable | yes | raw-storage-only 10 | no 10 |
| opencode | yes | no 10 | no 10 |
| composio-script | yes | yes 10 | yes 10 |

## Hypotheses (README rules applied to these cells)

- H1a vs the script, approval binding: **demonstrated**
- H1b vs the script, lost-comment reconciliation: **demonstrated on safety (4 win(s), 6 tie(s)); completion without a person: codemode more in 2 cell(s), fewer in 1**
- H2 vs Pi Durable, approval binding: **demonstrated**
- H2 vs Pi Durable, contract reconciliation: see per-scenario settlement: codemode settles from provider evidence where Pi reports interrupted; tied on safety; completion without a person: codemode more in 9 cell(s), fewer in 0
- H3 vs OpenCode: OpenCode durable recovery: unsupported (capability); codemode runs in the recovery scenarios are scored below, with OpenCode's rerun-from-scratch alongside; against the rerun: demonstrated on safety (4 win(s), 10 tie(s)); completion without a person: codemode more in 4 cell(s), fewer in 4

Head-to-head: safety by `wrong` runs (fewer wins); completion by `correct` runs (finished without a person).

| Against | Scenario | Row | codemode | Other | Safety | Completion |
| --- | --- | --- | --- | --- | --- | --- |
| composio-script | lost-response | host-default | safe-incomplete 4, correct 1 | correct 5 | tie | fewer |
| composio-script | lost-response | equal-limits | correct 5 | correct 5 | tie | same |
| composio-script | late-effect | host-default | safe-incomplete 4, correct 1 | wrong 5 | win | more |
| composio-script | late-effect | equal-limits | correct 5 | wrong 5 | win | more |
| composio-script | probe-budget | host-default | safe-incomplete 4, correct-needs-review 1 | wrong 5 | win | same |
| composio-script | probe-budget | equal-limits | correct-needs-review 5 | wrong 5 | win | same |
| composio-script | broken-transport | host-default | correct 5 | correct 5 | tie | same |
| composio-script | broken-transport | equal-limits | correct 5 | correct 5 | tie | same |
| composio-script | slow-reply | host-default | correct 5 | correct 5 | tie | same |
| composio-script | slow-reply | equal-limits | correct 5 | correct 5 | tie | same |
| pi-durable | lost-response | host-default | safe-incomplete 4, correct 1 | correct-needs-review 5 | tie | more |
| pi-durable | lost-response | equal-limits | correct 5 | correct-needs-review 5 | tie | more |
| pi-durable | kill-before-first-write | host-default | safe-incomplete 5 | safe-incomplete 5 | tie | same |
| pi-durable | kill-before-first-write | equal-limits | safe-incomplete 5 | safe-incomplete 5 | tie | same |
| pi-durable | kill-after-first-write | host-default | safe-incomplete 5 | correct-needs-review 5 | tie | same |
| pi-durable | kill-after-first-write | equal-limits | correct 5 | correct-needs-review 5 | tie | more |
| pi-durable | late-effect | host-default | safe-incomplete 4, correct 1 | correct-needs-review 5 | tie | more |
| pi-durable | late-effect | equal-limits | correct 5 | correct-needs-review 5 | tie | more |
| pi-durable | repeated-crash | host-default | correct 5 | safe-incomplete 5 | tie | more |
| pi-durable | repeated-crash | equal-limits | correct 5 | safe-incomplete 5 | tie | more |
| pi-durable | probe-budget | host-default | safe-incomplete 4, correct-needs-review 1 | correct-needs-review 5 | tie | same |
| pi-durable | probe-budget | equal-limits | correct-needs-review 5 | correct-needs-review 5 | tie | same |
| pi-durable | broken-transport | host-default | correct 5 | correct-needs-review 5 | tie | more |
| pi-durable | broken-transport | equal-limits | correct 5 | correct-needs-review 5 | tie | more |
| opencode (rerun) | lost-response | host-default | safe-incomplete 4, correct 1 | correct 5 | tie | fewer |
| opencode (rerun) | lost-response | equal-limits | correct 5 | correct 5 | tie | same |
| opencode (rerun) | kill-before-first-write | host-default | safe-incomplete 5 | correct 5 | tie | fewer |
| opencode (rerun) | kill-before-first-write | equal-limits | safe-incomplete 5 | correct 5 | tie | fewer |
| opencode (rerun) | kill-after-first-write | host-default | safe-incomplete 5 | correct 5 | tie | fewer |
| opencode (rerun) | kill-after-first-write | equal-limits | correct 5 | correct 5 | tie | same |
| opencode (rerun) | late-effect | host-default | safe-incomplete 4, correct 1 | wrong 5 | win | more |
| opencode (rerun) | late-effect | equal-limits | correct 5 | wrong 5 | win | more |
| opencode (rerun) | repeated-crash | host-default | correct 5 | correct 5 | tie | same |
| opencode (rerun) | repeated-crash | equal-limits | correct 5 | correct 5 | tie | same |
| opencode (rerun) | probe-budget | host-default | safe-incomplete 4, correct-needs-review 1 | wrong 5 | win | same |
| opencode (rerun) | probe-budget | equal-limits | correct-needs-review 5 | wrong 5 | win | same |
| opencode (rerun) | broken-transport | host-default | correct 5 | correct-needs-review 5 | tie | more |
| opencode (rerun) | broken-transport | equal-limits | correct 5 | correct-needs-review 5 | tie | more |

## Harness checks

```json
{
  "no_fault_identical_final_state": {
    "runs": 120,
    "distinct_final_states": 1,
    "pass": true
  },
  "blind_retry_flagged_unsafe_duplicate": {
    "verdict": "wrong",
    "duplicates": [
      {
        "op": "comment-1",
        "effects": 2
      }
    ],
    "pass": true
  }
}
```

