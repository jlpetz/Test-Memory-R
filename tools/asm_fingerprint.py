"""A hash of each function of the test code in TMR's post-LTO asm, to show a change left the
kernels' instructions as they were (TODO 29).

The functions are every one of `hot_loop_spills.py`'s MODULES, its COLD ones too (a kernel's
rescan, which formats the error lines, runs off the hot path but is the test's all the same), and
the workers' side of a step: `thread_pool` (the worker loop, WorkItem and WorkResult) and
`test_scaffolding` (TestRunner's bookkeeping). They are named as `hot_loop_spills.py` names them.
Each body is hashed after normalising what changes when the code doesn't:
  labels      `.LBB2750_1`, `.Ltmp3`, `.LCPI12_0`, `.LJTI4_0` and string constants
              (`anon.<hash>.7`) are renumbered in their order of use in the function
  copies      LLVM's numbered copies of a symbol (`<sym>.449`): a function's lose the number, as a
              call reaching one copy or another is the same call; data's (`switch.table.<sym>.1518.rel`,
              a static split into its fields, `<sym>.0`) are renumbered in their order of use, each
              symbol's apart, and hashed with their data
  symbols     v0 symbols lose their crate hashes (`Cs<hash>_`) and back-references (`B4_`), whose
              offsets move with the hashes' lengths, and a closure its place among its function's
              closures (`s5_0`), which moves when a closure is added before it
  sections    a COMDAT's number (`unique,2454`), which counts the file's sections
  data        what the function refers to by such a name (a constant pool, a jump table, a switch
              table, a string) is hashed with it; `__xmm@...`-style constants are named by content,
              and a number in it is written one way (LLVM writes `.quad 0x7fffffffffffffff` for a
              constant pool it typed as floats, `.quad 9223372036854775807` for one of integers)
  dropped     comments, debug-line directives, and the line table's `.Ltmp` labels, which nothing in
              the function names (one an instruction names is renumbered); alignment directives
              are kept
Two functions with one name (a generic's instances, a test fn's closures) keep a sorted list of
hashes. Checked on 2026-10-22: a rebuild with every crate hash changed gives the same hashes.
TODO 29 S3 moved the bin into a crate of its own: calls reached the lib's copy of `<String as
Display>::fmt` where they had reached another, and with one counter for all the copies a function
names, that renumbered the rest. So function copies lose their numbers, and data's are counted per
symbol. With the GUI linked in, a constant pool the kernels share with Reactor's code came out
in hex.

A source line can be in a function's code: a log record's line, inlined from another module, is
an immediate `Some(line)`, `(line << 32) | 1` (`movabsq $18979460481025`, line 4419). Lines move
when code above them does, so each function has a second hash with those masked, and --compare
lists a function whose code differs only there as `lines`, apart from the changed ones (TODO 29
S1: lines added to runner.rs above `pin_thread_to_cpu_with_config` showed in the worker loop). A
line in a panic's location is in its data, as bytes of a string, and isn't masked.

Build the asm, then:
    cargo rustc --release -p test-memory-r --bin tmr -- --emit asm
    python tools/asm_fingerprint.py [file.s]                    # name, lines hashed, hash
    python tools/asm_fingerprint.py [file.s] --save FILE.json   # a baseline
    python tools/asm_fingerprint.py [file.s] --compare FILE.json
    python tools/asm_fingerprint.py [file.s] --dump DIR         # each normalised body, to diff by hand
--compare lists the functions changed, gone and new, and exits 1 on any; one changed only in its
source lines is listed too, and doesn't count. A baseline records the
compiler that built its asm (the asm names it); when this asm's is another, --compare says so, and
exits 2 rather than 1 on a change, which may be the compiler's. With no file, the newest asm under
target/release/build, as hot_loop_spills.py takes it; an asm that isn't the binary's is refused as
it refuses it. Finding no function is an error: the lib's name or MODULES drifted.
"""
import hashlib
import json
import os
import re
import subprocess
import sys
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.dont_write_bytecode = True  # no __pycache__ in tools/
import hot_loop_spills  # noqa: E402  (the selection and the names are its)

# A COMDAT section's number in the file: `.section .text,"xr",one_only,<sym>,unique,2454`
UNIQUE = re.compile(r"\bunique,\d+\b")
# An immediate, which may be a `Some(line)`: `(line << 32) | 1`
IMMEDIATE = re.compile(r"\$(\d+)\b")
# Above the longest source file's last line: a `Some(line)` names a line below it
MAX_LINE = 1 << 20
# A name an instruction or a data line can refer to: a label, a symbol, `switch.table.<sym>.4.rel`
NAME = re.compile(r"[A-Za-z_.$@][\w.$@]*")
LOCAL = re.compile(r"\.L([A-Za-z_]+?)(\d[\d_]*)\b")
ANON = re.compile(r"^anon\.[0-9a-f]+\.\d+$")
SYMBOL = re.compile(r"_R[0-9A-Za-z_]+")
# LLVM's number for a local copy of a symbol
COPY = re.compile(r"\.\d+(?=\.rel$|$)")
DATA = re.compile(r"^\s+\.(quad|long|short|byte|zero|ascii|asciz)\b")
# A whole number as data, in decimal or hex, and the bits of its directive
NUMBER = re.compile(r"^\.(quad|long|short|byte)\s+(-?(?:0x[0-9a-fA-F]+|\d+))$")
BITS = {"quad": 64, "long": 32, "short": 16, "byte": 8}
SKIPPED = (".loc", ".cv_", ".file")
# A temporary label's definition. The line table (release's `debug = "line-tables-only"`, since
# d54fb4c) puts one at almost every instruction, which only the debug sections name; std's inlined
# code has a few without it
TEMP = re.compile(r"^\.Ltmp\d+:$")
# The workers' side of a step, which the spill table leaves out (no kernel loop of its own)
WORKERS = ["11thread_pool", "16test_scaffolding"]
# The compiler, as the debug info names it: `rustc version 1.101.0-nightly (1a2c4797e 2026-10-21)`
PRODUCER = re.compile(r'"[^"]*\(rustc version ([^"]+)\)"')


# v0's basic types, and a const value after its type: `j80_` is a usize 0x80
BASIC = "abcdefhijlmnostuxyz"
CONST = re.compile(r"[a-z]n?[0-9a-f]*_|p")


def type_end(sym, j):
    """Where the type at `sym[j]` ends, when it is a basic type, a reference or pointer to one, or an
    array of those (`A`, its element, its length); `None` for another. An array's length is a const
    (`Ayj80_`, `[u64; 128]`), whose digits read as an identifier's length would swallow the rest of
    the symbol, crate hashes and all."""
    c = sym[j:j + 1]
    if not c:
        return None
    if c in BASIC:
        return j + 1
    if c in "RQPO":
        k = j + 1
        # A reference's lifetime
        if c in "RQ" and (m := re.match(r"L[0-9A-Za-z]*_", sym[k:])):
            k += m.end()
        return type_end(sym, k)
    if c == "A":
        k = type_end(sym, j + 1)
        m = CONST.match(sym, k) if k is not None else None
        return m.end() if m else None
    return None


def plain_symbol(sym):
    """A v0 symbol without its crate hashes (`Cs<base62>_`), its back-references (`B<base62>_`)
    and the crate a generic was instantiated in (a crate root ending the symbol): which crate's copy
    of `<&T as Display>::fmt` a call reaches moves with the hashes. Identifiers, disambiguators
    (`s0_`), lifetimes, const values and arrays of basic types (`type_end`) are copied whole."""
    out, i, root = [], 0, None
    while i < len(sym):
        c = sym[i]
        if c.isdigit():
            j = i
            while j < len(sym) and sym[j].isdigit():
                j += 1
            n = int(sym[i:j])
            # An identifier that starts with a digit or `_` has a `_` between it and its length
            if j < len(sym) and sym[j] == "_" and n > 0:
                j += 1
            out.append(sym[i:j + n])
            i = j + n
        elif c in "BLG" and (m := re.match(r"[BLG][0-9A-Za-z]*_", sym[i:])):
            # A back-reference, a lifetime, a binder
            out.append("B" if c == "B" else m.group(0))
            i += m.end()
        elif c == "s" and (m := re.match(r"s[0-9A-Za-z]*_", sym[i:])) and (
                out[-1:] in (["X"], ["M"]) or sym[i + m.end():i + m.end() + 1].isdigit()):
            # A disambiguator: an impl's, or an identifier's (an `s` elsewhere is the type i16). A
            # closure's, before its empty identifier `0`, is its place among its function's closures,
            # which moves when one is added before it: dropped, as the first closure has none
            if sym[i + m.end():i + m.end() + 1] != "0":
                out.append(m.group(0))
            i += m.end()
        elif c == "A" and (end := type_end(sym, i)):
            out.append(sym[i:end])
            i = end
        elif c == "K" and (m := re.match(r"K(p|[a-z]n?[0-9a-f]*_)", sym[i:])):
            out.append(m.group(0))
            i += m.end()
        elif c == "C" and out[-1:] != ["N"]:
            # A crate root; after an `N`, a closure's namespace instead
            root = len(out)
            out.append(c)
            i = sym.index("_", i) + 1 if sym[i + 1:i + 2] == "s" else i + 1
        else:
            out.append(c)
            i += 1
    # A crate root and its name, last: the instantiating crate
    if root and len(out) == root + 2:
        out = out[:root]
    return "".join(out)


def number(line):
    """A data line with its number, if it is one, in unsigned decimal: one spelling for one value."""
    m = NUMBER.match(line)
    if not m:
        return line
    value = int(m.group(2), 16 if "0x" in m.group(2) else 10)
    return f".{m.group(1)}\t{value & ((1 << BITS[m.group(1)]) - 1)}"


def label_data(lines):
    """The data under each label that has some: constant pools, jump tables, switch tables and
    strings. A function's own label has instructions under it, so it has none."""
    data, label = {}, None
    for line in lines:
        m = re.match(r"^([A-Za-z_.$@][\w.$@]*):", line)
        if m:
            label = m.group(1)
            data[label] = []
        elif label and DATA.match(line):
            data[label].append(number(line.strip()))
        else:
            label = None
    return {k: v for k, v in data.items() if v}


def normalise(body, data):
    """The function's lines as they are hashed, then the data it refers to by name."""
    numbers = defaultdict(dict)
    used = []

    def renumber(kind, key):
        return numbers[kind].setdefault(key, len(numbers[kind]))

    def name(m):
        text = m.group(0)
        if text in data and text not in used:
            used.append(text)
        if LOCAL.match(text):
            return LOCAL.sub(lambda m: f".L{m.group(1)}#{renumber(m.group(1), m.group(2))}", text)
        if ANON.match(text):
            return f"anon#{renumber('anon', text)}"
        plain = SYMBOL.sub(lambda s: plain_symbol(s.group(0)), text)
        if plain != text and COPY.search(text):
            # Data, hashed with the function: which of its symbol's copies, in order of use. A
            # function: the call is the same whichever copy it reaches.
            if text in data:
                return COPY.sub(f".#{renumber(COPY.sub('', plain), text)}", plain)
            return COPY.sub("", plain)
        return plain

    kept = []
    for line in body:
        code = "" if line.lstrip().startswith("#") else line.split("#", 1)[0].strip()
        if code and not code.startswith(SKIPPED):
            kept.append(code)
    # A temporary label nothing in the function names is the line table's, and goes; one an
    # instruction names stays, in its place
    named = {n for code in kept if not TEMP.match(code) for n in NAME.findall(code)}
    kept = [code for code in kept if not (TEMP.match(code) and code[:-1] not in named)]
    out = [NAME.sub(name, UNIQUE.sub("unique,#", code)) for code in kept]
    for label in used:
        out.append(NAME.sub(name, label) + ":")
        out += [NAME.sub(name, x) for x in data[label]]
    return out


def without_lines(lines):
    """The normalised lines with each immediate that may be a `Some(line)` masked."""
    def mask(m):
        value = int(m.group(1))
        return "$<line>" if value & 0xFFFFFFFF == 1 and 0 < value >> 32 < MAX_LINE else m.group(0)
    return [IMMEDIATE.sub(mask, line) for line in lines]


def digest(lines):
    return hashlib.sha256("\n".join(lines).encode()).hexdigest()[:16]


def fingerprints(path):
    """Each function's name, with its (hash, hash without its lines, normalised lines) per
    instance."""
    with open(path, encoding="utf-8", errors="replace") as f:
        data = label_data(f.read().splitlines())
    found = defaultdict(list)
    for sym, body in hot_loop_spills.functions(path, hot_loop_spills.MODULES + WORKERS, cold=()).items():
        # A static's label reads as a function there too, with whatever data follows it
        if not any(line.startswith(".Lfunc_begin") for line in body[:2]):
            continue
        lines = normalise(body, data)
        found[hot_loop_spills.short_name(sym)].append((digest(lines), digest(without_lines(lines)), lines))
    return {name: sorted(found[name]) for name in sorted(found)}


def compiler(path):
    """The compiler that built the asm, as `rustc --version` gives it: from the asm's debug info,
    else (an asm built without it) the toolchain here."""
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            if m := PRODUCER.search(line):
                return f"rustc {m.group(1)}"
    return subprocess.run(["rustc", "--version"], capture_output=True, text=True).stdout.strip()


def main():
    args = sys.argv[1:]
    opt = {f: args[args.index(f) + 1] for f in ("--save", "--compare", "--dump") if f in args}
    path = hot_loop_spills.binary_asm(args)
    funcs = fingerprints(path)
    if not funcs:
        sys.exit(f"{path}: no hot-path function; check hot_loop_spills.py's MODULES against its symbols")
    table = {name: [d for d, _, _ in v] for name, v in funcs.items()}
    # Without the lines, sorted on their own: a list compares as the set of its name's instances
    masked = {name: sorted(m for _, m, _ in v) for name, v in funcs.items()}
    count = f"{sum(map(len, table.values()))} functions under {len(table)} names"
    if "--dump" in opt:
        os.makedirs(opt["--dump"], exist_ok=True)
        for name, v in funcs.items():
            for k, (_, _, lines) in enumerate(v):
                file = re.sub(r"[^\w.-]+", "_", name) + (f".{k}" if k else "") + ".s"
                with open(os.path.join(opt["--dump"], file), "w", encoding="utf-8") as f:
                    f.write("\n".join(lines) + "\n")
        print(f"{count} written to {opt['--dump']}")
    if "--save" in opt:
        rustc = compiler(path)
        # One function a line, so a baseline's diff in git reads as the functions that changed
        def block(hashes):
            return ",\n".join(f"  {json.dumps(name)}: {json.dumps(h)}" for name, h in hashes.items())
        with open(opt["--save"], "w", encoding="utf-8", newline="\n") as f:
            f.write(f'{{\n "asm": {json.dumps(path.replace(os.sep, "/"))},\n "rustc": {json.dumps(rustc)},\n'
                    f' "functions": {{\n{block(table)}\n }},\n "without_lines": {{\n{block(masked)}\n }}\n}}\n')
        print(f"{count} from {path} saved to {opt['--save']}")
    if "--compare" in opt:
        with open(opt["--compare"], encoding="utf-8") as f:
            base = json.load(f)
        differ = [n for n in table if n in base["functions"] and base["functions"][n] != table[n]]
        # A baseline saved before the hashes without lines has none: every difference counts
        lines = [n for n in differ if base.get("without_lines", {}).get(n) == masked[n]]
        changed = [n for n in differ if n not in lines]
        gone = [n for n in base["functions"] if n not in table]
        new = [n for n in table if n not in base["functions"]]
        for label, names in (("changed", changed), ("gone", gone), ("new", new), ("lines", lines)):
            for name in names:
                print(f"{label:8s} {name}")
        print(f"{count} against {len(base['functions'])} names in {opt['--compare']} ({base.get('rustc', '?')}): "
              f"{len(changed)} changed, {len(gone)} gone, {len(new)} new; {len(lines)} changed only in source lines")
        rustc = compiler(path)
        other = rustc != base.get("rustc")
        if other:
            print(f"WARNING: this asm is {rustc}'s, the baseline {base.get('rustc', '?')}'s: a change may be "
                  "the compiler's; rebuild the baseline's code with this toolchain to compare")
        sys.exit((2 if other else 1) if changed or gone or new else 0)
    if not opt:
        for name, v in funcs.items():
            for strict, _, lines in v:
                print(f"{name[:70]:70s} {len(lines):6d} {strict}")
        print(f"{count}, from {path}")


if __name__ == "__main__":
    main()
