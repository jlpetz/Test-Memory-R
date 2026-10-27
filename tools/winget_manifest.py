"""Petz.TMR's winget manifest for a released version (TODO 104), from the reviewed one in dist/winget/ (0.1.0's): the version, the
release's URLs, its installer's SHA-256 (the release's SHA256SUMS.txt) and its date (the release's), into
target/winget/manifests/p/Petz/TMR/<version>/, then `winget validate` on it.

For Petz.TMR's first version in winget-pkgs, which is submitted by hand. Later versions are release.yml's (Komac's `update`, from the
version before), so this is used once:

    python tools/winget_manifest.py 0.1.1
"""
import json
import pathlib
import re
import subprocess
import sys
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
TEMPLATE = ROOT / "dist" / "winget"
BASE = "0.1.0"  # the version dist/winget/ holds
REPO = "jlpetz/Test-Memory-R"


def fetch(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "tmr-winget-manifest"})) as response:
        return response.read().decode("utf-8")


def main():
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        sys.exit("usage: python tools/winget_manifest.py <version>, a release's, x.y.z (no pre-release: they stay out of winget)")
    version = sys.argv[1]
    tag = f"v{version}"
    try:
        release = json.loads(fetch(f"https://api.github.com/repos/{REPO}/releases/tags/{tag}"))
    except urllib.error.HTTPError as e:
        sys.exit(f"no GitHub release {tag} ({e}): tag it, and let release.yml publish it, first")
    sums = fetch(f"https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS.txt")
    setup = f"tmr-{version}-setup.exe"
    sha = next((line.split()[0] for line in sums.splitlines() if line.endswith(f"  {setup}")), None)
    if not sha:
        sys.exit(f"{tag}'s SHA256SUMS.txt names no {setup}")
    base_sha = re.search(r"InstallerSha256: (\w+)", (TEMPLATE / "Petz.TMR.installer.yaml").read_text(encoding="utf-8")).group(1)

    out = ROOT / "target" / "winget" / "manifests" / "p" / "Petz" / "TMR" / version
    out.mkdir(parents=True, exist_ok=True)
    for file in sorted(TEMPLATE.glob("Petz.TMR*.yaml")):
        text = file.read_text(encoding="utf-8")
        # Every version the template names is BASE's: PackageVersion, the release's URLs (download/v0.1.0, blob/v0.1.0, tag/v0.1.0) and
        # the Apps & Features name
        text = text.replace(base_sha, sha.upper()).replace(BASE, version)
        text = re.sub(r"ReleaseDate: \S+", f"ReleaseDate: {release['published_at'][:10]}", text)
        (out / file.name).write_text(text, encoding="utf-8", newline="\n")
        print(out / file.name)
    subprocess.run(["winget", "validate", "--manifest", str(out), "--disable-interactivity"], check=True)


if __name__ == "__main__":
    main()
