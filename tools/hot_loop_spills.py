"""Stack accesses in the hot loops of TMR's test code (TODO 99), from the post-LTO asm.

A hot loop is an innermost loop (a backward branch's range with no other loop inside it) in a test
module's function that makes no call and touches memory off the stack, at most 150 instructions.
Its stack operands (`(%rsp)`) are values the register allocator couldn't keep in registers. Two
kinds, worst first:
  round trip  a slot stored and reloaded each iteration: a value carried through memory, so a
              store-to-load latency on the loop's dependency chain
  reload      a value the loop never changes, reloaded each iteration: an L1 load slot
Each is counted by the register it spills: general purpose (gpr), or SIMD (a vector register,
xmm/ymm/zmm, or an AVX-512 mask, k0-k7). Cold functions (rescans, parsers, setup) are left out by
name, below.

Build the asm, then run this (with no file, the newest one under target/release/build):
    cargo rustc --release --bin tmr -- --emit asm
    python tools/hot_loop_spills.py [file.s] [--detail SUBSTRING [--all]] [--max N]
--detail prints each flagged loop of the kernels whose name contains SUBSTRING (with --all, their
clean hot loops too); --max exits 1 when the total flagged loops exceed N (a ratchet for a check
before releases).
"""
import glob
import os
import re
import sys
from collections import defaultdict

# The test modules, as they appear in v0 symbols (`3tmr` + length + name)
MODULES = ["5tests", "8sequence", "4seal", "15bandwidth_tests", "13latency_tests", "16latency_tests_v2",
           "12test_harness", "7pattern", "11pattern_gen", "16pattern_switch24"]
# Functions in them that run off the hot path: on an error, once per test, or at load time
COLD = ["rescan", "parse", "percentiles", "TestRunner", "scale_or"]
MAX_INSTRUCTIONS = 150

LABEL = re.compile(r"^(\.L\w+):")
BRANCH = re.compile(r"^\s+(j[a-z]+)\s+(\.L\w+)")
OFF_STACK = re.compile(r"-?\d*\((%r(?!sp\b)\w+)")
STACK = re.compile(r"(-?\d*)\(%rsp\)")
SIMD = re.compile(r"%[xyz]mm\d+|%k[0-7]\b")
KINDS = ["round trip gpr", "round trip simd", "reload gpr", "reload simd"]


def short_name(sym):
    """`module::function` from a v0 symbol: its length-prefixed identifiers after the crate's, and a
    `{closure}` mark where the function is one of a test fn's closures."""
    at = sym.find("3tmr")
    if at < 0:
        return sym[:60]
    # Every identifier after the crate's: `<length><name>`, skipping the path tags between them
    # (an impl's `NtB2_`, a closure's `s0_`)
    names, i, closure = [], at, False
    while i < len(sym):
        if not sym[i].isdigit():
            # A back-reference (`B5_`) or a disambiguator (`s0_`, `s_`) runs to its `_`
            if sym[i] in "Bs" and (m := re.match(r"[Bs][0-9A-Za-z]*_", sym[i:])):
                closure |= sym[i] == "s"
                i += m.end()
                continue
            i += 1
            continue
        j = i
        while j < len(sym) and sym[j].isdigit():
            j += 1
        n = int(sym[i:j])
        ident = sym[j:j + n]
        if n == 0 or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", ident) or len(ident) < n:
            i = j
            continue
        # A closure's disambiguator (`_`, `_0`) isn't a name
        if not re.fullmatch(r"_\d*", ident):
            names.append(ident)
        i = j + n
    name = "::".join(names[1:]) or sym[:60]
    return name + ("::{closure}" if closure else "")


def functions(path):
    out, name, body = {}, None, []
    with open(path, encoding="utf-8", errors="replace") as f:
        lines = f.read().splitlines()
    for line in lines:
        m = re.match(r"^(_R\w+):", line)
        if m:
            if name:
                out[name] = body
            sym = m.group(1)
            hot = any(f"3tmr{mod}" in sym for mod in MODULES) and not any(c in sym for c in COLD)
            name, body = (sym if hot else None), []
            continue
        if name:
            if line.startswith(".Lfunc_end") or "seh_endproc" in line:
                out[name] = body
                name, body = None, []
            else:
                body.append(line)
    return out


def innermost_loops(body):
    labels = {}
    for i, line in enumerate(body):
        m = LABEL.match(line)
        if m:
            labels[m.group(1)] = i
    loops = [(labels[m.group(2)], i) for i, line in enumerate(body)
             if (m := BRANCH.match(line)) and m.group(2) in labels and labels[m.group(2)] < i]
    for s, e in loops:
        if any(s <= s2 and e2 <= e and (s2, e2) != (s, e) for s2, e2 in loops):
            continue
        yield [x.strip() for x in body[s:e + 1] if x.startswith("\t") and not x.strip().startswith((".", "#"))]


def classify(loop):
    """None for a loop that isn't hot, else (kind, stack lines): "clean" when it keeps everything in
    registers."""
    if len(loop) > MAX_INSTRUCTIONS or any(x.startswith("call") for x in loop):
        return None
    if not any(OFF_STACK.search(x) for x in loop):
        return None
    stack = [x for x in loop if "(%rsp)" in x]
    if not stack:
        return "clean", []
    loads, stores, simd = set(), set(), set()
    for x in stack:
        offset = STACK.search(x).group(1)
        operands = x.split(None, 1)[1] if " " in x else ""
        dest = operands.rsplit(",", 1)[-1].strip()
        if STACK.search(dest) and not x.startswith(("cmp", "test")):
            stores.add(offset)
        else:
            loads.add(offset)
        if SIMD.search(x):
            simd.add(offset)
    trips = loads & stores
    # The loop's worst spill decides its kind; SIMD when that spill is a SIMD register's
    if trips:
        return ("round trip simd" if trips & simd else "round trip gpr"), stack
    return ("reload simd" if simd else "reload gpr"), stack


def main():
    args = sys.argv[1:]
    detail = args[args.index("--detail") + 1] if "--detail" in args else None
    show_clean = "--all" in args
    limit = int(args[args.index("--max") + 1]) if "--max" in args else None
    files = [a for a in args if a.endswith(".s")]
    path = files[0] if files else max(glob.glob("target/release/build/tmr-*/out/tmr.s"), key=os.path.getmtime)
    counts = defaultdict(lambda: dict.fromkeys(KINDS, 0))
    for sym, body in functions(path).items():
        name = short_name(sym)
        for loop in innermost_loops(body):
            found = classify(loop)
            if not found:
                continue
            kind, stack = found
            if kind != "clean":
                counts[name][kind] += 1
            elif not show_clean:
                continue
            if detail and detail in name:
                print(f"--- {name}: {kind}, {len(loop)} instructions, {len(stack)} on the stack")
                print("    " + "\n    ".join(re.sub(r"\s+", " ", x) for x in loop))
    total = {k: sum(c[k] for c in counts.values()) for k in KINDS}
    print(f"{'':48s} {'round trip':>16s} {'reload':>14s}")
    print(f"{'kernel':48s} {'gpr':>7s} {'simd':>8s} {'gpr':>7s} {'simd':>6s}")
    for name in sorted(counts, key=lambda n: tuple(-counts[n][k] for k in KINDS) + (n,)):
        c = counts[name]
        print(f"{name[:48]:48s} " + " ".join(f"{c[k]:{w}d}" for k, w in zip(KINDS, (7, 8, 7, 6))))
    print(f"{'total (' + str(len(counts)) + ' kernels)':48s} " + " ".join(f"{total[k]:{w}d}" for k, w in zip(KINDS, (7, 8, 7, 6))))
    if limit is not None and sum(total.values()) > limit:
        print(f"FAIL: {sum(total.values())} hot loops touch the stack, over {limit}")
        sys.exit(1)


if __name__ == "__main__":
    main()
