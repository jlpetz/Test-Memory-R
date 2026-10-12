"""Extract bounded store-loop candidates from a post-LTO assembly listing.

These candidates need manual inspection: nested-loop envelopes and unrelated
stores can also match. Counts include moves and branches, not CPU micro-ops.
"""

import hashlib
import json
from pathlib import Path
import re
import sys


def inspect(path):
    text = path.read_text(encoding="utf-8")
    starts = list(re.finditer(r"(?m)^\s*\.def\s+([^;]+);", text))
    loops = []
    functions = []
    for i, match in enumerate(starts):
        name = match[1].strip()
        if "simple_native_" not in name:
            continue
        stop = starts[i + 1].start() if i + 1 < len(starts) else len(text)
        body = text[match.start():stop]
        functions.append({
            "symbol": name,
            "sha256": hashlib.sha256(body.encode()).hexdigest(),
            "libcalls": re.findall(r"(?m)^\t(?:callq|jmp)\s+([^\n]*(?:memcpy|memset)[^\n]*)", body),
        })
        first = text[:match.start()].count("\n")
        lines = body.splitlines()
        labels = {}
        for j, line in enumerate(lines):
            label = re.match(r"(\.LBB\w+):", line)
            if label:
                labels[label[1]] = j
            jump = re.match(r"\s*j\w+\s+(\.LBB\w+)\s*$", line)
            if not jump or jump[1] not in labels:
                continue
            back = labels[jump[1]]
            if j - back > 140:
                continue
            block = lines[back:j + 1]
            stores = [
                s for s in block
                if re.search(r"\bvmov\w*\s+%(?:xmm|ymm|zmm)\d+,\s+[^%]*\(", s)
                and "%rsp" not in s and "%rbp" not in s
            ]
            if not stores:
                continue
            loops.append({
                "symbol": name,
                "line": first + back + 1,
                "instructions": sum(bool(re.match(r"\t[a-z]", s)) for s in block),
                "stores": stores,
                "body": "\n".join(block),
            })
    return {
        "source": path.as_posix(),
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "functions": functions,
        "loops": loops,
    }


if __name__ == "__main__":
    source, destination = map(Path, sys.argv[1:])
    result = inspect(source)
    destination.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    for loop in result["loops"]:
        if "simple_native_256s" in loop["symbol"]:
            print(loop["line"], loop["instructions"], loop["body"].splitlines()[0])
