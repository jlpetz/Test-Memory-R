# TODO 108: rank the cores by Windows' preference, within each L3 group, instead of typing them

Raised 2026-10-28 by the owner, from the warning a uniform Ryzen 7 8840HS (8 Zen 4 cores) gives on
every run: "Every core is typed the same ... Windows gives cores 1 and 5 a higher priority ... add
core-priority=hybrid". The facts below come from a read of the code and the TODO 96 record
(2026-10-28, three readers, each checked by a skeptic).

## Why

TODO 96 (2026-10-08/09, 7228377, 2546227, 9ce2082) stopped typing cores from Windows' core priority
(the CPU-set SchedulingClass), because on uniform Ryzens it is AMD's preferred-core order: the
8600G read as 2 P + 4 E and the 5700X as 2 + 6, seen in the TODO 93 sweeps' `--show-topology`.
The 8500G's 2 Zen 4 + 4 Zen 4c report nothing else, so `core-priority=hybrid` became opt-in, and
the warning names the top cores whenever only the priority tells cores apart. Most Ryzens have
preferred cores, so most AMD runs warn, for a case only AMD's Phoenix 2 parts (8500G, 8300G,
7545U, 8540U...) need. A clock probe at startup was turned down on its cost (2026-10-09).

The priority is a real ranking, not a flag (higher is preferred; cores 0 up):

| CPU | Priority | Levels | Meaning |
|---|---|---|---|
| 8600G, 6 identical | 3 2 1 4 0 4 | 5 | preferred-core order; clocks within 0.4% |
| 5700X, 8 identical | 1 6 3 5 6 2 0 4 | 7 | the same (not clocked) |
| 8500G, 2 Zen 4 + 4 Zen 4c | 1 0 0 0 1 0 | 2 | core types: 5.2 vs 3.5 GHz (`../coreclock-test/FINDINGS.md`) |
| EPYC and Xeon VMs | all 0 | 1 | nothing |

So ordering by it costs nothing on uniform parts and puts the 8500G's Zen 4 cores first, with no
core mislabelled E. The boost order is fixed for a boot (the owner, 2026-10-28), so it doesn't move
between runs of one boot.

## How CPUs are picked today (`src/cpu_selection.rs`, `src/session.rs:426-533`)

- The list is per physical core, in Windows' record order; each core's CPUs ascend (`cpu_topology.rs`
  sorts by group*64+bit). `cputype=cores` takes each core's first CPU; `cputype=threads` takes the
  same first CPUs, then the siblings of the chosen cores. Siblings already stay paired.
- `skip-cores` (default 1, "system responsiveness") drops the lowest-numbered cores; `N%` and `A-B`
  are its other forms. `cpus=` takes from the front of what's left; `cpu-stride` spaces the picks by
  position (TODO 30: the m8a.4xlarge's cores 0-7 and 8-15 are separate ~40 GB/s domains in one
  NUMA node).
- NUMA nodes never choose CPUs; each thread's memory node follows its CPU.
- With core types (Intel's efficiency class, AMD's Zen 5c CPUID, `core-priority=hybrid`), a second
  path skips E-cores first and orders P before E, and ignores `cpu-stride`.

## The change (the owner, 2026-10-28)

1. Rank the cores within each group sharing an L3 (the CPU-set LastLevelCacheIndex, which TMR
   doesn't read yet; group-relative, so keyed by (group, index)), by priority, highest first, ties by
   core number. Not by NUMA node: the m8a.4xlarge's two domains are one node.
2. Interleave the groups: the best of each group in turn, then the second of each, and so on.
3. `cpus=` takes from the front: balanced across domains, strongest first within each. This does
   what `cpu-stride=even` was added for, and also where it fails today (the default `skip-cores=1`
   makes `even` pack: `cpus=50%` puts 7 + 1 on the 16-core VM's domains, and 95 + 1 on the
   48xlarge's nodes).
4. `skip-cores=N` (and `N%`) drop from the end: the weakest cores, not core 0.
5. `skip-cores=A-B` keeps working on core numbers.

Then the 8500G's default run leaves out a Zen 4c core, uniform parts just reshuffle identical
cores, servers (all ties) keep core-number order, and the warning becomes an info line;
`core-priority=hybrid` affects only the P/E labels. What `cpu-stride` means on the new order is to
settle when built. Not concerns (the owner): `--compare-results` flagging "CPU placement" against
older files once, and default runs, TM5 `.cfg` runs, `--cache-latency` and `--ram-latency` taking
different cores.

## Bugs found by the read (fix with this, or before it)

1. Two crashes (read in the code, not run): `skip-cores=50%` or more under the default
   `cputype=threads` on 2-way SMT, and `skip-cores=N` with N at least the core count, leave an
   empty CPU list, and the run divides by its thread count instead of stopping with an error.
2. `skip-cores=N%` in threads mode takes the percentage of logical CPUs and removes that many
   cores: 25% on 6 cores / 12 threads removes 3 (`session.rs:428-439`, `resolve_skip_spec`).
3. With core types, `cpu-stride` is ignored without a word (`cpu_selection.rs:142-197`).
4. The start-up screen says "Skipping first 1 CPU(s)" for `skip-cores=0` (`ParamNote::SkipCores`
   is recorded before the spec resolves, `session.rs:855`), and shows cores `skip-cores=A-B` drops
   as available (`cpu_topology.rs:660-705`).
5. `cputype=threads` halves the cache-sized extents (`set_active_threads_per_core(2)`,
   `session.rs:579-581`) even when the selection put one thread per core (`cpus=50%`).
6. The skip order is written three times, already out of step: the count (`session.rs:463-501`),
   the selection (`cpu_selection.rs`) and the start-up table (`cpu_topology.rs:660-705`). One
   ordered list should replace them.

## To find out first

Core 0 and Windows: does CPU 0 take most of the interrupt and DPC work on these desktops? If it
does, leaving core 0 free may matter more than leaving the weakest core free, and step 4 would keep
the skip at core 0. Per-CPU interrupt and DPC time while idle and under a TMR run (perfmon's
Processor counters, or `xperf`) answers it.
