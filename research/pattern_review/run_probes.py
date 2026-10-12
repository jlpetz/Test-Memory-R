"""Run bounded independent review probes against a copy of the working tree.

No production Rust, index, OS allocations, or TMR executable is changed/run.
Only the added unit tests are executed, serially, using small aligned heap or
stack buffers. The output distinguishes defect reproductions from passing
contract checks; a passing reproduction means the defect exists.
"""

from pathlib import Path
import shutil
import subprocess


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
STAGING = ROOT / "target" / "pattern-review-source"


def main():
    STAGING.mkdir(parents=True, exist_ok=True)
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copy2(ROOT / name, STAGING / name)
    for name in ("src", "test_configs"):
        shutil.copytree(ROOT / name, STAGING / name, dirs_exist_ok=True)
    for source, probe in (
        ("results.rs", "results_probes.rs"),
        ("pattern.rs", "pattern_probes.rs"),
        ("tests.rs", "kernel_probes.rs"),
        ("runner.rs", "runner_probes.rs"),
    ):
        path = HERE / probe
        if not path.exists():
            continue
        with (STAGING / "src" / source).open("a", encoding="utf-8") as out:
            out.write(
                "\n#[cfg(test)]\n"
                f'#[path = "{path.as_posix()}"]\n'
                "mod native_review;\n"
            )
    command = [
        "cargo", "test", "--offline", "--manifest-path", str(STAGING / "Cargo.toml"),
        # A copied crate must not share the production crate's output filenames.
        "--target-dir", str(ROOT / "target" / "pattern-review-artifacts"), "--lib", "review_",
        "--", "--nocapture", "--test-threads=1",
    ]
    print("Review source copy:", STAGING, flush=True)
    result = subprocess.run(
        command, cwd=ROOT, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, encoding="utf-8", errors="replace"
    )
    (HERE / "probe-results.txt").write_text(result.stdout, encoding="utf-8")
    print(result.stdout)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
