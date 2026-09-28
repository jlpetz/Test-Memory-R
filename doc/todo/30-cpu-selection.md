# TODO 30. [TMR-APP] Two-Stage CPU Selection (skip-cores filter + cpu-stride spacing)

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

**Status**: DONE (2026-07-29, commit 2c8f399)

CPU selection is now an explicit two-stage pipeline, extracted from `main.rs` into a new
documented, unit-tested `src/cpu_selection.rs`:

```
All cores ──[Stage 1 FILTER]──▶ Available ──[Stage 2 COUNT+SPACING]──▶ Assigned
              skip-cores=                     cpus= + cpu-stride=
```

- **Stage 1 `skip-cores=`** (extended): `N` (first N, unchanged default) | `N%` (leading
  percent — portable across core counts) | `A-B` (exclude an inclusive core-id range, e.g.
  `skip-cores=0-7` tests only cores 8+ to isolate one memory domain).
- **Stage 2 `cpu-stride=`** (new): `1` (packed, previous behaviour) | `N` (every Nth) |
  `even` (spread the requested count across the whole pool).
- **Errors, never silently right-sizes**, when `count × stride` overflows the pool — the
  `cpus=` percentage is the intended way to scale down.
- `cpus=%` stays **relative to the post-filter Available pool** (decision: any value ≤100% is
  then always valid, keeping configs portable from 4- to 16-core machines). The relative math
  is now printed: `CPU Count: 50% of 16 available = 8 thread(s)`.
- `cputype=cores|threads` remains a Stage-1 prefilter, so stride operates on physical cores
  after SMT filtering (`cpu-stride=2` = "every other core", not "every other SMT sibling").

**Why (measured)**: on a 16-core EPYC 9R45 (AWS m8a.4xlarge) cores 0-7 and 8-15 sit on
separate ~40 GB/s memory-bandwidth domains; per-thread bandwidth = `pool ÷ threads in that
pool`. Dense packing loaded the domains unevenly and produced 2-3× bimodal per-thread results
that looked like a bug but were real hardware behaviour. With `cpus=50% cpu-stride=even` the
assignment becomes `[0,2,4,6,8,10,12,14]` (4 threads per domain): per-thread spread collapsed
from 2× to ~3% (StuckBit) **and** aggregate throughput rose — StuckBit128 51.8k→91.3k MiB/s,
Refresh256 57.8k→66.4k, overall ~60k→67.8k. 0 errors. See `memory/cpu-memory-domains.md`.

`main.rs` 1872→1671 lines; module carries the pipeline rationale + 9 unit tests (portable-%,
even spread across both domains, exact fit, error-not-clamp).

**Not fixable in allocation**: the hypervisor reports a single NUMA node, so there is no OS
API to request domain-local memory — placement control is the only lever. NUMA detection
itself is correct; the VM hides the real topology.
