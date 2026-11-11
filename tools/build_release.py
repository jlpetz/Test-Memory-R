"""Build the release EXE with a recorded compiler/SDK environment, locally and in GitHub Actions.

    python tools/build_release.py --locked
    python tools/build_release.py --describe   # inspect the environment, without building TMR

No toolchain update, SDK install or TMR execution. build_environment.ps1 -InstallSdk provisions the
SDK explicitly. Nightly stays rolling; its exact version is recorded. A manifest belongs to the EXE
and PDB hashes it names, not every rebuild of the same commit.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
MANIFEST_NAME = "tmr.build.json"


def output(args, env=None):
    return subprocess.run(args, cwd=ROOT, env=env, check=True, capture_output=True, text=True, encoding="utf-8").stdout.strip()


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def native_environment():
    with tempfile.TemporaryDirectory(prefix="tmr-build-environment-") as tmp:
        path = Path(tmp) / "environment.json"
        subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                        "-File", str(ROOT / "tools/build_environment.ps1"), "-Output", str(path)], check=True, cwd=ROOT)
        selected = json.loads(path.read_text(encoding="utf-8"))
    # Windows environment keys are case-insensitive; avoid both PATH and Path in the child block.
    env = {key.upper(): value for key, value in os.environ.items()}
    env.update({key.upper(): value for key, value in selected["environment"].items()})
    # Changing an SDK or its libraries must relink even if Cargo considers the Rust inputs fresh.
    env["TMR_NATIVE_BUILD_HASH"] = hashlib.sha256(json.dumps(selected["native"], sort_keys=True).encode()).hexdigest()
    return env, selected["native"]


def source_state():
    """Hash the checkout, including untracked build inputs; exclude Git-ignored outputs and local files."""
    paths = output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"]).split("\0")
    digest = hashlib.sha256()
    for name in sorted(set(paths)):
        if not name:
            continue
        path = ROOT / name
        digest.update(name.encode() + b"\0")
        digest.update(bytes.fromhex(sha256(path)) if path.is_file() else b"deleted")
    return {
        "commit": output(["git", "rev-parse", "HEAD"]),
        "status": output(["git", "--no-optional-locks", "status", "--porcelain"]),
        "checkout_sha256": digest.hexdigest(),
        "cargo_lock_sha256": sha256(ROOT / "Cargo.lock"),
    }


def cargo_config(env):
    """Ask Cargo for inherited build configuration too; never read registry credentials into the record."""
    result = {}
    for key in ("build", "target", "profile"):
        args = ["cargo", "+nightly", "-Z", "unstable-options", "config", "get", key, "--format=json"]
        command = subprocess.run(args, cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8")
        if command.returncode == 0:
            result.update(json.loads(command.stdout))
        elif f"config value `{key}` is not set" not in command.stderr:
            raise RuntimeError(f"Could not read Cargo's {key} configuration: {command.stderr.strip()}")
    return result


def compiler_command(env, config):
    override = env.get("RUSTC") or env.get("CARGO_BUILD_RUSTC") or config.get("build", {}).get("rustc")
    return [override, "-Vv"] if override else ["rustc", "+nightly", "-Vv"]


def describe(env, native, command):
    config = cargo_config(env)
    rustc_command = compiler_command(env, config)
    rustc = output(rustc_command, env)
    if "host: x86_64-pc-windows-msvc" not in rustc:
        raise RuntimeError("The release builder requires the x86_64-pc-windows-msvc nightly.")
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    # These change the build, unlike arbitrary process variables which can contain credentials.
    names = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
             "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_INCREMENTAL",
             "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
             "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS", "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
             "TMR_NATIVE_BUILD_HASH"}
    overrides = {key: value for key, value in env.items() if key in names or key.startswith("CARGO_PROFILE_")}
    if env.get("CARGO_BUILD_TARGET") or config.get("build", {}).get("target"):
        raise RuntimeError("Use the native Windows target without CARGO_BUILD_TARGET/build.target for the release artifact layout.")
    if env.get("CARGO_TARGET_DIR") or config.get("build", {}).get("target-dir"):
        raise RuntimeError("The release workflow requires the repository's target/release output directory.")
    return {
        "schema_version": 1,
        "source": source_state(),
        "rustc": rustc,
        "rustc_version_command": rustc_command,
        "cargo": output(["cargo", "+nightly", "-Vv"], env),
        "target": "x86_64-pc-windows-msvc",
        "command": command,
        "cargo_config": config,
        "release_profile": manifest["profile"]["release"],
        "app_default_features": tomllib.loads((ROOT / "crates/tmr-app/Cargo.toml").read_text(encoding="utf-8"))["features"]["default"],
        "environment_overrides": overrides,
        "native": native,
        "host": {"platform": sys.platform, "windows_version": str(sys.getwindowsversion())},
        "ci": {key: env[key] for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "IMAGEOS", "IMAGEVERSION") if key in env},
    }


def verify_artifacts(directory, manifest):
    if set(manifest["artifacts"]) != {"tmr.exe", "tmr.pdb"}:
        raise RuntimeError("A release manifest must name exactly tmr.exe and tmr.pdb.")
    for name, record in manifest["artifacts"].items():
        if Path(name).name != name:
            raise RuntimeError(f"Invalid artifact name: {name}")
        if sha256(Path(directory) / name) != record["sha256"]:
            raise RuntimeError(f"{name} no longer matches its build manifest; rebuild before archiving.")


def archive_build(directory, archive):
    directory, archive = Path(directory), Path(archive)
    manifest = json.loads((directory / MANIFEST_NAME).read_text(encoding="utf-8"))
    verify_artifacts(directory, manifest)
    source = manifest["source"]
    commit = source["commit"]
    if len(commit) != 40 or any(char not in "0123456789abcdef" for char in commit):
        raise RuntimeError("Invalid commit in build manifest.")
    suffix = "-dirty" if source["status"] else ""
    destination = archive / f"{commit[:12]}{suffix}-{manifest['artifacts']['tmr.exe']['sha256'][:16]}"
    destination.mkdir(parents=True, exist_ok=True)
    for name, record in manifest["artifacts"].items():
        target = destination / name
        if target.exists():
            if sha256(target) != record["sha256"]:
                raise RuntimeError(f"Refusing to overwrite a different archived file: {target}")
        else:
            shutil.copy2(directory / name, target)
    verify_artifacts(destination, manifest)
    # A rerun of the same artifact preserves the first record; a partial copy can be completed.
    saved = destination / MANIFEST_NAME
    if saved.exists():
        previous = json.loads(saved.read_text(encoding="utf-8"))
        verify_artifacts(destination, previous)
    else:
        shutil.copy2(directory / MANIFEST_NAME, saved)
    return destination


def build(locked=False, describe_only=False):
    env, native = native_environment()
    command = ["cargo", "+nightly", "build", "--release"] + (["--locked"] if locked else [])
    manifest = describe(env, native, command)
    if describe_only:
        print(json.dumps(manifest, indent=2))
        return
    destination = ROOT / "target/release" / MANIFEST_NAME
    destination.unlink(missing_ok=True)  # A failed build must not leave an old manifest claiming success.
    subprocess.run(command, cwd=ROOT, env=env, check=True)
    if source_state() != manifest["source"]:
        raise RuntimeError("The checkout changed during compilation. No manifest was saved; build the settled inputs again.")
    if output(manifest["rustc_version_command"], env) != manifest["rustc"]:
        raise RuntimeError("Rust changed during compilation. No manifest was saved.")
    _, native_after = native_environment()
    if native_after != native:
        raise RuntimeError("Native build tools changed during compilation. No manifest was saved.")
    manifest["artifacts"] = {
        name: {"sha256": sha256(destination.parent / name), "bytes": (destination.parent / name).stat().st_size}
        for name in ("tmr.exe", "tmr.pdb")
    }
    manifest["recorded_at_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    destination.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Build manifest: {destination}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--locked", action="store_true")
    parser.add_argument("--describe", action="store_true")
    args = parser.parse_args()
    try:
        build(args.locked, args.describe)
    except (RuntimeError, subprocess.CalledProcessError, OSError) as error:
        sys.exit(str(error))
