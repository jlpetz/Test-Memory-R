"""Independent payload-model check of repetition, not physical DDR measurements."""

from collections import Counter
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
MASK64 = (1 << 64) - 1


def mix(x):
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK64
    return x ^ (x >> 31)


def seed_from_hash(h, width):
    seed = 0
    for byte in range((width + 7) // 8):
        nibble = (h >> (4 * byte)) & 15
        seed |= (nibble | ((nibble ^ 15) << 4)) << (8 * byte)
    return seed & ((1 << width) - 1)


def line(seed, width, payload):
    image = 0
    for bit in range(512):
        place = bit % payload
        value = ((seed >> (place % width)) & 1) ^ ((place // width) & 1)
        image |= value << bit
    return image


def switching(image, width, payload):
    edges = payload - width
    mask = (1 << edges) - 1
    return sum(
        (((image >> start) ^ (image >> (start + width))) & mask).bit_count()
        for start in range(0, 512, payload)
    ), edges * (512 // payload)


def main():
    half_mask = (1 << 256) - 1
    rows = []
    for width, payload, hash_bits in [(12, 256, 8), (16, 256, 8), (24, 512, 12),
                                     (32, 512, 16), (64, 512, 32)]:
        exhaustive = hash_bits <= 12
        hashes = range(1 << hash_bits) if exhaustive else map(mix, range(4096))
        counts = Counter()
        equal_halves = 0
        tested = 0
        for h in hashes:
            image = line(seed_from_hash(h, width), width, payload)
            words = [(image >> (i * 64)) & MASK64 for i in range(8)]
            counts[len(set(words))] += 1
            equal_halves += (image & half_mask) == (image >> 256)
            flips, edges = switching(image, width, payload)
            assert flips == edges
            tested += 1
        rows.append({
            "width": width, "payload_bits": payload,
            "hash_bits_reaching_line": hash_bits,
            "seeds_checked": tested, "exhaustive": exhaustive,
            "equal_32_byte_halves": equal_halves,
            "distinct_word_counts": dict(sorted(counts.items())),
            "known_payload_edge_switching": 1.0,
        })

    # Reusing the complement as the second 32-byte burst preserves switching
    # inside each burst for the current 12/16 profiles and distinguishes halves.
    # For 24/32/64, that boundary lies inside the modeled 64-byte payload: it
    # breaks one formerly toggling transition on each pin.
    alternate = []
    for width, payload in [(12, 256), (16, 256), (24, 512), (32, 512), (64, 512)]:
        image = line(seed_from_hash(0xA7D391, width), width, payload)
        changed = image ^ (half_mask << 256)
        flips, edges = switching(changed, width, payload)
        alternate.append({
            "width": width, "payload_bits": payload,
            "invert_second_half_flips": flips, "known_payload_edges": edges,
            "rate": flips / edges,
        })
    result = {
        "scope": "Contiguous payload models, only adjacent payload-to-payload edges; no encoding or overhead.",
        "current": rows,
        "experimental_second_half_complement": alternate,
    }
    (HERE / "repetition-results.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
