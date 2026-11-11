"""Run CI's steps here, with CI's own commands, so a failure shows before a push (the owner, 2026-11-03).

The steps are read from the workflow file, not copied: the single-line Cargo commands, build_release.py
and build-tool unit checks run in the file's order. Other steps (the
checkout, the toolchain, the cache, the tool installs, release.yml's PowerShell blocks) are listed
as skipped. Like CI, it stops at the first step that fails (`--keep-going` runs the rest anyway).

    python tools/ci_local.py              # Rust-nightly-build.yml: fmt, clippy x3, release build, nextest, audit
    python tools/ci_local.py --release    # release.yml's: --locked, the publish dry run
    python tools/ci_local.py --list       # the steps, without running them
    python tools/ci_local.py --quick      # a change's check: fmt, clippy x3 and nextest only (about a minute)

A full run that passes keeps its EXE, PDB and manifest in ../builds/<commit>[-dirty]-<exe-hash>/,
so a live check or deploy takes it from there and a new compiler cannot overwrite the old build.

Needs what CI installs: the nightly with rustfmt and clippy, cargo-nextest and cargo-audit
(`cargo install --locked cargo-nextest cargo-audit`; cargo-about too for release.yml's notices, which
this doesn't run). Exits 0 when every step passed.
"""
import argparse
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
from build_release import archive_build, native_environment

REPO = Path(__file__).resolve().parent.parent
WORKFLOWS = {False: REPO / ".github/workflows/Rust-nightly-build.yml", True: REPO / ".github/workflows/release.yml"}
BUILDS = REPO.parent / "builds"
# A change's check (--quick): what catches a code change, without the release build and the audit
QUICK = (" fmt ", " clippy ", " nextest ", " unittest ")


def keep_build():
    """Archive the exact EXE/PDB pair produced by the recorded release build."""
    return archive_build(REPO / "target/release", BUILDS)


def steps(path):
    """The first job's Cargo, shared release-builder and build-tool check commands, in order."""
    out, name = [], None
    for line in path.read_text(encoding="utf-8").splitlines():
        if m := re.match(r"^\s*- name:\s*(.+?)\s*$", line):
            if name is not None:
                out.append((name, None))
            name = m.group(1)
        elif name is not None and (m := re.match(r"^\s+run:\s*(.+?)\s*$", line)):
            command = m.group(1)
            supported = command.startswith(("cargo ", "python tools/build_release.py", "python -B -m unittest "))
            out.append((name, command if supported else None))
            name = None
        elif re.match(r"^  [A-Za-z_-]+:\s*$", line) and out:
            # The next job (release.yml's crates-io, winget): its steps run only on a tag, not here
            if name is not None:
                out.append((name, None))
            break
    else:
        if name is not None:
            out.append((name, None))
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--release", action="store_true", help="release.yml's build job instead of CI's")
    parser.add_argument("--list", action="store_true", help="list the steps without running them")
    parser.add_argument("--keep-going", action="store_true", help="run every step, even after one fails")
    parser.add_argument("--quick", action="store_true", help="a change's check: fmt, clippy and nextest only")
    args = parser.parse_args()
    sys.stdout.reconfigure(encoding="utf-8")

    workflow = WORKFLOWS[args.release]
    plan = steps(workflow)
    if args.quick:
        plan = [(n, c if c and any(k in f" {c} " for k in QUICK) else None) for n, c in plan]
    print(f"{workflow.relative_to(REPO)}: {sum(1 for _, c in plan if c)} steps to run, {sum(1 for _, c in plan if not c)} skipped")
    if args.list:
        for name, command in plan:
            print(f"  {'run ' if command else 'skip'}  {name}" + (f"\n          {command}" if command else ""))
        return 0

    env, _ = native_environment()
    env["CARGO_INCREMENTAL"] = "0"  # dtolnay/rust-toolchain does this in CI, including Clippy and tests.
    results = []
    for name, command in plan:
        if command is None:
            continue
        print(f"\n=== {name}\n$ {command}", flush=True)
        start = time.monotonic()
        code = subprocess.run(shlex.split(command), cwd=REPO, env=env).returncode
        results.append((name, code, time.monotonic() - start))
        if code != 0 and not args.keep_going:
            break

    print("\n=== Summary")
    for name, code, secs in results:
        print(f"  {'pass' if code == 0 else 'FAIL'}  {secs:7.1f}s  {name}")
    ran = len(results)
    total = sum(1 for _, c in plan if c)
    if ran < total:
        print(f"  ({total - ran} not run after the failure)")
    failed = [r for r in results if r[1] != 0]
    if failed or ran < total:
        return 1
    if not args.quick and (kept := keep_build()):
        print(f"  kept: {kept}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
