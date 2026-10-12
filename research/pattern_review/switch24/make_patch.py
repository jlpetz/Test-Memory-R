"""Export the verified candidate, excluding temporary review-only test modules."""

import difflib
import hashlib
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
COPY = ROOT / "target" / "switch24-candidate"


def main():
    inputs = json.loads((HERE / "inputs.json").read_text())
    patch = []
    outputs = {}
    for name, probe in (
        ("pattern_gen.rs", None),
        ("pattern.rs", "pattern_probes.rs"),
        ("tests.rs", "kernel_probes.rs"),
    ):
        relative = f"src/{name}"
        before = (HERE / (name + ".baseline")).read_bytes()
        assert hashlib.sha256(before).hexdigest() == inputs[relative], f"Baseline changed: {relative}"
        after = (COPY / relative).read_bytes()
        if probe:
            suffix = (
                '\r\n#[cfg(test)]\r\n#[path = "'
                + (HERE.parent / "followup" / probe).as_posix()
                + '"]\r\nmod native_review;\r\n'
            ).encode()
            assert after.endswith(suffix), relative
            after = after[:-len(suffix)]
        # The repository intentionally contains both LF and CRLF source files.
        # Preserve each file's original convention rather than making a full-file diff.
        eol = "\r\n" if b"\r\n" in before else "\n"
        after = after.decode().replace("\r\n", "\n").replace("\n", eol).encode()
        current = (ROOT / relative).read_bytes()
        assert current in (before, after), f"Source changed: {relative}"
        patch.extend(difflib.unified_diff(
            before.decode().splitlines(keepends=True),
            after.decode().splitlines(keepends=True),
            fromfile=f"a/{relative}", tofile=f"b/{relative}",
        ))
        outputs[relative] = hashlib.sha256(after).hexdigest()
    relative = "src/pattern_switch24.rs"
    after = (HERE / "candidate_switch24.rs").read_bytes()
    stored = after
    if (ROOT / relative).exists():
        stored = (ROOT / relative).read_bytes()
        assert stored.replace(b"\r\n", b"\n") == after.replace(b"\r\n", b"\n"), \
            "Product encoder differs from the tested candidate"
    patch.extend(difflib.unified_diff(
        [], after.decode().splitlines(keepends=True),
        fromfile="/dev/null", tofile=f"b/{relative}",
    ))
    outputs[relative] = hashlib.sha256(stored).hexdigest()
    (HERE / "switch24-simd.patch").write_bytes("".join(patch).encode())
    (HERE / "expected-output-hashes.json").write_text(json.dumps(outputs, indent=2) + "\n")
    print("Exported the four-file patch, preserving each original line-ending convention.")


if __name__ == "__main__":
    main()
