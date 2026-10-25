"""Prepare an isolated full-source candidate; never edit the product sources."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
COPY = ROOT / "target" / "switch24-candidate"


def main(review_probes=False):
    COPY.mkdir(parents=True, exist_ok=True)
    inputs = {}
    for directory in ("src", "testdata"):
        for path in (ROOT / directory).rglob("*"):
            if path.is_file():
                inputs[path.relative_to(ROOT).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
        shutil.copytree(ROOT / directory, COPY / directory, dirs_exist_ok=True)
    for name in ("Cargo.toml", "Cargo.lock"):
        inputs[name] = hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
        shutil.copy2(ROOT / name, COPY / name)
    inputs["git_head"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    inputs["git_index"] = subprocess.check_output(["git", "diff", "--cached", "--name-only"], cwd=ROOT, text=True)
    saved_inputs = HERE / "inputs.json"
    expected = HERE / "expected-output-hashes.json"
    previous = json.loads(saved_inputs.read_text()) if saved_inputs.exists() else None
    outputs = json.loads(expected.read_text()) if expected.exists() else {}
    for name in ("pattern.rs", "pattern_gen.rs", "tests.rs"):
        baseline = HERE / (name + ".baseline")
        relative = f"src/{name}"
        if previous and baseline.exists():
            assert hashlib.sha256(baseline.read_bytes()).hexdigest() == previous[relative]
            assert inputs[relative] in (previous[relative], outputs.get(relative)), \
                f"{relative} has changed since this experiment; preserve these results and start a new review."
            # The optimization may already be installed in the working tree.
            # Reconstruct the candidate from this experiment's original inputs.
            shutil.copy2(baseline, COPY / relative)
        else:
            shutil.copy2(ROOT / relative, baseline)
    if previous is None:
        saved_inputs.write_text(json.dumps(inputs, indent=2) + "\n")
    shutil.copy2(HERE / "candidate_switch24.rs", COPY / "src" / "pattern_switch24.rs")
    gen = COPY / "src" / "pattern_gen.rs"
    text = gen.read_text(encoding="utf-8")
    marker = "// ─── Tests ───────────────────────────────────────────────────────────────────"
    assert text.count(marker) == 1
    text = text.replace(marker, '#[path = "pattern_switch24.rs"]\npub(crate) mod switch24;\n\n' + marker)
    gen.write_text(text, encoding="utf-8", newline="\r\n")
    pattern = COPY / "src" / "pattern.rs"
    text = pattern.read_text(encoding="utf-8")
    old = """            (Family::Switch, BusBeat::Bits24) => {
                let $line = |lid: u64| $crate::pattern::switch_line_v::<$n, V>(lid, key, BusBeat::Bits24).map(|x| x ^ mask);
                $body
            }"""
    new = """            (Family::Switch, BusBeat::Bits24) => {
                let $line = |lid: u64| {
                    let hash = $crate::pattern::fmix(lid.wrapping_add(key));
                    $crate::pattern_gen::switch24::vectors!($n, hash).map(|x| x ^ mask)
                };
                $body
            }"""
    assert text.count(old) == 1
    text = text.replace(old, new)
    # Forward the literal SIMD lane count into the width-specific encoder macro.
    text = text.replace("($n:literal, $phase:expr, |$line:ident| $body:block)",
                        "($n:tt, $phase:expr, |$line:ident| $body:block)")
    pattern.write_text(text, encoding="utf-8", newline="\r\n")
    tests = COPY / "src" / "tests.rs"
    text = tests.read_text(encoding="utf-8")
    old = "([$simd:ty, $w:literal], $ptr:expr, $start:expr, $end:expr, $lid0:expr, $period:expr, $phase:expr)"
    assert text.count(old) == 2
    text = text.replace(old, old.replace("$w:literal", "$w:tt"))
    ending = "\r\n" if b"\r\n" in (HERE / "tests.rs.baseline").read_bytes() else "\n"
    tests.write_text(text, encoding="utf-8", newline=ending)
    if review_probes:
        for source, probe in (
            ("results.rs", "results_probes.rs"),
            ("pattern.rs", "pattern_probes.rs"),
            ("tests.rs", "kernel_probes.rs"),
            ("runner.rs", "runner_probes.rs"),
        ):
            suffix = (
                '\r\n#[cfg(test)]\r\n#[path = "'
                + (HERE.parent / "followup" / probe).as_posix()
                + '"]\r\nmod native_review;\r\n'
            )
            with (COPY / "src" / source).open("ab") as out:
                out.write(suffix.encode())
    print(COPY)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--review-probes", action="store_true")
    main(parser.parse_args().review_probes)
