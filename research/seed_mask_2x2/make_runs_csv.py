"""Collect the seed-mask A/B and 2x2 runs into runs.csv, one row per run.

Usage: python -I make_runs_csv.py <5700X run-log dir> <8600G run-log dir>
The run logs are TMR's console output, one file per run, named <set>_b<width>_<build>_<n>.txt
(ab4, ab5, ab7, ab8) or r6_b<width>.txt. They stay outside the repo.
"""
import csv
import glob
import os
import re
import sys

TESTS = ["Bench-Init-TMR-4", "Bench-Verify-TMR-4", "Bench-Init-Switch", "Bench-Verify-Switch"]
NAME = re.compile(r"^(ab4|ab5|ab7|ab8)_b(\d+)_(r\da?)_(\d+)\.txt$|^(r6)_b(\d+)\.txt$")


def rows(cpu, folder):
    for path in sorted(glob.glob(os.path.join(folder, "*.txt"))):
        m = NAME.match(os.path.basename(path))
        if not m:
            continue
        if m.group(1):
            run_set, width, build, n = m.group(1), int(m.group(2)), m.group(3), int(m.group(4))
        else:
            run_set, width, build, n = "r6", int(m.group(6)), "r6", 1
        text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", open(path, encoding="utf-8", errors="replace").read())
        speeds = dict(re.findall(r"Test report - Cycle 1 - Step \d+ (Bench-\S+): \S+ [\d.]+s, \d+ errors[^@]*@ ([\d.]+) MiB/s", text))
        page = re.search(r"Running Bench-Verify-Switch on \d+ MiB \(extent \d+ MiB\): \d+ MiB (\S+)", text)
        start = re.search(r"\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)", text)
        yield {
            "cpu": cpu, "set": run_set, "n": n, "build": build, "width": width,
            **{t: speeds.get(t, "") for t in TESTS},
            "failed_checks": len(re.findall(r"the check's differences, ORed", text)),
            "pages": page.group(1) if page else "",
            "start": start.group(1) if start else "",
        }


out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs.csv")
with open(out, "w", newline="", encoding="utf-8") as f:
    w = csv.DictWriter(f, fieldnames=["cpu", "set", "n", "build", "width", *TESTS, "failed_checks", "pages", "start"])
    w.writeheader()
    for cpu, folder in (("5700X", sys.argv[1]), ("8600G", sys.argv[2])):
        for row in sorted(rows(cpu, folder), key=lambda r: (r["set"], r["n"], r["width"])):
            w.writerow(row)
print("wrote", out)
