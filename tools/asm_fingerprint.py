"""A hash of each function of the test code in TMR's post-LTO asm, to show a change left the
kernels' instructions as they were (TODO 29).

The functions are every one of `hot_loop_spills.py`'s MODULES, its COLD ones too (a kernel's
rescan, which formats the error lines, runs off the hot path but is the test's all the same), and
the workers' side of a step: `thread_pool` (the worker loop, WorkItem and WorkResult) and
`test_scaffolding` (TestRunner's bookkeeping). They are named as `hot_loop_spills.py` names them.
Each body is hashed after normalising what changes when the code doesn't:
  labels      `.LBB2750_1`, `.Ltmp3`, `.LCPI12_0`, `.LJTI4_0`, string constants (`anon.<hash>.7`)
              and LLVM's local copies of a symbol (`<sym>.449`, `switch.table.<sym>.1518.rel`)
              are renumbered in their order of use in the function
  symbols     v0 symbols lose their crate hashes (`Cs<hash>_`) and back-references (`B4_`), whose
              offsets move with the hashes' lengths
  data        what the function refers to by such a name (a constant pool, a jump table, a switch
              table, a string) is hashed with it; `__xmm@...`-style constants are named by content
  dropped     comments and debug-line directives; alignment directives are kept
Two functions with one name (a generic's instances, a test fn's closures) keep a sorted list of
hashes. Checked on 2026-10-22: a rebuild with every crate hash changed gives the same hashes.

Build the asm, then:
    cargo rustc --release --bin tmr -- --emit asm
    python tools/asm_fingerprint.py [file.s]                    # name, lines hashed, hash
    python tools/asm_fingerprint.py [file.s] --save FILE.json   # a baseline
    python tools/asm_fingerprint.py [file.s] --compare FILE.json
    python tools/asm_fingerprint.py [file.s] --dump DIR         # each normalised body, to diff by hand
--compare lists the functions changed, gone and new, and exits 1 on any. A baseline records the
compiler that built its asm (the asm names it); when this asm's is another, --compare says so, and
exits 2 rather than 1 on a change, which may be the compiler's. With no file, the newest asm under
target/release/build, as hot_loop_spills.py takes it. Finding no function is an error: the lib's
name or the build command drifted.
"""
import glob
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

# A name an instruction or a data line can refer to: a label, a symbol, `switch.table.<sym>.4.rel`
NAME = re.compile(r"[A-Za-z_.$@][\w.$@]*")
LOCAL = re.compile(r"\.L([A-Za-z_]+?)(\d[\d_]*)\b")
ANON = re.compile(r"^anon\.[0-9a-f]+\.\d+$")
SYMBOL = re.compile(r"_R[0-9A-Za-z_]+")
# LLVM's number for a local copy of a symbol
COPY = re.compile(r"\.\d+(?=\.rel$|$)")
DATA = re.compile(r"^\s+\.(quad|long|short|byte|zero|ascii|asciz)\b")
SKIPPED = (".loc", ".cv_", ".file")
# The workers' side of a step, which the spill table leaves out (no kernel loop of its own)
WORKERS = ["11thread_pool", "16test_scaffolding"]
# The compiler, as the debug info names it: `rustc version 1.101.0-nightly (1a2c4797e 2026-10-21)`
PRODUCER = re.compile(r'"[^"]*\(rustc version ([^"]+)\)"')


def plain_symbol(sym):
    """A v0 symbol without its crate hashes (`Cs<base62>_`), its back-references (`B<base62>_`)
    and the crate a generic was instantiated in (a crate root ending the symbol): which crate's copy
    of `<&T as Display>::fmt` a call reaches moves with the hashes. Identifiers, disambiguators
    (`s0_`), lifetimes and const values are copied whole."""
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
            # A disambiguator: an impl's, or an identifier's (an `s` elsewhere is the type i16)
            out.append(m.group(0))
            i += m.end()
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
            data[label].append(line.strip())
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
            return COPY.sub(f".#{renumber('copy', text)}", plain)
        return plain

    out = []
    for line in body:
        code = "" if line.lstrip().startswith("#") else line.split("#", 1)[0].strip()
        if code and not code.startswith(SKIPPED):
            out.append(NAME.sub(name, code))
    for label in used:
        out.append(NAME.sub(name, label) + ":")
        out += [NAME.sub(name, x) for x in data[label]]
    return out


def fingerprints(path):
    """Each function's name, with its (hash, normalised lines) per instance."""
    with open(path, encoding="utf-8", errors="replace") as f:
        data = label_data(f.read().splitlines())
    found = defaultdict(list)
    for sym, body in hot_loop_spills.functions(path, hot_loop_spills.MODULES + WORKERS, cold=()).items():
        # A static's label reads as a function there too, with whatever data follows it
        if not any(line.startswith(".Lfunc_begin") for line in body[:2]):
            continue
        lines = normalise(body, data)
        digest = hashlib.sha256("\n".join(lines).encode()).hexdigest()[:16]
        found[hot_loop_spills.short_name(sym)].append((digest, lines))
    return {name: sorted(found[name]) for name in sorted(found)}


def compiler(path):
    """The compiler that built the asm, as `rustc --version` gives it: from the asm's debug info,
    else (an asm built without it) the toolchain here."""
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            if m := PRODUCER.search(line):
                return f"rustc {m.group(1)}"
    return subprocess.run(["rustc", "--version"], capture_output=True, text=True).stdout.strip()


def glob_asm():
    """Every asm file the build may have left, as hot_loop_spills.py looks for them."""
    return glob.glob("target/release/build/tmr*/**/tmr.s", recursive=True) + glob.glob("target/release/deps/tmr-*.s")


def main():
    args = sys.argv[1:]
    opt = {f: args[args.index(f) + 1] for f in ("--save", "--compare", "--dump") if f in args}
    files = [a for a in args if a.endswith(".s")]
    found = glob_asm()
    if not files and not found:
        sys.exit("no asm: build it with `cargo rustc --release --bin tmr -- --emit asm`")
    path = files[0] if files else max(found, key=os.path.getmtime)
    funcs = fingerprints(path)
    if not funcs:
        sys.exit(f"{path}: no hot-path function; check hot_loop_spills.py's MODULES against its symbols")
    table = {name: [d for d, _ in v] for name, v in funcs.items()}
    count = f"{sum(map(len, table.values()))} functions under {len(table)} names"
    if "--dump" in opt:
        os.makedirs(opt["--dump"], exist_ok=True)
        for name, v in funcs.items():
            for k, (_, lines) in enumerate(v):
                file = re.sub(r"[^\w.-]+", "_", name) + (f".{k}" if k else "") + ".s"
                with open(os.path.join(opt["--dump"], file), "w", encoding="utf-8") as f:
                    f.write("\n".join(lines) + "\n")
        print(f"{count} written to {opt['--dump']}")
    if "--save" in opt:
        rustc = compiler(path)
        # One function a line, so a baseline's diff in git reads as the functions that changed
        functions = ",\n".join(f"  {json.dumps(name)}: {json.dumps(hashes)}" for name, hashes in table.items())
        with open(opt["--save"], "w", encoding="utf-8", newline="\n") as f:
            f.write(f'{{\n "asm": {json.dumps(path.replace(os.sep, "/"))},\n "rustc": {json.dumps(rustc)},\n'
                    f' "functions": {{\n{functions}\n }}\n}}\n')
        print(f"{count} from {path} saved to {opt['--save']}")
    if "--compare" in opt:
        with open(opt["--compare"], encoding="utf-8") as f:
            base = json.load(f)
        changed = [n for n in table if n in base["functions"] and base["functions"][n] != table[n]]
        gone = [n for n in base["functions"] if n not in table]
        new = [n for n in table if n not in base["functions"]]
        for label, names in (("changed", changed), ("gone", gone), ("new", new)):
            for name in names:
                print(f"{label:8s} {name}")
        print(f"{count} against {len(base['functions'])} names in {opt['--compare']} ({base.get('rustc', '?')}): "
              f"{len(changed)} changed, {len(gone)} gone, {len(new)} new")
        rustc = compiler(path)
        other = rustc != base.get("rustc")
        if other:
            print(f"WARNING: this asm is {rustc}'s, the baseline {base.get('rustc', '?')}'s: a change may be "
                  "the compiler's; rebuild the baseline's code with this toolchain to compare")
        sys.exit((2 if other else 1) if changed or gone or new else 0)
    if not opt:
        for name, v in funcs.items():
            for digest, lines in v:
                print(f"{name[:70]:70s} {len(lines):6d} {digest}")
        print(f"{count}, from {path}")


if __name__ == "__main__":
    main()
