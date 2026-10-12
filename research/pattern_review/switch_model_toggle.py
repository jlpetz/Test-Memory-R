"""The 12- and 24-bit switch profiles (doc/pattern_spec.md) on transfer_profiles.py's layouts.

Added 2026-10-11 to back the spec's figures; a model comparison, not hardware. The rule: `s` is
the low W bits of the balanced spread `a` of a random 32-bit line hash, and place q of a burst
holds bit (q mod W) of s, inverted when q / W is odd. Scored as transfer_profiles.py scores:
the share of same-pin transitions between two payload bits that toggle.

- payload first: the payload fills the burst's first 256 (12) or 512 (24) slots, in transfer
  order, and the overhead comes after (what the rule assumes);
- illustrative: transfer_profiles.metadata_example, the patent's overhead positions for 12 pins
  with synthetic payload numbering, and its side-by-side copy for 24 (not a JEDEC DDR6 map).

Also today's 16-, 32- and 64-bit profiles on both. Run: python -I switch_model_toggle.py
"""

import random
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from transfer_profiles import TransferProfile, metadata_example, transition_bounds  # noqa: E402


def balanced_a(h):
    """Byte b of a is nibble b of h, then its inverse in the high nibble."""
    a = 0
    for b in range(8):
        n = (h >> (4 * b)) & 15
        a |= (n | ((n ^ 15) << 4)) << (8 * b)
    return a


def switch_payload(payload_bits, width, h):
    s = balanced_a(h) & ((1 << width) - 1)
    out = 0
    for q in range(payload_bits):
        out |= (((s >> (q % width)) & 1) ^ ((q // width) & 1)) << q
    return out


def payload_first(width):
    burst = 256 if width == 12 else 512
    slots = tuple(i if i < burst else None for i in range(width * 24))
    return TransferProfile(f"payload-first-{width}x24", width, 24, slots, "the rule's own assumption")


def score(profile, generator_width, rng, cases):
    fractions = []
    for _ in range(cases):
        payload = switch_payload(profile.payload_bits, generator_width, rng.getrandbits(32))
        s = transition_bounds(profile, profile.pack(payload))
        fractions.append(s["data_to_data_flips"] / s["data_to_data_edges"])
    return statistics.mean(fractions), min(fractions), max(fractions)


def main():
    rng = random.Random(20261011)
    for width in (12, 24):
        for profile in (payload_first(width), metadata_example(width)):
            mean, lo, hi = score(profile, width, rng, 1024)
            print(f"{width}-bit rule on {profile.name}: mean {mean:.3f}, lines {lo:.3f}-{hi:.3f}")
            others = ", ".join(f"{w}: {score(profile, w, rng, 512)[0]:.3f}" for w in (16, 32, 64))
            print(f"  today's profiles on it: {others}")


if __name__ == "__main__":
    main()
