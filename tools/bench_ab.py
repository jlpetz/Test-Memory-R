"""Run TMR binaries alternately and compare their per-test speeds (TODO 99).

Usage: python tools/bench_ab.py OUTDIR REFERENCE "TMR ARGS" label=path/to/tmr.exe label2=... [--runs N]
Runs each binary N times, alternating (A B A B ...), from the TMR-APP directory, and saves each
run's console output in OUTDIR. Each run's speeds come from its results file: a test's MiB/s
averaged over its cycles. REFERENCE is a test whose code the change doesn't touch: each test's
speed is also given relative to it in the same run, so the box's state cancels out.

Give the same binary two labels (copy it under a second name) for the noise floor: an A/A run's
"vs ref" column is how far apart two runs of identical code land on this box.
Build nothing while it runs, and keep the box idle.

A test one binary lacks (renamed or removed) shows as "-" in its columns. `--reparse` rebuilds the
table from OUTDIR's saved run outputs (each `<label>_<n>.txt` and the results file it names) without
running anything, e.g. after a crash or to re-read an old comparison.

On a test host: add `--host LABEL --scripts DIR`, where DIR holds the host's run-tmr.ps1 and
fetch.ps1 (kept outside the repo, with the host names); each label's path is then the binary's
name as deployed there (`r2=tmr-r2.exe`). Each run's output and results file are copied into OUTDIR.
"""
import json
import os
import re
import statistics
import subprocess
import sys

APP = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def ps(script, *args):
    """Runs one of the host's PowerShell scripts; its output, or an exit with it on failure."""
    proc = subprocess.run(["powershell", "-NoProfile", "-File", script, *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace")
    if proc.returncode != 0:
        sys.exit(f"{os.path.basename(script)} failed: {proc.stdout[-2000:]}{proc.stderr[-2000:]}")
    return proc.stdout


def speeds_of(path):
    """Each test's MiB/s in a results file, averaged over its cycles, keyed by label."""
    with open(path, encoding="utf-8") as f:
        result = json.load(f)
    speeds = {}
    for cycle in result["cycles"]:
        for test in cycle["tests"]:
            # The label tells a .cfg's steps apart ("Mem-SimpleV2-512_A (Test 6)"); the name doesn't
            speeds.setdefault(test.get("label") or test["name"], []).append(test["throughput_mib_s"])
    return {name: statistics.mean(v) for name, v in speeds.items()}


def reparse(out_path, host):
    """A saved run's speeds, from its console output's results file."""
    m = re.search(r"Results saved: (\S+\.json)", open(out_path, encoding="utf-8", errors="replace").read())
    if not m:
        sys.exit(f"no results file named in {out_path}")
    # A host's results were fetched beside the output; a local run's are under the app
    local = os.path.join(os.path.dirname(os.path.abspath(out_path)), os.path.basename(m.group(1)))
    return speeds_of(local if host or os.path.exists(local) else os.path.join(APP, m.group(1))), m.group(1)


def run(exe, args, out_path, host=None):
    if host:
        label, scripts = host
        name = os.path.splitext(os.path.basename(out_path))[0]
        outdir = os.path.dirname(os.path.abspath(out_path))
        ps(os.path.join(scripts, "run-tmr.ps1"), "-HostLabel", label, "-Name", name, "-Exe", exe,
           "-ArgLine", args, "-TimeoutMin", "60", "-LocalDir", outdir)
        stdout = open(out_path, encoding="utf-8", errors="replace").read()
    else:
        proc = subprocess.run([exe] + args.split(), cwd=APP, capture_output=True, text=True, encoding="utf-8", errors="replace")
        stdout = proc.stdout
        with open(out_path, "w", encoding="utf-8") as f:
            f.write(proc.stdout)
            f.write(proc.stderr)
    m = re.search(r"Results saved: (\S+\.json)", stdout)
    if not m:
        sys.exit(f"{exe}: no results file; see {out_path}")
    if host:
        ps(os.path.join(host[1], "fetch.ps1"), "-HostLabel", host[0], "-Dir", m.group(1).replace("/", "\\"),
           "-LocalDir", os.path.dirname(os.path.abspath(out_path)))
        path = os.path.join(os.path.dirname(os.path.abspath(out_path)), os.path.basename(m.group(1)))
    else:
        path = os.path.join(APP, m.group(1))
    return speeds_of(path), m.group(1)


def main():
    argv = sys.argv[1:]
    opts = {}
    for flag in ("--runs", "--host", "--scripts"):
        if flag in argv:
            i = argv.index(flag)
            opts[flag] = argv[i + 1]
            del argv[i:i + 2]
    runs = int(opts.get("--runs", 3))
    host = (opts["--host"], opts["--scripts"]) if "--host" in opts else None
    again = "--reparse" in argv
    argv = [a for a in argv if a != "--reparse"]
    if len(argv) < 4:
        sys.exit(__doc__)
    outdir, reference, args = argv[0], argv[1], argv[2]
    bins = [a.split("=", 1) for a in argv[3:]]
    os.makedirs(outdir, exist_ok=True)
    seen = {label: [] for label, _ in bins}
    for r in range(runs):
        for label, exe in bins:
            out = os.path.join(outdir, f"{label}_{r + 1}.txt")
            speeds, path = reparse(out, host) if again else run(exe, args, out, host)
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
            if not values:
                # This binary has no such test (renamed or removed)
                means.append(float("nan"))
                rel.append(float("nan"))
                row += f" {'-':>22s}"
                continue
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
