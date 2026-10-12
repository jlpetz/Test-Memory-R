"""Offline transfer-profile research, not a production DDR6 implementation.

The 12-DQ example uses the overhead *positions* described in public Samsung
patent US20250363001A1, Fig. 4 / paragraphs describing that figure. Its remaining
payload positions are numbered in SYNTHETIC beat-major order. This is explicitly
NOT a verified JESD209-6 / CPU-controller payload-bit map.

The 24-DQ example duplicates that abstract lane layout side by side. It tests
arbitrary-width support; it is NOT a proposed normative desktop DDR6 map.

Outputs compare models, not measured pin activity. Unknown metadata is left
unknown; bounds are exact for independently free unknown bits on each pin.
"""

from dataclasses import dataclass
from pathlib import Path
import hashlib
import itertools
import json
import random
import statistics


HERE = Path(__file__).resolve().parent
MASK64 = (1 << 64) - 1
G = 0x9E3779B97F4A7C15


def fmix(x):
    x &= MASK64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK64
    return x ^ (x >> 31)


def unxor(x, shift):
    out = x
    for n in range(shift, 64, shift):
        out ^= x >> n
    return out


def inv_fmix(x):
    x = unxor(x, 31)
    x = x * pow(0x94D049BB133111EB, -1, 1 << 64) & MASK64
    x = unxor(x, 27)
    x = x * pow(0xBF58476D1CE4E5B9, -1, 1 << 64) & MASK64
    return unxor(x, 30)


def mix(a, b):
    return fmix(a ^ fmix((b + G) & MASK64))


def unmix_a(output, b):
    return inv_fmix(output) ^ fmix((b + G) & MASK64)


@dataclass(frozen=True)
class TransferProfile:
    name: str
    dq: int
    beats: int
    # At each transfer/pin: payload bit number, or None for uncontrollable data.
    slots: tuple
    status: str

    @property
    def payload_bits(self):
        return sum(i is not None for i in self.slots)

    @property
    def edges(self):
        return self.dq * (self.beats - 1)

    def validate(self):
        assert self.dq > 0 and self.beats > 1
        assert len(self.slots) == self.dq * self.beats
        bits = sorted(i for i in self.slots if i is not None)
        assert bits == list(range(self.payload_bits)), "payload map must be bijective"
        assert self.payload_bits % 64 == 0, "ordinary u64 host buffer in these examples"

    def pack(self, payload):
        return [None if bit is None else (payload >> bit) & 1 for bit in self.slots]

    def unmap(self, wire):
        """Extract the software-controllable payload; never manufacture metadata."""
        payload = 0
        for bit, value in zip(self.slots, wire):
            if bit is not None:
                payload |= value << bit
        return payload


def contiguous(dq, beats):
    return TransferProfile(
        f"plain-{dq}x{beats}", dq, beats, tuple(range(dq * beats)),
        "unencoded address-order mathematical model",
    )


def metadata_example(dq):
    assert dq in (12, 24)
    metadata_lanes = {4, 5, 10, 11}
    slots = []
    n = 0
    for t in range(24):
        for lane in range(dq):
            overhead = lane % 12 in metadata_lanes and (8 <= t < 12 or 20 <= t < 24)
            if overhead:
                slots.append(None)
            else:
                slots.append(n)
                n += 1
    return TransferProfile(
        f"illustrative-{dq}dq-bl24", dq, 24, tuple(slots),
        "experimental: public-patent overhead locations, SYNTHETIC payload numbering"
        if dq == 12 else
        "experimental 24-DQ extension; NOT a JEDEC DDR6 profile",
    )


def profile_switch(profile, lane_seed, polarity=0):
    """Choose an alternating target on each pin, then invert the payload map."""
    target = [
        ((lane_seed >> (i % profile.dq)) & 1) ^ ((i // profile.dq) & 1) ^ polarity
        for i in range(len(profile.slots))
    ]
    return profile.unmap(target)


def compile_nibble_tables(profile):
    """A general packing implementation using small precomputed XOR tables.

    Runtime hot work: ceil(DQ/4) table contributions XORed with a constant.
    This is a cost model / reference, not a measured SIMD performance claim.
    """
    base = profile_switch(profile, 0)
    tables = []
    for group in range((profile.dq + 3) // 4):
        tables.append(tuple(
            profile_switch(profile, digit << (4 * group)) ^ base
            for digit in range(16)
        ))
    return base, tuple(tables)


def table_switch(base, tables, lane_seed):
    output = base
    for group, table in enumerate(tables):
        output ^= table[(lane_seed >> (4 * group)) & 15]
    return output


def transition_bounds(profile, wire):
    """Min/max flips with the unknown slots arbitrary, respecting shared endpoints.

    A short two-state dynamic program per pin. Merely counting each unknown edge
    independently is weaker: endpoints across a metadata gap constrain parity.
    """
    minimum = maximum = 0
    known_edges = known_flips = 0
    for lane in range(profile.dq):
        sequence = wire[lane::profile.dq]
        lo = {v: 0 for v in (0, 1) if sequence[0] is None or v == sequence[0]}
        hi = dict(lo)
        for prev, current in zip(sequence, sequence[1:]):
            if prev is not None and current is not None:
                known_edges += 1
                known_flips += prev ^ current
            choices = (0, 1) if current is None else (current,)
            lo = {v: min(score + (old ^ v) for old, score in lo.items()) for v in choices}
            hi = {v: max(score + (old ^ v) for old, score in hi.items()) for v in choices}
        minimum += min(lo.values())
        maximum += max(hi.values())
    return {
        "data_to_data_edges": known_edges,
        "data_to_data_flips": known_flips,
        "all_dq_edges": profile.edges,
        "min_flips": minimum,
        "max_flips": maximum,
        "min_fraction": minimum / profile.edges,
        "max_fraction": maximum / profile.edges,
        "iid_unknown_expected_fraction": (
            known_flips + (profile.edges - known_edges) / 2
        ) / profile.edges,
    }


def native_switch_payload(word_count, width, h):
    # The production encoder's image family, with a supplied 32-bit hash.
    a = 0
    for b in range(8):
        n = (h >> (4 * b)) & 15
        a |= (n | ((n ^ 15) << 4)) << (8 * b)
    out = 0
    for i in range(word_count * 64):
        value = ((a >> (i % width)) & 1) ^ ((i // width) & 1)
        out |= value << i
    return out


def raw_width_switch(payload_bits, width, seed):
    # The tempting but incorrect abstraction when holes/encoding intervene.
    return sum(
        (((seed >> (i % width)) & 1) ^ ((i // width) & 1)) << i
        for i in range(payload_bits)
    )


def score_candidates(profile):
    rng = random.Random(20261011)
    cases = {
        "native-switch16": [], "native-switch32": [], "native-switch64": [],
        f"raw-width{profile.dq}": [], "profile-mapped": [], "random-payload": [],
    }
    for _ in range(1024):
        seed = rng.getrandbits(64)
        payloads = {
            "native-switch16": native_switch_payload(profile.payload_bits // 64, 16, seed),
            "native-switch32": native_switch_payload(profile.payload_bits // 64, 32, seed),
            "native-switch64": native_switch_payload(profile.payload_bits // 64, 64, seed),
            f"raw-width{profile.dq}": raw_width_switch(profile.payload_bits, profile.dq, seed),
            "profile-mapped": profile_switch(profile, seed),
            "random-payload": rng.getrandbits(profile.payload_bits),
        }
        for name, payload in payloads.items():
            s = transition_bounds(profile, profile.pack(payload))
            cases[name].append(s["data_to_data_flips"] / s["data_to_data_edges"])
    return {
        name: {"mean": statistics.mean(x), "min": min(x), "max": max(x)}
        for name, x in cases.items()
    }


def validate_tables(profile):
    base, tables = compile_nibble_tables(profile)
    seeds = list(range(1 << min(profile.dq, 12)))
    seeds += [fmix(i) for i in range(1024)]
    for seed in seeds:
        payload = profile_switch(profile, seed)
        assert table_switch(base, tables, seed) == payload
        assert payload ^ profile_switch(profile, seed, 1) == (1 << profile.payload_bits) - 1
        score = transition_bounds(profile, profile.pack(payload))
        assert score["data_to_data_flips"] == score["data_to_data_edges"]
    # Repeat with a shuffled payload-bit numbering. Same physical target, a
    # different CPU packing, no changes to the generator or verifier algorithm.
    permutation = list(range(profile.payload_bits))
    random.Random(17).shuffle(permutation)
    swapped = TransferProfile(
        profile.name + "-payload-permuted", profile.dq, profile.beats,
        tuple(None if i is None else permutation[i] for i in profile.slots),
        "synthetic permutation of the same physical slot layout",
    )
    swapped.validate()
    for seed in seeds[:64]:
        assert profile.pack(profile_switch(profile, seed)) == swapped.pack(profile_switch(swapped, seed))
    # A known, fixed per-pin XOR mask preserves every same-pin transition.
    sample = profile.pack(profile_switch(profile, 0xace))
    toggled = [
        None if value is None else value ^ ((0x693 >> (i % profile.dq)) & 1)
        for i, value in enumerate(sample)
    ]
    assert transition_bounds(profile, sample) == transition_bounds(profile, toggled)
    return {
        "tested_seeds": len(seeds),
        "table_groups": len(tables),
        "table_bytes": len(tables) * 16 * (profile.payload_bits // 8),
        "base_bytes": profile.payload_bits // 8,
        "table_sha256": hashlib.sha256(
            b"".join(n.to_bytes(profile.payload_bits // 8, "little") for t in tables for n in t)
        ).hexdigest(),
    }


def dbi_counterexample():
    # An illustrative two-mask, byte-XOR encoder of the type described in the
    # public pre-standard Synopsys presentation. No assertion about a real host's
    # mask-selection algorithm. Both inputs have exactly four ones.
    before = [0xA5, 0x5A]
    masks = [0xA5, 0x5A]
    after = [b ^ mask for b, mask in zip(before, masks)]
    assert all(b.bit_count() == 4 for b in before)
    return {
        "status": "possible programmable-XOR transform, NOT a measured LPDDR6 controller",
        "input_bytes": before, "selected_masks": masks, "wire_bytes": after,
        "input_flips": (before[0] ^ before[1]).bit_count(),
        "output_flips": (after[0] ^ after[1]).bit_count(),
    }


def validate_transition_bounds():
    """Independently compare the DP with every completion of short ternary traces."""
    profile = TransferProfile("bounds-self-check", 1, 6, tuple(range(6)), "toy")
    traces = completions = 0
    for sequence in itertools.product((0, 1, None), repeat=6):
        scores = []
        for replacement in itertools.product((0, 1), repeat=sequence.count(None)):
            remaining = iter(replacement)
            bits = [next(remaining) if bit is None else bit for bit in sequence]
            scores.append(sum(a ^ b for a, b in zip(bits, bits[1:])))
            completions += 1
        bounds = transition_bounds(profile, sequence)
        assert bounds["min_flips"] == min(scores)
        assert bounds["max_flips"] == max(scores)
        traces += 1
    return {"traces": traces, "completed_binary_traces": completions}


def linepair_seed_witness():
    key = 0x405AC1F500000000
    step_key = unmix_a(unmix_a(key, 0x5E), 0)
    seed = unmix_a(unmix_a(unmix_a(step_key, 0x5EED), 0), 1)
    assert mix(mix(mix(mix(mix(seed, 1), 0), 0x5EED), 0), 0x5E) == key
    return {"seed": f"0x{seed:016x}", "suite_cycle": 1, "step": 0, "pair": 0,
            "family": "linepair", "key": f"0x{key:016x}",
            "byte_offsets": ["0x0", "0x800000040"],
            "equal_word": "0x405ac1f5405ac1f5"}


def main():
    profiles = [
        contiguous(16, 32), contiguous(32, 16), contiguous(64, 8),
        metadata_example(12), metadata_example(24),
    ]
    output = {
        "scope": "mathematical transfer models; no physical measurement or normative DDR6 implementation",
        "overhead_location_source": "https://patents.google.com/patent/US20250363001A1/en",
        "profiles": [],
        "metadata_bounds_validation": validate_transition_bounds(),
        "programmable_xor_counterexample": dbi_counterexample(),
        "linepair_collision_replay": linepair_seed_witness(),
    }
    for profile in profiles:
        profile.validate()
        record = {
            "name": profile.name, "status": profile.status,
            "dq": profile.dq, "beats": profile.beats,
            "payload_bytes": profile.payload_bits // 8,
            "unknown_slots": sum(i is None for i in profile.slots),
            "profile_switch": transition_bounds(
                profile, profile.pack(profile_switch(profile, 0x69A5C3))
            ),
            "packing_validation": validate_tables(profile),
            "candidate_data_edge_fractions": score_candidates(profile),
        }
        output["profiles"].append(record)
        print(profile.name, profile.status, flush=True)
        print("  target:", record["profile_switch"])
        print("  packing:", record["packing_validation"])
        print("  candidate means:", {k: round(v["mean"], 6) for k, v in record["candidate_data_edge_fractions"].items()})
    (HERE / "transfer-profile-results.json").write_text(json.dumps(output, indent=2) + "\n", encoding="utf-8")
    print("Linepair witness:", output["linepair_collision_replay"])
    print("Programmable-XOR example:", output["programmable_xor_counterexample"])


if __name__ == "__main__":
    main()
