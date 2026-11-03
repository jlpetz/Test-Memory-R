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

## How CPUs were picked before the build (`src/cpu_selection.rs`, `src/session.rs:426-533` at 428b234)

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

## Core 0 and Windows: measured (2026-11-03)

The question that decided step 4: does CPU 0 take most of Windows' interrupt and DPC work? Measured on the three
desktops, idle, with perfmon's per-CPU `% Interrupt Time`, `% DPC Time` and `Interrupts/sec` over 30 s: it doesn't,
on any of them.

| Host | CPU | What carries the interrupts |
|---|---|---|
| petz007 | 8600G | CPU 10 the busiest, about 1,150/s; CPU 0 149/s |
| daddy-petz | 5700X | spread over all 16 CPUs, 600-1,150/s each |
| desktop-v6v7goc | 8500G | CPUs 1 and 9 the busiest |

So `skip-cores=N` and `N%` drop the weakest cores from the end of the ranked order, as planned, not core 0.

## What was built (2026-11-03)

One ordered list (`cpu_selection.rs`) replaces the three copies of the skip order (bug 6): `choose` ranks the cores,
leaves the skip out, counts what's left, and picks the threads; the session's count, the selection and the start-up
table (`startup_topology`) all read its `Choice`. Pure functions over `Core`s (`cores` builds them from the topology),
tested on made-up topologies (`synthetic`).

- **The order** (`rank`): by core type first (performance, then efficiency, the higher class first, then unknown),
  then within each L3 group by priority, highest first, ties by core number, the groups interleaved. The groups take
  their turns by their lowest core, alternating NUMA nodes (each node's first group, then each node's second), so a
  count short of a whole round still splits evenly over the nodes (30 threads on the 48xlarge: 15 and 15). That last
  part is the build's choice: the plan said only "interleave the groups".
- **The signals**: `CpuTopologyInfo` gains `priority` (SchedulingClass) and `l3` ((processor group,
  LastLevelCacheIndex), as the index is group-relative), both from one `GetSystemCpuSetInformation` read in
  `detect_cpu_topology`; `core_priorities` reads the field. A core's priority is the highest of its CPUs', its L3 its
  first CPU's.
- **Core types**: the type comes before the priority. On the 8500G `core-priority=hybrid` types by priority, so the
  order and the choice are the same with it or without (tested): it only labels. That holds on a part with one L3, as
  the Phoenix 2 parts it is for have; on one with several (a 7950X given it by mistake), each type takes its own turns,
  so the top-priority cores come first instead of the L3s alternating from the start. With types from the efficiency
  class or AMD's CPUID, performance cores first as before, and a Zen 5 + Zen 5c part's two L3s don't interleave the
  types. The 8500G's warning is an info line now (`detect_cpu_topology_auto`).
- **`skip-cores=`** (`SkipSpec`, also the parameter's parser): `N` and `N%` from the end of the order, `N%` of the
  cores under either cputype (bug 2), never every core; `A-B` core numbers. A count of at least the cores, or a range
  over all of them, stops the run with an error (bug 1).
- **`cpus=`** (`thread_count`): in u128, so `cpus=4294967295%` doesn't overflow; `0` and `0%` one thread, over 100%
  all, as before.
- **`cpu-stride=`**: given, the pool is what the skip leaves, by core number, spaced as before; unset (the config's
  `stride_spec` is an `Option` now) the ranked order. With core types it applies the same way (bug 3), one code path
  for both: the type order gives way to core numbers once a stride is asked for. The skip is the ranked order's
  under a stride too, so `skip-cores=1 cpu-stride=2` leaves out the weakest core, not the last by number.
- **Threads a core** (bug 5): the most the choice put on one core, so `cputype=threads cpus=50%` on SMT doesn't halve
  the cache-sized extents.
- **The start-up screen** (bug 4): `ParamNote::SkipCores` carries the value as given; the selection's line names the
  cores left out ("Skipping core 5, the last in the core order (skip-cores=1), ..."), so `Setup::CpuFilter` went; the
  table marks a range's cores Skipped, and gains OS Priority and L3 Group columns (numbered from the lowest core up)
  and a last line saying how the threads were ordered, one row a CPU as before. The window's Threads tab shows the
  same table.
- Docs: `--help`, `doc/test_parameters.md` (a CPU selection section), `dist/docs/guide.md`; the remote-memory warning
  no longer says to spread the threads with `cpu-stride=even`.

Tests (`cpu_selection::tests`): the desktops' priority vectors, the all-zero server, the m8a.4xlarge (2 L3 groups in
one node: `cpus=50%` 4 + 4), the m8a.48xlarge (2 nodes of 12 groups of 8: 4 a group, 48 a node), SMT on and off,
every skip and `cpus=` form, `cpu-stride=`, core types, each bug as a regression test, and the count, choice and table
agreeing over a matrix of machines and settings. The CLI's goldens render the table from the selection's own code
(`test_support::hybrid_cpu_topology_report`, `two_l3_cpu_topology_report`). On the machine running the tests, the CPU
sets list every CPU and a core's CPUs share their L3 group and priority.

Still to check on hardware: that Windows on the m8a.4xlarge reports its two L3 groups (the start-up table's L3 Group
column, `--startup-debug` shows it without a run), and the desktops' order against their priorities.

## 2026-11-03: checked on hardware, closed

The start-up CPU table of every host in the campaign (a5de854) matched its priorities: the 8600G (3 2 1 4 0 4) put threads
on cores 3, 5, 0, 1, 2, 4; the 5700X (1 6 3 5 6 2 0 4) on 1, 4, 3, 7, 2, 5, 0, 6; the 8500G (1 0 0 0 1 0) on its Zen 4
cores 0 and 4 first. The 2-socket EPYC (m8a.metal-48xl: 192 cores, 2 nodes, 24 L3 groups, all priorities 0) put its first
24 threads one in each L3 group, alternating the nodes, 96 a node.
