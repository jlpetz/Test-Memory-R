# TODO 103. [TMR-APP] Potential load improvement for the AMX tile test

**Raised** 2026-10-24 by the owner, from plumb TODO 13 (raised there 2026-10-23, "to think on").
Low priority, for later consideration.

## The gap

- Mem-TileTransform (TODO 102) runs three phases on each chunk:
  - W stores the pattern tiles.
  - RW multiplies each tile in place by the transform matrix, one multiply per tile.
  - R checks each group's checksum, with `1 + r` multiplies per tile.
- The load setting `tile_load` (r) affects R alone.
- The test checks that memory holds up while the CPU is busy
  (`../fearlessD/design/amx-transform-verify.md`). Under load, though, the matrix unit is busy in R
  and nearly idle in W and RW, so the load comes in bursts, in one phase out of three.

## Measured (plumb, 2026-10-23)

Bench `xformw`/`xformrw`/`xformr`, DRAM, one thread, each phase's share of a pass:

| r | W | RW | R | pass, measured | pass, from the shares |
|---|---|---|---|---|---|
| 0 | 34% | 30% | 36% | 17.09 GiB/s | 17.20 GiB/s |
| 8 | 26% | 23% | 51% | 13.36 GiB/s | 13.33 GiB/s |

At r = 8 the matrix unit is busy for about half the pass. The shares predict the whole pass to
within 1%, so a phase timed on its own predicts what a change does to the pass.

## Options

1. **r in RW and W too.**
   - **RW.** RW's result is the stored data, so extra multiplies have to accumulate into a spare
     tile. They can be checked the way R's are. Repeating the tile's multiply by the resident
     transform matrix (T7) r times gives a group sum of r times the sum of the set's `T(P_i)`. That
     is a constant, like R's `(1 + r) E`. As with R's extra multiplies, this checks the matrix
     unit's arithmetic on a tile loaded once. It doesn't add memory reads.
   - **RW's tile budget.** RW's read-ahead holds 7 of the 8 tiles: T7 is the matrix, T0 and T1 are
     the results, and T2-T5 hold the loads ahead. Only T6 is free, which allows one accumulator
     chain against R's two. Two chains would mean dropping the read-ahead, which was worth 5-7% in
     DRAM in plumb.
   - **W.** W's data comes from the pattern table, which sits in cache. Extra multiplies there load
     the unit, and their check covers the unit rather than memory. W uses only T0 and T1, so there
     are tiles to spare.
   - **The cost.** W stores at fill speed and RW reads ahead. Extra multiplies slow both, so the
     test moves less memory per second. Time W and RW alone at r = 8 with the extra multiplies, and
     the shares above give the pass.
   - **The knob.** Either one key (`tile_load` for every phase) or one per phase.
2. **An AMX load thread on a neighbouring core.** This is plumb's bench `--load amx`. It gives
   steady load whatever the phase. But each core has its own matrix unit, so it loads the package's
   power and heat, not the unit on the core under test.
3. **Leave the bursts as they are.**

## Next

Choose an option. For option 1, measure W and RW with the extra multiplies before building it in.
