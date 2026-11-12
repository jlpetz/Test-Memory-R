"""Offline access-order examples; no memory allocation or hardware detection.

Run: python -B research/access_geometry/models.py
The selectors below are deliberately simple mathematical models, not recovered
AMD/Intel maps. Physical line = base + virtual line is assumed for each example.
"""

from collections import Counter
from itertools import chain


def visits(n, period, group):
    """The proposal's complete-group specification."""
    assert n > 0 and group > 0 and period >= group
    assert n % group == 0 and period % group == 0
    for offset in reversed(range(0, min(period, n), group)):
        for first in range(offset, n, period):
            yield tuple(range(first, first + group))


def line_reference(n, period):
    """Independent expression of today's descending-offset group-1 order."""
    return sorted(range(n), key=lambda line: (-(line % period), line // period))


def check_coverage():
    checked = 0
    for n in (64, 128, 192, 256, 320, 512, 1024):
        for group in (1, 2, 4, 8, 16):
            periods = {
                group, 2 * group, 3 * group, 7 * group, 16 * group,
                n - group, n, n + group, 2 * n + group,
                ((511 + group - 1) // group) * group,
                ((513 + group - 1) // group) * group,
            }
            for period in sorted(periods):
                groups = list(visits(n, period, group))
                order = list(chain.from_iterable(groups))
                # This checks uniqueness, omissions, and bounds independently of
                # the decomposition used by the walk.
                assert sorted(order) == list(range(n)), (n, period, group)
                advances = sum(b[0] - a[0] == period for a, b in zip(groups, groups[1:]))
                assert advances == (n - min(period, n)) // group
                if group == 1:
                    assert order == line_reference(n, period)
                checked += 1
    return checked


SELECTORS = {
    "two_channels_alternating_lines": lambda line: line & 1,
    "two_channels_two_line_regions": lambda line: (line >> 1) & 1,
    "two_channels_four_line_regions": lambda line: (line >> 2) & 1,
    "two_channels_eight_line_regions": lambda line: (line >> 3) & 1,
    "two_channels_low_high_xor": lambda line: (line ^ (line >> 9)) & 1,
    "four_channels_alternating_lines": lambda line: line & 3,
}


def score(n, period, group, base, selector):
    groups = [[selector(base + line) for line in visit] for visit in visits(n, period, group)]
    destinations = list(chain.from_iterable(groups))
    counts = Counter(destinations)
    # Every geometry must have identical destination totals on this allocation.
    assert counts == Counter(selector(base + line) for line in range(n))
    changes = sum(a != b for a, b in zip(destinations, destinations[1:]))
    return {
        "period_lines": period,
        "group_lines": group,
        "base_physical_line": base,
        "mean_destinations_per_visit": sum(len(set(visit)) for visit in groups) / len(groups),
        "consecutive_destination_changes": changes,
        "consecutive_pairs": n - 1,
        "destination_totals": dict(sorted(counts.items())),
    }


def main():
    n = 4096
    geometries = [(512, group) for group in (1, 2, 4, 8, 16)] + [(513, 1)]
    results = {
        "assumptions": [
            "Illustrative selectors, not a real controller model.",
            "Contiguous physical lines; bases aligned to 4 KiB.",
            "Counts include the entire sweep and offset-pass boundaries.",
            "No cache, prefetch, instruction reordering, timing, or DRAM scheduler model.",
            "No inference about error-detection effectiveness.",
        ],
        "coverage_cases": check_coverage(),
        "chunk_lines": n,
        "models": {
            name: [
                score(n, period, group, base, selector)
                for base in (0, 64, 448, 512, 4032)
                for period, group in geometries
            ]
            for name, selector in SELECTORS.items()
        },
    }
    print(f"Coverage: {results['coverage_cases']} cases passed; all model histograms match.")
    print("Mean destinations per visit, P=512, base=0:")
    print("model                                           G1   G2   G4   G8  G16")
    for name, samples in results["models"].items():
        values = [sample["mean_destinations_per_visit"] for sample in samples[:5]]
        print(f"{name:46}" + "".join(f"{value:5.1f}" for value in values))


if __name__ == "__main__":
    main()
