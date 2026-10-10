"""Independent, deterministic logical-data simulations for the TMR audit.

These are NOT measurements of the physical DRAM bus. Widths are logical bit lags
inside a 64-byte line; DDR4/DDR5 labels require conventional unscrambled mappings.
DBI is the DDR4-style per-byte DC rule, not a claim that DDR5 applies that rule.

Usage: python research/pattern_audit/simulate.py
No third-party dependencies. The largest pattern sample is 1 MiB.
"""

from collections import Counter
from fractions import Fraction
from pathlib import Path
import json
import math
import random

M64 = (1 << 64) - 1
M63 = (1 << 63) - 1
G = 0x9E3779B97F4A7C15
BASE = 0x1E980000000
LINES = 16384
KEY = 0x243F6A8885A308D3
P0, P1 = 0x5DEECE66D, 0xB
WIDTHS = (16, 32, 64)
DBI = bytes(v ^ (0xFF if v.bit_count() < 4 else 0) for v in range(256))
AUDIT = Path(__file__).resolve().parent


def mix(x):
    return x ^ (x >> 32)


def splitmix(x):
    x &= M64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & M64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & M64
    return x ^ (x >> 31)


def block_seed(addr, thread, cycle):
    h = ((addr * G) & M64) ^ ((thread * 0x517CC1B727220A95) & M64)
    h ^= (cycle * 0x6C62272E07BB0142) & M64
    h = ((h ^ (h >> 33)) * 0xFF51AFD7ED558CCD) & M64
    return h ^ (h >> 33)


def key12(thread=0, cycle=1, p0=P0, p1=P1):
    return block_seed(0, thread, cycle) ^ ((p0 * 0xD6E8FEB86659FD93) & M64) ^ (
        ((p1 << 32) | (p1 >> 32)) & M64
    )


def balanced(h, byte_count=8):
    """Current mode 14 encoding: four independent bits per byte."""
    return sum(
        (((h >> (4 * i)) & 15) | ((~(h >> (4 * i)) & 15) << 4)) << (8 * i)
        for i in range(byte_count)
    )


def seal_word(addr, tagged=False, key=0, invert=False):
    x = ((addr >> 4) * G + key) & (M63 if tagged else M64)
    return mix(x) ^ (M64 if bool(addr & 8) ^ invert else 0)


def cheap_word(addr, key=KEY):
    return mix((((addr >> 3) * G) + key) & M64)


def tm5_motif(page):
    """RS_GeneratePattern with the 64-bit bus option, checked against its asm."""
    d = ((page >> 12) + (page >> 28)) & 0xFFFF
    rotations = [((d << j) | (d >> (16 - j))) & 0xFFFF for j in range(12)]
    words = []
    for group in range(3):
        q = sum(rotations[4 * group + j] << (16 * j) for j in range(4))
        words.extend([q, q ^ M64])
    return words


def tm5_seal_lines():
    for line in range(LINES):
        address = BASE + 64 * line
        motif = tm5_motif(address & ~4095)
        yield [motif[((address & 4095) // 8 + j) % 6] for j in range(8)]


def tm5_simple0_lines():
    motif = tm5_motif(BASE)
    # ST_GeneratePattern swaps the value and complement on an even 4 KiB page.
    if BASE & 0x1000 == 0:
        motif = [word ^ M64 for word in motif]
    words = motif + motif[:2]
    for _ in range(LINES):
        yield words


def current_lines(mode, p0=P0, p1=P1, beat=32):
    seed0 = block_seed(BASE, 0, 1)
    key = key12(p0=p0, p1=p1)
    seed = step = 0
    for line in range(LINES):
        address = BASE + 64 * line
        indices = range(8 * line, 8 * line + 8)
        if mode == 0:
            yield [(seed0 ^ ((i * G) & M64)) ^ (M64 if (i >> 9) & 1 else 0) for i in indices]
        elif mode == 1:
            yield [((seed0 - 3 * i) & M64) ^ (M64 if (i >> 3) & 1 else 0) for i in indices]
        elif mode == 2:
            if line % 64 == 0:
                seed = block_seed(address, 0, 1)
                step = (seed * P0) & M64
            yield [(seed + j * step) & M64 for j in range(8)]
            seed = (seed * p0 + p1) & M64
            step = (step * seed) & M64
        elif mode in (10, 11):
            base = p0 if mode == 10 else p0 ^ p1
            yield [i ^ base for i in indices]
        elif mode == 12:
            seed = splitmix(address + key)
            x = seed ^ key
            step = (((((x << 29) | (x >> 35)) & M64) * G) & M64) | 1
            yield [(seed + j * step) & M64 for j in range(8)]
        elif mode == 13:
            yield [splitmix(i + p0) for i in indices]
        elif mode == 14:
            a = balanced(splitmix(address + key))
            if beat == 64:
                yield [a if j % 2 == 0 else a ^ M64 for j in range(8)]
            elif beat == 32:
                yield [(a & 0xFFFFFFFF) | ((~a & 0xFFFFFFFF) << 32)] * 8
            else:
                pair = (a & 0xFFFF) | ((~a & 0xFFFF) << 16)
                yield [pair | (pair << 32)] * 8
        else:
            raise ValueError(mode)


def from_blocks(blocks, bits):
    image = sum((blocks[i % len(blocks)] & ((1 << bits) - 1)) << (bits * i)
                for i in range((512 + bits - 1) // bits)) & ((1 << 512) - 1)
    return [(image >> (64 * i)) & M64 for i in range(8)]


def triple_line(addr, invert_alternate=False, paired_lines=False):
    line = addr >> 6
    identity = line >> 1 if paired_lines else line
    h = splitmix(identity + KEY)
    a, c = balanced(h, 2), balanced(h >> 8, 2)
    words = from_blocks([a, a ^ 0xFFFF, c], 16)
    return [w ^ M64 for w in words] if invert_alternate and line & 1 else words


def near_flip_line_from_seed(seed, width):
    """DBI-safe near flip with a decodable 64-bit line identifier.

    Work in nibble payload space (k=W/2 bits). Complement each beat except
    one payload bit. Its byte expands that choice into two held physical bits,
    preserving weight four in every byte, and giving exactly W-2 toggles.
    Initial payload plus the one-hot choices losslessly carry all 64 seed bits.
    """
    k = width // 2
    shift = k.bit_length() - 1
    mask = (1 << k) - 1
    payload, control = seed & mask, seed >> k
    beats = []
    for _ in range(512 // width):
        beats.append(balanced(payload, width // 8))
        payload = (payload ^ mask) ^ (1 << (control & (k - 1)))
        control >>= shift
    return from_blocks(beats, width)


def decode_near_flip(words, width):
    image = as_int(words)
    k = width // 2
    shift = k.bit_length() - 1
    payloads = []
    for beat in range(512 // width):
        b = (image >> (beat * width)) & ((1 << width) - 1)
        payloads.append(sum(((b >> (8 * i)) & 15) << (4 * i) for i in range(width // 8)))
    seed = payloads[0]
    at = k
    for a, b in zip(payloads, payloads[1:]):
        delta = a ^ b ^ ((1 << k) - 1)
        assert delta != 0 and delta & (delta - 1) == 0
        seed |= (delta.bit_length() - 1) << at
        at += shift
    return seed & M64


def header_flip_from_seed(seed, width):
    """First 16 bytes losslessly encode the line seed, then alternate the last beat.

    This trades a variable number of header toggles for a much cheaper encoder.
    Every byte is balanced; header and tail are all still expected data.
    """
    payload_bits = width // 2
    header = [balanced(seed >> shift, width // 8) for shift in range(0, 64, payload_bits)]
    beats = header[:]
    while len(beats) < 512 // width:
        beats.append(beats[-1] ^ ((1 << width) - 1))
    return from_blocks(beats, width)


def decode_balanced_header(words):
    raw = as_int(words).to_bytes(64, "little")
    return sum((byte & 15) << (i * 4) for i, byte in enumerate(raw[:16]))


def as_int(words):
    return sum(w << (64 * i) for i, w in enumerate(words))


def changed(image, width):
    return ((image ^ (image >> width)) & ((1 << (512 - width)) - 1)).bit_count()


def score(name, lines):
    flips = Counter()
    post_dbi = Counter()
    distinct_words, distinct_lines = set(), set()
    minimum_word_variety = 8
    same_lines = 0
    equal_low32 = 0
    adjacent = 0
    paired_adjacent = 0
    previous = None
    count = 0
    balanced_count = 0
    for words in lines:
        line = as_int(words)
        raw = line.to_bytes(64, "little")
        encoded = int.from_bytes(raw.translate(DBI), "little")
        for w in WIDTHS:
            flips[w] += changed(line, w)
            post_dbi[w] += changed(encoded, w)
        distinct_words.update(words)
        distinct_lines.add(line)
        variety = len(set(words))
        minimum_word_variety = min(minimum_word_variety, variety)
        same_lines += variety == 1
        equal_low32 += len({v & 0xFFFFFFFF for v in words}) == 1
        balanced_count += sum(b.bit_count() == 4 for b in raw)
        if previous is not None:
            adjacent += (line ^ previous).bit_count()
            if count & 1:
                paired_adjacent += (line ^ previous).bit_count()
        previous = line
        count += 1
    return {
        "name": name,
        "lines": count,
        "flip_share": {str(w): flips[w] / (count * (512 - w)) for w in WIDTHS},
        "dbi_share": {str(w): post_dbi[w] / (count * (512 - w)) for w in WIDTHS},
        "distinct_u64": len(distinct_words),
        "distinct_lines": len(distinct_lines),
        "min_distinct_u64_in_line": minimum_word_variety,
        "single_word_lines": same_lines,
        "lines_with_identical_low32": equal_low32,
        "balanced_byte_share": balanced_count / (64 * count),
        "adjacent_line_hamming_share": adjacent / (512 * (count - 1)),
        "paired_line_hamming_share": paired_adjacent / (512 * (count // 2)),
    }


def finite_bound():
    """Find explicit 16-beat witnesses; 32 logical data pins, 64-byte burst."""
    witnesses = {}
    for sequence in range(1 << 16):
        s1 = ((sequence ^ (sequence >> 1)) & ((1 << 15) - 1)).bit_count()
        s2 = ((sequence ^ (sequence >> 2)) & ((1 << 14) - 1)).bit_count()
        if (s1, s2) in ((11, 8), (10, 10)):
            witnesses.setdefault((s1, s2), sequence)
    a, b = witnesses[(11, 8)], witnesses[(10, 10)]
    beats = []
    for t in range(16):
        nibble = ((a >> t) & 1) | (14 if (b >> t) & 1 else 0)
        byte = nibble | ((nibble ^ 15) << 4)
        beats.append(byte * 0x01010101)
    words = from_blocks(beats, 32)
    image = as_int(words)
    f32, f64 = Fraction(changed(image, 32), 480), Fraction(changed(image, 64), 448)
    assert f32 > Fraction(2, 3) and f64 > Fraction(2, 3)
    assert all(b.bit_count() == 4 for b in image.to_bytes(64, "little"))
    return {
        "binary_sequences": [hex(a), hex(b)],
        "line_words": [hex(w) for w in words],
        "p32": str(f32), "p64": str(f64),
        "p32_float": float(f32), "p64_float": float(f64),
        "finite_upper_bound_min_p32_p64": str(Fraction(30, 44)),
    }


def seal_collision():
    """Construct a same-value collision between opposite halves of distinct units.

    h(x)=x^(x>>32); h(x) xor all-ones = h(x xor 0xFFFFFFFF00000000).
    Choose x with high word 0x405AC1F5: complementing h(x) then
    inverting G lands exactly 2**32 units higher (64 GiB apart).
    Search only synthetic numbers, never map or access these addresses.
    """
    inverse = pow(G, -1, 1 << 64)
    high = 0x405AC1F5
    for low in range(1 << 25):
        x = (high << 32) | low
        unit = (x * inverse) & M64
        other = (((x ^ 0xFFFFFFFF00000000) * inverse) & M64)
        if 0x10000 <= unit * 16 < (1 << 47) and 0x10000 <= other * 16 + 8 < (1 << 47):
            first, second = unit * 16, other * 16 + 8
            assert first != second and seal_word(first) == seal_word(second)
            assert seal_word(first, tagged=True) != seal_word(second, tagged=True)
            return {"address_a": hex(first), "address_b": hex(second),
                    "delta_bytes": second - first, "value": hex(seal_word(first))}
    raise AssertionError("no synthetic user-address witness found")


def triple_counterexamples():
    sample = triple_line(BASE)
    assert changed(as_int(sample), 48) == 0
    extrema = {24: [1.0, 0.0], 12: [1.0, 0.0]}
    total = Counter()
    worst = {}
    for a_code in range(256):
        a = balanced(a_code, 2)
        for c_code in range(256):
            c = balanced(c_code, 2)
            image = as_int(from_blocks([a, a ^ 0xFFFF, c], 16))
            for width in extrema:
                p = changed(image, width) / (512 - width)
                total[width] += p
                if p < extrema[width][0]:
                    extrema[width][0] = p
                    worst[width] = [a, c]
                extrema[width][1] = max(extrema[width][1], p)
    return {"lag48_share": 0, "extrema": extrema,
            "mean_over_all_65536_payloads": {w: p / 65536 for w, p in total.items()},
            "worst_A_C": worst}


def validate_near_flip():
    rng = random.Random(20261009)
    samples = [0, M64] + [rng.getrandbits(64) for _ in range(4096)]
    for width in WIDTHS:
        for seed in samples:
            words = near_flip_line_from_seed(seed, width)
            image = as_int(words)
            assert changed(image, width) == (512 // width - 1) * (width - 2)
            assert image.to_bytes(64, "little").translate(DBI) == image.to_bytes(64, "little")
            assert decode_near_flip(words, width) == seed
            header_words = header_flip_from_seed(seed, width)
            header_image = as_int(header_words)
            assert header_image.to_bytes(64, "little").translate(DBI) == header_image.to_bytes(64, "little")
            assert decode_balanced_header(header_words) == seed
            assert changed(header_image, width) >= 384


def rng_coverage():
    rows = []
    for gib in (1, 8, 16, 32):
        words = gib * (1 << 30) // 8
        reads = min(max(words // 1000, 5000), 50000) // 8 * 8
        visited, lines = set(), set()
        for seq in range(8):
            state = (0x123456789ABCDEF + 1 + seq * 0x8765432187654321) & M64
            for _ in range(reads // 8):
                state ^= (state << 13) & M64
                state ^= state >> 17
                state ^= (state << 5) & M64
                index = (state * words) >> 64
                visited.add(index)
                lines.add(index // 8)
        rows.append({"gib": gib, "reads": reads, "distinct_words": len(visited),
                     "word_coverage_percent": 100 * len(visited) / words,
                     "line_coverage_percent": 100 * len(lines) / (words / 8)})
    return rows


def main():
    candidates = [(f"TMR {mode}", current_lines(mode)) for mode in (0, 1, 10, 11, 12, 13)]
    candidates.insert(2, ("TMR 2, community sequential params", current_lines(2, 0x77777777, 0x33333333)))
    candidates.insert(3, ("TMR 2, benchmark params", current_lines(2)))
    candidates += [
        ("TMR 10, SimpleTest default params", current_lines(10, 0xDEADBEEFDEADBEEF, 0xCAFEBABECAFEBABE)),
        ("TMR 11, SimpleTest default params", current_lines(11, 0xDEADBEEFDEADBEEF, 0xCAFEBABECAFEBABE)),
        ("TM5 seal, 64-bit bus reference", tm5_seal_lines()),
        ("TM5 mode 0, one fixed block, 64-bit bus reference", tm5_simple0_lines()),
    ]
    candidates += [
        ("TMR seal", ([seal_word(BASE + i * 64 + j * 8) for j in range(8)] for i in range(LINES))),
        ("Cheap full-address permutation", ([cheap_word(BASE + i * 64 + j * 8) for j in range(8)] for i in range(LINES))),
        ("Tagged 63-bit pair seal", ([seal_word(BASE + i * 64 + j * 8, tagged=True) for j in range(8)] for i in range(LINES))),
        ("Triple 16 balanced", (triple_line(BASE + i * 64) for i in range(LINES))),
        ("Triple independently hashed, odd lines inverted",
         (triple_line(BASE + i * 64, True, False) for i in range(LINES))),
        ("Triple pair-shared hash, odd lines inverted",
         (triple_line(BASE + i * 64, True, True) for i in range(LINES))),
    ]
    for width in WIDTHS:
        candidates.append((f"TMR 14/{width}", current_lines(14, beat=width)))
        for name, generator in [
            ("Balanced near-flip", near_flip_line_from_seed),
            ("Balanced identity header + flip tail", header_flip_from_seed),
        ]:
            # Consume now: generator expressions otherwise capture width/name late.
            lines = [generator(splitmix((BASE >> 6) + i + KEY), width) for i in range(LINES)]
            candidates.append((f"{name}/{width}", lines))
    results = []
    print("One MiB, logical lag scores; not a physical bus measurement.", flush=True)
    for name, lines in candidates:
        result = score(name, lines)
        results.append(result)
        p, dbi = result["flip_share"], result["dbi_share"]
        print(f"{name:48} DDR4 {float(p['64'])*64:6.2f}/64  DDR5 {float(p['32'])*32:6.2f}/32  "
              f"DBI64 {float(dbi['64']):.4f}  distinct {result['distinct_u64']:6} words/"
              f"{result['distinct_lines']:5} lines", flush=True)
    validate_near_flip()
    findings = {
        "assumptions": "64-byte logical lines, increasing-address data order, no scrambling, DC byte DBI only",
        "base": hex(BASE), "lines": LINES, "thread": 0, "cycle": 1, "param0": hex(P0), "param1": hex(P1),
        "scores": results,
        "finite_bound_witness": finite_bound(),
        "triple_counterexamples": triple_counterexamples(),
        "seal_collision": seal_collision(),
        "rng_coverage": rng_coverage(),
        "dbi_balanced_alphabet": {
            "available_balanced_bytes": math.comb(8, 4),
            "current_mode14_bytes": 16,
            "max_balanced_bits_per_byte": math.log2(math.comb(8, 4)),
        },
    }
    (AUDIT / "simulation-results.json").write_text(json.dumps(findings, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: v for k, v in findings.items() if k != "scores"}, indent=2))


if __name__ == "__main__":
    main()
