"""Extract relevant compiled function summaries; review is not a global memcpy grep."""
from pathlib import Path
import re
import json

ROOT = Path(__file__).resolve().parents[2]
paths = list((ROOT / "target/release").rglob("*.s"))
production = max(paths, key=lambda p: p.stat().st_mtime)
probe = ROOT / "target/pattern-audit-probe/cpu_probe.s"
output = []

for source in [production, probe]:
    if not source.exists():
        continue
    asm = source.read_text(encoding="utf-8")
    functions = re.split(r"(?m)^\s*\.def\s+", asm)[1:]
    selected = []
    for fn in functions:
        name = fn.split(";", 1)[0]
        if source == probe:
            relevant = "fill_" in name or "verify_" in name
        else:
            relevant = any(x in name for x in [
                "tmr_fill_512", "tmr_check_512", "tmr_word", "SealKernel",
                "simple_test_v2_256", "simple_test_nt_256",
                "mirror_move_v2_256", "refresh_stable_256", "block_move",
            ])
        if not relevant:
            continue
        instructions = re.findall(r"(?m)^\t([a-z][a-z0-9]+)\s", fn)
        calls = [target for target in re.findall(r"(?m)^\t(?:callq|jmp)\s+([^\n]+)", fn)
                 if not target.startswith(".L")]
        offset = asm.find("\t.def\t" + name)
        selected.append({
            "symbol": name,
            "start_line": asm[:offset].count("\n") + 1,
            "registers": {w: len(re.findall(r"%" + w + r"\d+", fn)) for w in ["xmm", "ymm", "zmm"]},
            "counts": {ins: instructions.count(ins) for ins in [
                "vmovntdq", "vmovntdqa", "clflushopt", "sfence", "mfence",
                "vpmullq", "vpmuludq", "imulq", "vpsrlq", "vpxor", "vpxorq", "vpaddq",
            ]},
            "calls": calls,
        })
    output.append({"source": str(source.relative_to(ROOT)), "functions": selected})

destination = Path(__file__).with_name("codegen-results.json")
destination.write_text(json.dumps(output, indent=2) + "\n", encoding="utf-8")
for record in output:
    print(record["source"])
    for fn in record["functions"]:
        print(fn["symbol"], json.dumps(fn["counts"]), "calls", len(fn["calls"]))
