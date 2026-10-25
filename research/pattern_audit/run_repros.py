"""Run bounded audit probes against a source copy; never edit production src/.

Usage (from the repository root): python research/pattern_audit/run_repros.py
Only ordinary aligned heap allocations are used by the added probes.
The copied crate and Cargo build products live under target/.
"""

from pathlib import Path
import shutil
import subprocess


def main():
    audit = Path(__file__).resolve().parent
    repo = audit.parents[1]
    staging = repo / "target" / "pattern-audit-source"
    staging.mkdir(parents=True, exist_ok=True)
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copy2(repo / name, staging / name)
    for name in ("src", "testdata"):
        shutil.copytree(repo / name, staging / name, dirs_exist_ok=True)
    for source, extra in (("runner.rs", "runner_repros.rs"), ("tests.rs", "kernel_repros.rs")):
        dest = staging / "src" / source
        with dest.open("a", encoding="utf-8") as out:
            out.write(
                "\n#[cfg(test)]\n"
                f'#[path = "{(audit / extra).as_posix()}"]\n'
                "mod audit_repros;\n"
            )
    command = [
        "cargo", "test", "--offline", "--manifest-path", str(staging / "Cargo.toml"),
        "--target-dir", str(repo / "target"), "--lib", "audit_",
        "--", "--nocapture", "--test-threads=1",
    ]
    print("Probes use a source copy at", staging, flush=True)
    result = subprocess.run(command, cwd=repo, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, encoding="utf-8", errors="replace")
    (audit / "repro-results.txt").write_text(result.stdout, encoding="utf-8")
    print(result.stdout)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
