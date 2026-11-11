"""Build provenance and archive checks; temporary ordinary files only, no compilation or installation."""
import json
from pathlib import Path
import tempfile
import unittest

import build_release
import ci_local


class BuildToolsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="tmr-build-tools-test-")
        self.root = Path(self.tmp.name).resolve()
        self.addCleanup(self.tmp.cleanup)
        self.build = self.root / "release"
        self.archive = self.root / "builds"
        self.build.mkdir()
        self.make_build(b"first executable", b"first pdb")

    def make_build(self, exe, pdb):
        for name, content in (("tmr.exe", exe), ("tmr.pdb", pdb)):
            (self.build / name).write_bytes(content)
        manifest = {
            "source": {"commit": "a" * 40, "status": ""},
            "artifacts": {name: {"sha256": build_release.sha256(self.build / name)}
                          for name in ("tmr.exe", "tmr.pdb")},
        }
        (self.build / build_release.MANIFEST_NAME).write_text(json.dumps(manifest), encoding="utf-8")
        return manifest

    def test_new_executable_at_same_commit_preserves_old_archive(self):
        first = build_release.archive_build(self.build, self.archive)
        self.make_build(b"another compiler's executable", b"another compiler's pdb")
        second = build_release.archive_build(self.build, self.archive)
        self.assertNotEqual(first, second)
        self.assertEqual((first / "tmr.exe").read_bytes(), b"first executable")
        self.assertEqual((second / "tmr.pdb").read_bytes(), b"another compiler's pdb")

    def test_repeated_archive_preserves_original_manifest(self):
        first = build_release.archive_build(self.build, self.archive)
        original = (first / build_release.MANIFEST_NAME).read_bytes()
        manifest = json.loads(original)
        manifest["recorded_at_utc"] = "a later run"
        (self.build / build_release.MANIFEST_NAME).write_text(json.dumps(manifest), encoding="utf-8")
        self.assertEqual(build_release.archive_build(self.build, self.archive), first)
        self.assertEqual((first / build_release.MANIFEST_NAME).read_bytes(), original)

    def test_stale_manifest_rejects_changed_exe_and_changed_pdb(self):
        for name in ("tmr.exe", "tmr.pdb"):
            with self.subTest(name=name):
                self.make_build(b"first executable", b"first pdb")
                (self.build / name).write_bytes(b"changed after the recorded build")
                with self.assertRaisesRegex(RuntimeError, "no longer matches"):
                    build_release.archive_build(self.build, self.archive)
        self.assertFalse(self.archive.exists())

    def test_corrupt_archive_is_not_overwritten(self):
        destination = build_release.archive_build(self.build, self.archive)
        (destination / "tmr.exe").write_bytes(b"unexpected content")
        with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
            build_release.archive_build(self.build, self.archive)
        self.assertEqual((destination / "tmr.exe").read_bytes(), b"unexpected content")

    def test_missing_or_extra_artifact_is_refused(self):
        manifest = self.make_build(b"exe", b"pdb")
        manifest["artifacts"]["../outside"] = {"sha256": "0" * 64}
        with self.assertRaisesRegex(RuntimeError, "exactly"):
            build_release.verify_artifacts(self.build, manifest)
        del manifest["artifacts"]["../outside"]
        del manifest["artifacts"]["tmr.pdb"]
        with self.assertRaisesRegex(RuntimeError, "exactly"):
            build_release.verify_artifacts(self.build, manifest)

    def test_commit_cannot_escape_archive_directory(self):
        manifest = self.make_build(b"exe", b"pdb")
        manifest["source"]["commit"] = "../outside"
        (self.build / build_release.MANIFEST_NAME).write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "Invalid commit"):
            build_release.archive_build(self.build, self.archive)

    def test_both_workflows_use_shared_builder_and_check_tools(self):
        for release, workflow in ci_local.WORKFLOWS.items():
            commands = [command for _, command in ci_local.steps(workflow) if command]
            expected = "python tools/build_release.py" + (" --locked" if release else "")
            self.assertEqual(commands.count(expected), 1)
            self.assertIn("python -B -m unittest discover -s tools -p test_build_tools.py", commands)
            self.assertFalse(any("no-verify" in command for command in commands))
            self.assertFalse(any(command.startswith("cargo +nightly build ") for command in commands))
            self.assertIn("./tools/build_environment.ps1 -InstallSdk -GitHub", workflow.read_text(encoding="utf-8"))

    def test_interrupted_archive_can_finish_without_replacing_exe(self):
        destination = build_release.archive_build(self.build, self.archive)
        exe_time = (destination / "tmr.exe").stat().st_mtime_ns
        (destination / "tmr.pdb").unlink()
        (destination / build_release.MANIFEST_NAME).unlink()
        self.assertEqual(build_release.archive_build(self.build, self.archive), destination)
        self.assertEqual((destination / "tmr.exe").stat().st_mtime_ns, exe_time)
        self.assertEqual((destination / "tmr.pdb").read_bytes(), b"first pdb")


if __name__ == "__main__":
    unittest.main()
