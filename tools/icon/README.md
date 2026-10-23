# The icon: `tmr.ico` and the DIMM artwork

TMR's icon is a DDR5 UDIMM with its heat spreader off, on a black board, its traces lit red like
neon (TODO 7, the owner's brief, 2026-10-23). Two scripts make it; neither is part of the build,
which only reads the result, `crates/tmr-app/tmr.ico`.

- `dimm.py DIR` writes the SVGs into `DIR`: `dimm-burn.svg`, the artwork at the module's real
  proportions (133.35 x 31.25 mm, 4 + 4 chips, the PMIC, 144 fingers a side), and `icon<px>.svg`
  for each icon size. 256 px is vector; 96 px and below are drawn on the pixel grid, one-pixel
  lines on whole pixels, so nothing blurs. It also writes `preview.html` (the artwork and every
  size, on dark and light) and `pixels.html` (each pixel-grid size, 96 px and below, rasterised,
  then magnified).
- `pack.py DIR` rasterises those SVGs with headless Edge and packs `DIR/tmr.ico`: 14 sizes (256,
  96, 80, 72, 64, 60, 48, 40, 36, 32, 30, 24, 20, 16 px), each a 32-bit PNG. It also writes
  `icon<px>.png` for each size and `dimm-burn@2x.png`, the artwork at twice its size.

`svg/` holds what `dimm.py` drew last, committed (the owner's: the sources beside the icon they
make), with its two pages: open `svg/preview.html` to see them. Draw into it, so `git diff` shows
what a change did, then pack and copy the icon; `pack.py`'s PNGs, `render.html` and `tmr.ico` in
`svg/` are ignored (the icon is committed once, in `crates/tmr-app/`, and its PNGs are in it):

```bash
python tools/icon/dimm.py tools/icon/svg
python tools/icon/pack.py tools/icon/svg
cp tools/icon/svg/tmr.ico crates/tmr-app/tmr.ico
```

The build embeds `tmr.ico` as the exe's icon (`crates/tmr-app/build.rs`), and the window shows it in
its title bar and on the taskbar (`crates/tmr-gui/src/window.rs`).

**Checks.** `dimm.py` asserts as it draws: in the vector modules, no two lit traces closer than half
a stroke, edge to edge, no bus riser within a stroke of a part, and no lit trace through another
line's via; on the pixel grid, every lane ending on a finger, none in the key, and no two parts of
a chip in one column. A layout change that breaks one fails the run. The artwork's stitching vias
are only placed where they clear every lit trace by a stroke, so a change drops one rather than
failing: count them after a change to the bus or the lanes (72 now, one over each finger no signal
takes).
Look at `preview.html` and `pixels.html` after any change: the asserts can't see how it reads.

**Choices** (the owner's, and the review rounds'): the neon is red, not orange; no silkscreen
text, only TMR turned upright on the last chip, at 256 and 96 px and in the artwork; the chips'
markings run vertically; a top bus line at every size; a pair of modules from 20 px up, the one
behind darker, and one module at 16 px, where a pair would leave one-pixel chips.
