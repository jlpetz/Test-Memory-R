"""Run TMR binaries alternately and compare their per-test speeds (TODO 99).

Usage: python tools/bench_ab.py OUTDIR REFERENCE "TMR ARGS" label=path/to/tmr.exe label2=... [--runs N]
Runs each binary N times, alternating (A B A B ...), from the TMR-APP directory, and saves each
run's console output in OUTDIR. Each run's speeds come from its results file: a test's MiB/s
averaged over its cycles. REFERENCE is a test whose code the change doesn't touch: each test's
speed is also given relative to it in the same run, so the box's state cancels out.

Give the same binary two labels (copy it under a second name) for the noise floor: an A/A run's
"vs ref" column is how far apart two runs of identical code land on this box.
Build nothing while it runs, and keep the box idle.
"""
import json
import os
import re
import statistics
import subprocess
import sys

APP = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(exe, args, out_path):
    proc = subprocess.run([exe] + args.split(), cwd=APP, capture_output=True, text=True, encoding="utf-8", errors="replace")
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(proc.stdout)
        f.write(proc.stderr)
    m = re.search(r"Results saved: (\S+\.json)", proc.stdout)
    if not m:
        sys.exit(f"{exe}: no results file (exit {proc.returncode}); see {out_path}")
    with open(os.path.join(APP, m.group(1)), encoding="utf-8") as f:
        result = json.load(f)
    speeds = {}
    for cycle in result["cycles"]:
        for test in cycle["tests"]:
            speeds.setdefault(test["name"], []).append(test["throughput_mib_s"])
    return {name: statistics.mean(v) for name, v in speeds.items()}, m.group(1)


def main():
    argv = sys.argv[1:]
    runs = int(argv[argv.index("--runs") + 1]) if "--runs" in argv else 3
    argv = [a for i, a in enumerate(argv) if a != "--runs" and (i == 0 or argv[i - 1] != "--runs")]
    if len(argv) < 4:
        sys.exit(__doc__)
    outdir, reference, args = argv[0], argv[1], argv[2]
    bins = [a.split("=", 1) for a in argv[3:]]
    os.makedirs(outdir, exist_ok=True)
    seen = {label: [] for label, _ in bins}
    for r in range(runs):
        for label, exe in bins:
            speeds, path = run(exe, args, os.path.join(outdir, f"{label}_{r + 1}.txt"))
            seen[label].append(speeds)
            print(f"run {r + 1} {label}: {path}", flush=True)
    tests = sorted({t for runs_ in seen.values() for s in runs_ for t in s})
    print()
    header = f"{'test':28s}" + "".join(f" {label + ' MiB/s':>22s}" for label, _ in bins)
    if len(bins) == 2:
        header += f" {'raw':>7s} {'vs ref':>7s}"
    else:
        # Each binary after the first against the first, raw and relative to the reference
        header += "".join(f" {label + ' raw':>10s} {label + ' ref':>10s}" for label, _ in bins[1:])
    print(header)
    for test in tests:
        row = f"{test[:28]:28s}"
        means, rel = [], []
        for label, _ in bins:
            values = [s[test] for s in seen[label] if test in s]
            rels = [s[test] / s[reference] for s in seen[label] if test in s and reference in s and s[reference] > 0]
            mean = statistics.mean(values)
            means.append(mean)
            rel.append(statistics.mean(rels) if rels else float("nan"))
            row += f" {mean:9.0f} ({min(values):5.0f}-{max(values):5.0f})"
        if len(bins) == 2:
            row += f" {means[1] / means[0]:7.3f} {rel[1] / rel[0]:7.3f}"
        else:
            row += "".join(f" {means[i] / means[0]:10.3f} {rel[i] / rel[0]:10.3f}" for i in range(1, len(bins)))
        print(row)


if __name__ == "__main__":
    main()
