# TODO 110: the window's calibrations

The owner, 2026-10-31, during TODO 109's second round: "We should show if it has calibration data (none / basic / extended), and
allow the user to run a calibration from the GUI too. That can maybe be a separate todo after this current work. Or merged into the
multi thread calibration existing todo."

## Why apart from TODO 22

TODO 22 is a new measurement (every thread at once, a third stored variant, `calibration_mt`), engine work and parked. This item is
the window's side of the calibrations that exist. Once TODO 22 lands, its variant is one more state here and one more command; nothing
here waits on it.

## What there is now (2026-10-31)

- The calibrations are stored in `tmr-cfg.json` (`app_config.rs`): `calibration` (standard, the owner's "basic"),
  `calibration_extended`, and the seal's width. `get_best_calibration` prefers extended. Both are tied to the machine's identity
  (`is_calibration_valid`): one stored for other hardware is stale.
- The session reads them at setup (`session.rs`, `Setup::CacheCalibration`, `Setup::SealCalibration`), and the window's System tab
  shows the result (`sections.rs`): loaded (standard or extended, with its date), none, stale, off (`--no-calibration`). That is only
  once a run has started; before Start the window doesn't say.
- `--calibrate-cache`, `--calibrate-cache-ext`, `--calibrate-seal`, `--calibrate-all` are one-shot commands (`commands.rs`). They
  write to a `dyn Write` and ask whether to replace a stored calibration (`prompt_calibration_save`; `--no-save` keeps what is stored).
  The window turns them away: "A one-shot command (--help, --calibrate-cache, ...) runs on the console" (`model.rs`).

## Wanted

1. The state as the window opens, read from `tmr-cfg.json` without a run: cache none / standard / extended / stale, with the date;
   the seal's width or none. On the Run tab or the System tab (to decide), refreshed after a calibration.
2. A Calibrate command (cache, extended, seal, all) that runs in the window on a thread, its output shown as it comes, the
   replace-or-keep question a dialog, the state refreshed when it ends.

## To decide

- Where the output goes: the Log tab (it has the transcript already), or a pane of its own.
- A calibration and a run don't overlap: they'd measure each other. Start is off while one calibrates, and Calibrate while a run runs.
- Whether the window offers to calibrate when there's none (a first run), or only says so.
