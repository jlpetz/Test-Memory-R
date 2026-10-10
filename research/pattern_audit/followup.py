"""Bounded logical-data study: signal histories and controller transformations.

No TMR execution, live memory faults, or physical signal-integrity simulation.
All histories and transitions stay INSIDE a synthetic 64-byte payload burst.
No scheduling, idle/preamble, ECC pins, physical adjacency, or real scrambler
algorithm is inferred. Reuses the independently checked audit pattern model.

Usage: python research/pattern_audit/followup.py
"""

from collections import Counter
from itertools import islice
from pathlib import Path
import json
import math
import statistics

import simulate as model


LINES = 1024
HISTORY = 5
OUTPUT = Path(__file__).resolve().parent / "followup-results.json"


def to_beats(words, width):
    image = sum(word << (64 * index) for index, word in enumerate(words))
    mask = (1 << width) - 1
    return [(image >> offset) & mask for offset in range(0, 512, width)]


def metrics(bursts, width):
    histories = [Counter() for _ in range(width)]
    history_mask = (1 << HISTORY) - 1
    changed = quiet_pairs = edges = lines = 0
    for beats in bursts:
        assert len(beats) == 512 // width
        lines += 1
        edges += len(beats) - 1
        for before, after in zip(beats, beats[1:]):
            toggles = (before ^ after).bit_count()
            changed += toggles
            # Ordered pair: one quiet logical bit and a different toggling bit.
            quiet_pairs += toggles * (width - toggles)
        for pin, counter in enumerate(histories):
            history = 0
            for beat_index, beat in enumerate(beats):
                history = ((history << 1) | ((beat >> pin) & 1)) & history_mask
                if beat_index + 1 >= HISTORY:
                    counter[history] += 1
    assert lines == LINES
    entropies = []
    for counter in histories:
        count = sum(counter.values())
        entropies.append(-sum((n / count) * math.log2(n / count) for n in counter.values()))
    coverage = [len(counter) for counter in histories]
    return {
        "width_bits": width,
        "lines": lines,
        "within_burst_edges": edges,
        "flip_share": changed / (edges * width),
        "quiet_victim_toggling_aggressor_pair_share": quiet_pairs / (edges * width * (width - 1)),
        "history_bits": HISTORY,
        "possible_histories_per_logical_bit": 1 << HISTORY,
        "histories_seen_per_logical_bit": coverage,
        "history_coverage_min_median_max": [min(coverage), statistics.median(coverage), max(coverage)],
        "history_entropy_bits_min_mean_max": [min(entropies), statistics.mean(entropies), max(entropies)],
        "history_counts_per_logical_bit": [dict(sorted(counter.items())) for counter in histories],
    }


def candidate_words(width):
    addresses = [model.BASE + 64 * line for line in range(LINES)]
    candidates = {
        "Mode 14": list(islice(model.current_lines(14, beat=width), LINES)),
        "Mode 13 scalar reference": list(islice(model.current_lines(13), LINES)),
        "Cheap full-address identity": [
            [model.cheap_word(address + 8 * word) for word in range(8)]
            for address in addresses
        ],
        "Current seal": [
            [model.seal_word(address + 8 * word) for word in range(8)]
            for address in addresses
        ],
        "Balanced triple": [model.triple_line(address) for address in addresses],
    }
    for name, generator in (
        ("Balanced near-flip", model.near_flip_line_from_seed),
        ("Balanced header and flip tail", model.header_flip_from_seed),
    ):
        candidates[name] = [
            generator(model.splitmix((address >> 6) + model.KEY), width)
            for address in addresses
        ]
    return candidates


def dbi(beat, width):
    return int.from_bytes(beat.to_bytes(width // 8, "little").translate(model.DBI), "little")


def rotate_each_byte(beat, width):
    result = 0
    for offset in range(0, width, 8):
        byte = (beat >> offset) & 255
        result |= (((byte << 3) | (byte >> 5)) & 255) << offset
    return result


def transformed_bursts(bursts, width, kind):
    mask = (1 << width) - 1
    for line, beats in enumerate(bursts):
        repeated = model.splitmix(line + 0x6A09E667F3BCC909) & mask
        transformed = []
        for index, beat in enumerate(beats):
            if kind == "No transformation":
                value = beat
            elif kind == "Fixed within-byte pin permutation":
                value = rotate_each_byte(beat, width)
            elif kind == "Same XOR mask for every beat of a burst":
                value = beat ^ repeated
            elif kind == "Alternating zero/all-one XOR masks":
                value = beat ^ (mask if index & 1 else 0)
            elif kind == "Independent-looking XOR masks per beat":
                value = beat ^ (model.splitmix(line * len(beats) + index + model.KEY) & mask)
            elif kind == "DDR4 DC-byte DBI only":
                value = dbi(beat, width)
            elif kind == "Same XOR mask then DDR4 DC-byte DBI":
                value = dbi(beat ^ repeated, width)
            else:
                raise ValueError(kind)
            transformed.append(value)
        yield transformed


def validate_algebra():
    # Exhaust every pair of four-bit differences. OR cannot cancel a mismatch;
    # XOR by itself could, which is why these must not be confused.
    for first in range(16):
        for second in range(16):
            assert ((first | second) == 0) == (first == 0 and second == 0)
    assert 7 ^ 7 == 0 and 7 | 7 != 0

    # Complementary input bytes remain complementary under a common XOR mask.
    # DBI then collapses the pair unless the transformed byte is balanced.
    a = 0xA5
    survivors = 0
    for xor_mask in range(256):
        before = model.DBI[a ^ xor_mask]
        after = model.DBI[(a ^ 255) ^ xor_mask]
        delta = (before ^ after).bit_count()
        assert delta in (0, 8)
        survivors += delta == 8
    assert survivors == math.comb(8, 4) == 70

    # A held victim plus a switching aggressor is absent at maximum switching.
    for width in (16, 32, 64):
        assert width * (width - width) == 0
        assert (width - 2) * 2 > 0

    # Reduced-size exhaustive check of the circle-partition proof in the report:
    # h observations of one bit of an arithmetic progression have at most 2h
    # histories. Each observation introduces only two boundaries on the circle.
    maximum_histories = 0
    for step in range(1, 256, 2):
        histories = {
            tuple(((phase + index * step) & 255) >> 7 for index in range(HISTORY))
            for phase in range(256)
        }
        maximum_histories = max(maximum_histories, len(histories))
        assert len(histories) <= 2 * HISTORY
    assert maximum_histories == 10
    return {
        "or_reduction_exhaustive_difference_pairs": 256,
        "arithmetic_progression_history_check": {
            "modulus": 256, "odd_steps": 128, "starting_phases_per_step": 256,
            "history_bits": HISTORY, "maximum_distinct_histories": maximum_histories,
        },
        "complement_byte_common_xor_then_dbi": {
            "xor_masks_exhausted": 256,
            "masks_preserving_full_toggle": survivors,
            "mean_toggle_share": survivors / 256,
        },
    }


def main():
    algebra = validate_algebra()
    scores = []
    for width in (32, 64):
        for name, words in candidate_words(width).items():
            result = metrics((to_beats(line, width) for line in words), width)
            result["pattern"] = name
            scores.append(result)
            if name == "Mode 14":
                assert result["flip_share"] == 1
                assert result["history_coverage_min_median_max"] == [2, 2, 2]
                assert result["quiet_victim_toggling_aggressor_pair_share"] == 0
            if name == "Cheap full-address identity" and width == 64:
                assert max(result["histories_seen_per_logical_bit"][32:]) <= 2 * HISTORY
            print(
                f"{width:2} {name:31} flips {result['flip_share']:6.2%}  "
                f"quiet/toggle {result['quiet_victim_toggling_aggressor_pair_share']:6.2%}  "
                f"5-bit histories {result['history_coverage_min_median_max']}  "
                f"mean entropy {result['history_entropy_bits_min_mean_max'][1]:.3f}/5",
                flush=True,
            )

    transformations = []
    width = 32
    bursts = [to_beats(words, width) for words in islice(model.current_lines(14, beat=width), LINES)]
    for kind in (
        "No transformation",
        "Fixed within-byte pin permutation",
        "Same XOR mask for every beat of a burst",
        "Alternating zero/all-one XOR masks",
        "Independent-looking XOR masks per beat",
        "DDR4 DC-byte DBI only",
        "Same XOR mask then DDR4 DC-byte DBI",
    ):
        result = metrics(transformed_bursts(bursts, width, kind), width)
        result["transformation"] = kind
        transformations.append(result)
        if kind in ("No transformation", "Fixed within-byte pin permutation",
                    "Same XOR mask for every beat of a burst", "DDR4 DC-byte DBI only"):
            assert result["flip_share"] == 1
        if kind == "Alternating zero/all-one XOR masks":
            assert result["flip_share"] == 0
        print(f"Transform: {kind:48} flips {result['flip_share']:.4%}", flush=True)
    OUTPUT.write_text(json.dumps({
        "assumptions": {
            "synthetic_lines_per_pattern": LINES,
            "line_bytes": 64,
            "base": hex(model.BASE),
            "within_burst_only": True,
            "controller_masks": "Illustrative algebraic models, NOT Intel/AMD implementations",
            "dbi": "DDR4 DC-byte rule only; its use on 32-bit model data is not a DDR5 claim",
            "physical_signals_simulated": False,
            "production_simd_substitutions_mode13": "Not modeled; scalar mathematical reference only",
        },
        "algebra": algebra,
        "pattern_scores": scores,
        "transformation_scores": transformations,
    }, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote {OUTPUT}")


if __name__ == "__main__":
    main()
