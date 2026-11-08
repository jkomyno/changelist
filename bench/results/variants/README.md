# Interleaved codemode builds

Three codemode builds measured in one benchmark process, round-robin with a
rotating start, so machine drift affects them equally. Separate runs of the same
build differed by about 2 percent on latency-bound cells and up to 20 percent on
some zero-latency cells, which is larger than the effects measured here.

| Engine | Build | Bridge client |
| --- | --- | --- |
| `cm-A` | `8cb7a63` (trace schema 1, bridge protocol 1) | from `8cb7a63` |
| `cm-B` | `de86444` (trace schema 2, host facts, inspection) | from `de86444` |
| `cm-C` | `286b88d` (B plus the frame-copy fixes) | from `286b88d` |

Each engine is `bench/engines/codemode.ts` with its binary path and bridge-client
import pointed at that build; the harness, fixtures, scenarios, and limits are
those of `8cb7a63`. The run used `--reps 20 --warmup 3 --cold 0` over every
non-holdout scenario. `runs.jsonl.gz` holds every run; `summary.md` the tables.

Warm task p50 differences, in ms:

| Cell | B − A (trace v2) | C − B (frame copies) |
| --- | ---: | ---: |
| triage, zero | 0.0 | −3.2 |
| triage, equal | +1.8 | −1.4 |
| triage, mixed | +7.5 | −5.8 |
| denied, zero | +1.1 | −4.5 |
| denied, equal | +2.7 | −3.7 |
| denied, mixed | +1.9 | −3.6 |
| guest-exception, zero | +0.5 | −2.9 |
| deadline, mixed | −0.2 | +0.2 |
| lost-response, mixed | −0.2 | −0.2 |

The trace and host facts cost no more than the run-to-run spread, except
triage under mixed latency at p50 (+7.5 ms; its p95 fell 1.7 ms). The frame-copy
fixes saved 1 to 6 ms where results crossed the guest boundary.
