"""Stack accesses in the hot loops of TMR's test code (TODO 99), from the post-LTO asm.

A hot loop is an innermost loop (a backward branch's range with no other loop inside it) in a test
module's function that makes no call and touches memory off the stack, at most 150 instructions.
Its stack operands (`(%rsp)`) are values the register allocator couldn't keep in registers. Two
kinds, worst first:
  round trip  a slot stored and reloaded each iteration: a value carried through memory, so a
              store-to-load latency on the loop's dependency chain
  reload      a value the loop never changes, reloaded each iteration: an L1 load slot
Each is counted by the register it spills: general purpose (gpr), or SIMD (a vector register,
xmm/ymm/zmm, or an AVX-512 mask, k0-k7). Two last columns count hot loops, spilling or not, for
review and measurement rather than failure:
  indexed     memory addressed with an index register (`disp(%base,%index,8)`) rather than a walked
              pointer and a fixed displacement. It cost Mem-CacheBust 6% (and a folded load in a
              3-operand vector op splits in two on Intel), but it isn't always slower; a 512-bit
              verify's 16% in L2, once put down to it, was a lost fused vpternlogq (Rule 6.3).
  const       a constant loaded from the binary's data (`(%rip)`) every iteration: a reload, like a
              stack one, that the stack columns can't see.
  long        over 150 instructions: the heavy pattern families' unrolled steps run to 200 and more,
              so read it rather than assume it is cold (loops over 600 are left out).
  width       in a kernel whose name gives a width (128, 256 or 512), an access to test memory
              through a vector register of another width: the compiler widened a narrow variant
              (Spd-Copy-128 ran on ymm) or split a wide one (a u64x8 as two ymm halves).
A stack access through an index register (`128(%rsp,%r10)`, an array kept on the stack) counts as a
stack access; an indexed prefetch doesn't count as indexed (it folds into no other op). A "loop"
whose range holds a `ret` is an epilogue laid out inside an outer loop's blocks, and is skipped.
Cold functions (rescans, parsers, setup) are left out by name, below.

A hot-shaped loop that makes a call isn't in the table: it would be a chunk's or a phase's loop,
or a hot loop that picked up a call (an intrinsic that didn't inline, a helper kept out of line,
a copy or fill loop LLVM turned into memcpy or memset). Below the table they are counted by why
(`calls_in`): timing, an error path, bookkeeping or allocation (`COLD_CALLS`), dispatch (the call
is a test function with a loop of its own, or a few calls below), or a struct move (a memcpy of a
constant size); the rest are listed by kernel and callee, to read: an indirect call, a memcpy or
memset of a size the loop computes, a test function with no loop under it. A kernel generic over the
access a unit test records it by (`tests::access`) is named without it: `Direct` is the only one
in the binary.

Build the asm, then run this (with no file, the newest one under target/release/build):
    cargo rustc --release -p tmr-app --bin tmr -- --emit asm
    python tools/hot_loop_spills.py [file.s] [--detail SUBSTRING [--all]] [--max N]
--detail prints each flagged loop (stack or any review flag) of the kernels whose name contains
SUBSTRING (with --all, their clean hot loops too); --max exits 1 when the loops touching the stack
exceed N (a ratchet for a check before releases). An asm that isn't the binary's exits 1: one with
no `main`. Since TODO 29 S3 the bin is tmr-app's, and the old command,
    cargo rustc --release -- --emit asm
builds the lib's asm instead, pre-LTO, as `tmr-<hash>.s`, which reads as a believable table; so the
newest asm is looked for among those too, and refused rather than passed over for an older
`tmr.s`. Finding no hot loop exits 1 too: the engine's crate name has drifted from the `3tmr` its
symbols are matched by, or MODULES from its modules.
"""
import glob
import os
import re
import sys
from collections import defaultdict

# The test modules, as they appear in v0 symbols (`3tmr` + length + name)
MODULES = ["5tests", "8sequence", "4seal", "15bandwidth_tests", "13latency_tests", "16latency_tests_v2",
           "12test_harness", "7pattern", "11pattern_gen", "16pattern_switch24", "5xform"]
# Functions in them that run off the hot path: on an error, once per test, or at load time
COLD = ["rescan", "parse", "percentiles", "finalize_stats", "TestRunner", "scale_or",
        # Mem-TileTransform's set-up (once a test) and tier 2's helpers (TODO 102)
        "PatternSet", "Matrices", "distinct_odd", "gen_patterns", "self_test", "group_checksum", "still_wrong", "signature"]
# Calls a hot-shaped loop may make off its hot path, by symbol substring: timing (once a chunk), error and panic paths, the
# bookkeeping and allocation around a chunk, formatting, a one-time initialisation
COLD_CALLS = {
    "timing": ["3std4time", "QueryPerformanceCounter", "perf_counter", "9functions5sleep"],
    "error": ["unwrap_failed", "expect_failed", "slice_index_fail", "panicking", "handle_error", "Rescan", "rescan", "13error_context"],
    "bookkeeping": ["16test_scaffolding", "raw_vec", "HeapFree", "GetProcessHeap", "process_heap_alloc", "alloc3fmt", "once_lock"],
}
# memcpy and its kind: with a constant size it is a struct moved, not a copy loop LLVM collapsed
MEM_CALLS = ("memcpy", "memmove", "memset")
CONST_SIZE = re.compile(r"^mov[lq]\s+\$(\d+), %r8d?$")
MAX_INSTRUCTIONS = 600
# The binary's post-LTO asm: the bin is tmr-app's (TODO 29 S3)
BUILD = "cargo rustc --release -p tmr-app --bin tmr -- --emit asm"
# Where the build leaves asm: the bin's `tmr.s`; the lib's `tmr-<hash>.s`, to refuse when it is the
# newest; and the layout before cargo's build directories, `deps/tmr-<hash>.s`
ASM = ["target/release/build/tmr*/**/tmr.s", "target/release/build/tmr*/**/tmr-*.s", "target/release/deps/tmr-*.s"]
LONG = 150

LABEL = re.compile(r"^(\.L\w+):")
BRANCH = re.compile(r"^\s+(j[a-z]+)\s+(\.L\w+)")
OFF_STACK = re.compile(r"-?\d*\((%r(?!sp\b)\w+)")
STACK = re.compile(r"(-?\d*)\(%rsp(?:,%r\w+(?:,\d)?)?\)")
SIMD = re.compile(r"%[xyz]mm\d+|%k[0-7]\b")
KINDS = ["round trip gpr", "round trip simd", "reload gpr", "reload simd"]
# Counted for review, not failure: see the module docs
REVIEW = ["indexed", "const", "long", "width"]
# The width a kernel's name gives, and the width of each vector register class
NAMED_WIDTH = re.compile(r"(?<![0-9])(128|256|512)(?![0-9])")
REG_WIDTH = {"x": 128, "y": 256, "z": 512}


def off_width(name, loop):
    """Whether a loop of the kernel `name` accesses test memory (off the stack, not the binary's
    constants) through a vector register of another width than its name gives."""
    m = NAMED_WIDTH.search(name)
    if not m:
        return False
    want = int(m.group(1))
    for x in loop:
        if not OFF_STACK.search(x) or "(%rip)" in x or x.startswith(("lea", "prefetch")):
            continue
        widths = {REG_WIDTH[r] for r in re.findall(r"%([xyz])mm\d+", x)}
        if widths and max(widths) != want:
            return True
    return False
# A memory operand with an index register, off the stack (an `lea` is arithmetic, not an access)
INDEXED = re.compile(r"-?\d*\(%r(?!sp\b)\w+,%r\w+(,\d)?\)")
# A type of `tests::access` as a generic argument (`NtNtB6_6access6Direct`, its length-prefixed name
# after): the access a kernel's functions take so a unit test can record them. Only `Direct` is in
# the binary, so the kernel's name leaves it out and stays the function's.
ACCESS_ARG = re.compile(r"NtNt(?:NtB[0-9A-Za-z]*_5tests|B[0-9A-Za-z]*_)6access(\d+)")


def drop_access_args(sym):
    while m := ACCESS_ARG.search(sym):
        sym = sym[:m.start()] + sym[m.end() + int(m.group(1)):]
    return sym


def short_name(sym):
    """`module::function` from a v0 symbol: its length-prefixed identifiers after the crate's, and a
    `{closure}` mark where the function is one of a test fn's closures."""
    sym = drop_access_args(sym)
    at = sym.find("3tmr")
    if at < 0:
        return sym[:60]
    # Every identifier after the crate's: `<length><name>`, skipping the path tags between them
    # (an impl's `NtB2_`, a closure's `s0_`)
    names, i, closure, consts = [], at, False, []
    while i < len(sym):
        if not sym[i].isdigit():
            # A const-generic argument: `K`, its type, then its value in hex (`Kj10_` is 16)
            if sym[i] == "K" and (m := re.match(r"K[a-z](n?)([0-9a-f]*)_", sym[i:])):
                consts.append(("-" if m.group(1) else "") + str(int(m.group(2) or "0", 16)))
                i += m.end()
                continue
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
    if consts:
        name += "::<" + ",".join(consts) + ">"
    return name + ("::{closure}" if closure else "")


def functions(path, modules=MODULES, cold=COLD):
    """Each function of `modules` but the `cold` ones, by its symbol: its lines. `asm_fingerprint.py`
    reads more of them than the spill table does."""
    out, name, body = {}, None, []
    with open(path, encoding="utf-8", errors="replace") as f:
        lines = f.read().splitlines()
    for line in lines:
        m = re.match(r"^(_R\w+):", line)
        if m:
            if name:
                out[name] = body
            sym = m.group(1)
            hot = any(f"3tmr{mod}" in sym for mod in modules) and not any(c in sym for c in cold)
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


def loops_below(sym, kernels, depth=3, seen=None):
    """Whether the test function `sym`, or one it calls a few levels down, has a loop: a kernel that
    hands its work to the closures holding its loops (`sequence_read_256` to `::stride::{closure}`)
    is a kernel all the same, not a helper that should have inlined."""
    seen = set() if seen is None else seen
    if sym in seen or depth < 0:
        return False
    seen.add(sym)
    body = kernels[sym]
    if next(innermost_loops(body), None) is not None:
        return True
    callees = {x.split(None, 1)[-1] for x in (y.strip() for y in body) if x.startswith("call")}
    return any(c in kernels and loops_below(c, kernels, depth - 1, seen) for c in callees)


def calls_in(loop, kernels):
    """A hot-shaped loop's calls, each (why, callee): a cold call (`COLD_CALLS`' kinds), "dispatch" (a
    test function with loops of its own, the loop handing it a chunk or a phase), "struct move N B"
    (a memcpy of a constant size), or "read" for the rest: an indirect call, a memcpy or memset of a
    size it computes (a copy or fill loop LLVM may have collapsed), a helper that didn't inline. None
    for a loop that isn't hot-shaped or makes no call. `kernels`: the test functions' bodies."""
    if len(loop) > MAX_INSTRUCTIONS or any(x.startswith("ret") for x in loop) or not any(OFF_STACK.search(x) for x in loop):
        return None
    found = []
    for i, x in enumerate(loop):
        if not x.startswith("call"):
            continue
        callee = x.split(None, 1)[-1]
        why = next((kind for kind, subs in COLD_CALLS.items() if any(s in callee for s in subs)), None)
        if why is None and callee in MEM_CALLS:
            # The size is the third argument, %r8, set a few instructions before
            size = next((m.group(1) for y in loop[max(0, i - 6):i][::-1] if (m := CONST_SIZE.match(y))), None)
            why = f"struct move {size} B" if size else "read"
        elif why is None and callee in kernels:
            why = "dispatch" if loops_below(callee, kernels) else "read"
        elif why is None and callee.startswith("_R") and any(c in callee for c in COLD):
            why = "error"
        found.append((why or "read", callee))
    # A loop that hands a kernel its chunk or phase makes its other calls once a chunk or phase too (`Program::phase` beside it)
    if any(why == "dispatch" for why, _ in found):
        found = [(why if why != "read" else "dispatch", callee) for why, callee in found]
    return found or None


def classify(loop):
    """None for a loop that isn't hot, else (kind, stack lines, review flags): kind "clean" when it
    keeps everything in registers; the flags are `REVIEW` names."""
    if len(loop) > MAX_INSTRUCTIONS or any(x.startswith(("call", "ret")) for x in loop):
        return None
    if not any(OFF_STACK.search(x) for x in loop):
        return None
    flags = []
    if any(INDEXED.search(x) and not x.startswith(("lea", "prefetch")) for x in loop):
        flags.append("indexed")
    if any("(%rip)" in x and not x.startswith("lea") for x in loop):
        flags.append("const")
    if len(loop) > LONG:
        flags.append("long")
    stack = [x for x in loop if STACK.search(x)]
    if not stack:
        return "clean", [], flags
    loads, stores, simd = set(), set(), set()
    for x in stack:
        offset = STACK.search(x).group(1)
        operands = x.split(None, 1)[1] if " " in x else ""
        # The last operand: split at the commas outside an address's parentheses
        dest = re.split(r",(?![^(]*\))", operands)[-1].strip()
        if STACK.search(dest) and not x.startswith(("cmp", "test")):
            stores.add(offset)
        else:
            loads.add(offset)
        if SIMD.search(x):
            simd.add(offset)
    trips = loads & stores
    # The loop's worst spill decides its kind; SIMD when that spill is a SIMD register's
    if trips:
        return ("round trip simd" if trips & simd else "round trip gpr"), stack, flags
    return ("reload simd" if simd else "reload gpr"), stack, flags


def callee_name(callee):
    """A call's target as the table names a function: a symbol shortened, an import by its name"""
    if callee.startswith("_R"):
        return short_name(callee)
    m = re.match(r"\*__imp_(\w+)", callee)
    return m.group(1) if m else callee


def binary_asm(args):
    """The asm to read: the file named in `args`, else the newest the build left. Exits unless it is
    the binary's, the one with a `main`: the lib's has the same kernels before LTO (TODO 29 S3)."""
    files = [a for a in args if a.endswith(".s")]
    found = [f for pattern in ASM for f in glob.glob(pattern, recursive=True)]
    if not files and not found:
        sys.exit(f"no asm: build it with `{BUILD}`")
    path = files[0] if files else max(found, key=os.path.getmtime)
    with open(path, encoding="utf-8", errors="replace") as f:
        if not any(line.startswith("main:") for line in f):
            sys.exit(f"FAIL: {path} isn't the binary's asm (no `main`), so not post-LTO: build it with `{BUILD}`")
    return path


def main():
    args = sys.argv[1:]
    detail = args[args.index("--detail") + 1] if "--detail" in args else None
    show_clean = "--all" in args
    limit = int(args[args.index("--max") + 1]) if "--max" in args else None
    path = binary_asm(args)
    columns = KINDS + REVIEW
    counts = defaultdict(lambda: dict.fromkeys(columns, 0))
    hot = set()
    kernels = functions(path)
    calling = defaultdict(int)  # loops that call, by why
    to_read = []
    for sym, body in kernels.items():
        name = short_name(sym)
        for loop in innermost_loops(body):
            called = calls_in(loop, kernels)
            if called:
                # A loop with a call isn't in the table; one whose calls aren't all known is listed below it
                whys = {why for why, _ in called}
                calling["read" if "read" in whys else sorted(whys)[0]] += 1
                if "read" in whys:
                    to_read.append((name, len(loop), sorted({callee_name(c) for why, c in called if why == "read"})))
                continue
            found = classify(loop)
            if not found:
                continue
            kind, stack, flags = found
            if off_width(name, loop):
                flags = flags + ["width"]
            hot.add(name)
            if kind != "clean":
                counts[name][kind] += 1
            for flag in flags:
                counts[name][flag] += 1
            if kind == "clean" and not flags and not show_clean:
                continue
            if detail and detail in name:
                print(f"--- {name}: {', '.join([kind] + flags)}, {len(loop)} instructions, {len(stack)} on the stack")
                print("    " + "\n    ".join(re.sub(r"\s+", " ", x) for x in loop))
    # Else an empty table reads as a clean one (TODO 29 S3)
    if not hot:
        sys.exit(f"FAIL: no hot loop in {path}: the crate isn't `tmr` (`3tmr`), or MODULES aren't its modules")
    widths = (7, 8, 7, 6, 8, 6, 5, 6)
    total = {k: sum(c[k] for c in counts.values()) for k in columns}
    print(f"{'':48s} {'round trip':>16s} {'reload':>14s}")
    print(f"{'kernel':48s} {'gpr':>7s} {'simd':>8s} {'gpr':>7s} {'simd':>6s} {'indexed':>8s} {'const':>6s} {'long':>5s} {'width':>6s}")
    for name in sorted(counts, key=lambda n: tuple(-counts[n][k] for k in columns) + (n,)):
        c = counts[name]
        print(f"{name[:48]:48s} " + " ".join(f"{c[k]:{w}d}" for k, w in zip(columns, widths)))
    print(f"{'total (' + str(len(counts)) + ' kernels)':48s} " + " ".join(f"{total[k]:{w}d}" for k, w in zip(columns, widths)))
    print(f"kernels whose hot loops are all clean and unflagged: {len(hot - set(counts))}")
    known = ", ".join(f"{n} {why}" for why, n in sorted(calling.items()) if why != "read")
    print(f"hot-shaped loops that make a call, so not in the table: {sum(calling.values())}"
          f" ({known}; {calling['read']} to read)")
    for name, length, callees in sorted(to_read):
        print(f"  {name[:60]:60s} {length:4d} instructions, calls {', '.join(callees)}")
    spilling = sum(total[k] for k in KINDS)
    if limit is not None and spilling > limit:
        print(f"FAIL: {spilling} hot loops touch the stack, over {limit}")
        sys.exit(1)


if __name__ == "__main__":
    main()
