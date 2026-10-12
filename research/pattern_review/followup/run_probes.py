"""Run follow-up checks against a source copy, with its own build directory.

Production files, Git's index, large-page allocation and tmr.exe are untouched.
The first review's source delta and original probe results are preserved.
"""

from pathlib import Path
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
STAGING = ROOT / "target" / "pattern-review-source"


def main():
    sys.stdout.reconfigure(encoding="utf-8")
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
        with (STAGING / "src" / source).open("a", encoding="utf-8") as out:
            out.write(
                "\n#[cfg(test)]\n"
                f'#[path = "{(HERE / probe).as_posix()}"]\n'
                "mod native_review;\n"
            )
    command = [
        "cargo", "test", "--offline", "--manifest-path", str(STAGING / "Cargo.toml"),
        "--target-dir", str(ROOT / "target" / "pattern-review-artifacts"),
        "--lib", "review_", "--", "--nocapture", "--test-threads=1",
    ]
    result = subprocess.run(
        command, cwd=ROOT, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, encoding="utf-8", errors="replace",
    )
    (HERE / "probe-results.txt").write_text(result.stdout, encoding="utf-8")
    print(result.stdout)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
