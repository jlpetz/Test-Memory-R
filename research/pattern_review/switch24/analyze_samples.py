"""Summarize the bounded CPU samples; the first sample can follow a test-name prefix."""

import json
from pathlib import Path
import re
import statistics

HERE = Path(__file__).resolve().parent


def main():
    groups = {}
    samples = re.findall(
        r"sample,(\d+),(\d+),(\d+),(\w+),(verify|fill),([\d.]+)",
        (HERE / "cpu-samples.txt").read_text(encoding="utf-8"),
    )
    for kib, _block, width, name, action, value in samples:
        groups.setdefault(f"{kib}/{width}/{name}/{action}", []).append(float(value))
    assert len(samples) == 540 and len(groups) == 60
    rows = []
    for key, values in groups.items():
        assert len(values) == 9
        rows.append({
            "key": key, "n": len(values), "median": statistics.median(values),
            "min": min(values), "max": max(values),
        })
    (HERE / "cpu-summary.json").write_text(json.dumps(rows, indent=2) + "\n")
    for row in rows:
        if "/256/" in row["key"] and any(f"/{name}/" in row["key"] for name in ("shared", "lookup")):
            print(row)


if __name__ == "__main__":
    main()
