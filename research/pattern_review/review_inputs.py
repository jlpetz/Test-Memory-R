"""Record the review's inputs, and summarise the freshly built release assembly.

Read-only with respect to production sources and Git. Output is confined to this
research directory. Run from the repository root with Python 3.
"""

from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def git(*args):
    return subprocess.check_output(
        ["git", *args], cwd=ROOT, text=True, encoding="utf-8"
    ).strip()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inputs():
    paths = list((ROOT / "src").rglob("*.rs"))
    paths += [
        ROOT / "Cargo.toml",
        ROOT / "Cargo.lock",
        ROOT / ".cargo/config.toml",
        ROOT / "doc/pattern_spec.md",
        ROOT / "doc/todo/97-native-patterns.md",
        ROOT / "doc/pattern_plan_2026-10-10.md",
    ]
    return {
        "captured_utc": datetime.now(timezone.utc).isoformat(),
        "head": git("rev-parse", "HEAD"),
        "git_status": git("status", "--short"),
        "index_diff": git("diff", "--cached", "--stat"),
        "files": {
            str(p.relative_to(ROOT)).replace("\\", "/"): digest(p)
            for p in sorted(paths)
        },
    }


def codegen():
    # The bin's post-LTO output, not the pre-LTO library listing.
    paths = list((ROOT / "target/release").rglob("tmr.s"))
    source = max(paths, key=lambda p: p.stat().st_mtime)
    asm = source.read_text(encoding="utf-8")
    starts = list(re.finditer(r"(?m)^\s*\.def\s+([^;]+);", asm))
    selected = []
    needles = [
        "simple_native_", "native_rescan", "Rescan6reread", "Rescan6finish",
        "SealKernel4fill", "SealKernel8check_as",
        "tmr_fill_", "tmr_check_", "tm5_fill_", "tm5_check_",
        "flush_range_to_dram", "simple_test_v2_multi",
        "simple_test_v2_128", "simple_test_v2_256", "simple_test_v2_512",
    ]
    for i, match in enumerate(starts):
        name = match.group(1).strip()
        if not any(n in name for n in needles):
            continue
        stop = starts[i + 1].start() if i + 1 < len(starts) else len(asm)
        body = asm[match.start():stop]
        instructions = re.findall(r"(?m)^\t([a-z][a-z0-9]+)\s", body)
        counts = Counter(instructions)
        calls = [
            c for c in re.findall(r"(?m)^\t(?:callq|jmp)\s+([^\n]+)", body)
            if not c.startswith(".L")
        ]
        selected.append({
            "symbol": name,
            "line": asm.count("\n", 0, match.start()) + 1,
            "lines": body.count("\n"),
            "register_references": {
                reg: len(re.findall(r"%" + reg + r"\d+", body))
                for reg in ["xmm", "ymm", "zmm"]
            },
            "instructions": dict(sorted(counts.items())),
            "calls": calls,
            # These are candidates for manual inspection, not an automatic
            # certification of each path's widths, loads or loop structure.
            "memory_vector_lines": [
                line.strip() for line in body.splitlines()
                if "(" in line and re.search(r"%(xmm|ymm|zmm)\d+", line)
                and not re.search(r"\(%r(?:sp|bp)\)", line)
            ][:100],
        })
    return {
        "source": str(source.relative_to(ROOT)).replace("\\", "/"),
        "sha256": digest(source),
        "built_utc": datetime.fromtimestamp(source.stat().st_mtime, timezone.utc).isoformat(),
        "functions": selected,
    }


if __name__ == "__main__":
    snapshot = HERE / "review-inputs.json"
    now = inputs()
    if snapshot.exists():
        old = json.loads(snapshot.read_text(encoding="utf-8"))
        changed = [
            p for p, h in now["files"].items()
            if old["files"].get(p) != h
        ]
        print("Production inputs changed since snapshot:", changed)
        if changed:
            raise SystemExit("Review inputs changed; do not silently refresh the baseline.")
    else:
        snapshot.write_text(json.dumps(now, indent=2) + "\n", encoding="utf-8")
        print("Recorded", len(now["files"]), "source/config/spec hashes at", now["head"])
    output = codegen()
    (HERE / "codegen-review.json").write_text(
        json.dumps(output, indent=2) + "\n", encoding="utf-8"
    )
    print("Assembly:", output["source"], output["built_utc"])
    for fn in output["functions"]:
        libcall = [c for c in fn["calls"] if "memset" in c or "memcpy" in c]
        print(
            fn["line"], fn["symbol"], fn["register_references"],
            "NT", fn["instructions"].get("vmovntdq", 0),
            "memcpy/memset candidates", libcall,
        )
