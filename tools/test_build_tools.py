"""Build provenance and archive checks; temporary ordinary files only, no compilation or installation."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
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
            text = workflow.read_text(encoding="utf-8")
            self.assertIn("./tools/build_environment.ps1 -InstallSdk -GitHub", text)
            self.assertIn("python-version: '>=3.11 <4'", text)
            self.assertLess(text.index("uses: actions/setup-python@"), text.index("./tools/build_environment.ps1"))
            self.assertLess(text.index("./tools/build_environment.ps1"), text.index("run: python "))

    def export_github_environment(self, selected):
        """Run the actual PowerShell exporter, with ordinary temporary files in place of GitHub's command files."""
        source = self.root / "environment.json"
        source.write_text(json.dumps(selected), encoding="utf-8")
        environment_file, path_file = self.root / "github_env", self.root / "github_path"
        for path in (environment_file, path_file):
            path.write_text("", encoding="utf-8")
        script = self.root / "export.ps1"
        script.write_text(
            "param($Helper, $InputFile, $EnvironmentFile, $PathFile)\n"
            "$ErrorActionPreference = 'Stop'\n"
            ". $Helper\n"
            "$inputData = Get-Content -LiteralPath $InputFile -Raw -Encoding UTF8 | ConvertFrom-Json\n"
            "$selected = [ordered]@{}\n"
            "foreach ($property in $inputData.PSObject.Properties) { $selected[$property.Name] = $property.Value }\n"
            "Export-GitHubBuildEnvironment -Environment $selected -EnvironmentFile $EnvironmentFile -PathFile $PathFile\n",
            encoding="utf-8",
        )
        result = subprocess.run(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script),
             str(build_release.ROOT / "tools/github_environment.ps1"), str(source), str(environment_file), str(path_file)],
            capture_output=True, text=True, encoding="utf-8",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return environment_file.read_text(encoding="utf-8"), path_file.read_text(encoding="utf-8").splitlines()

    @unittest.skipUnless(sys.platform == "win32", "Windows build environment")
    def test_github_path_keeps_python_and_msvc_ahead_of_other_executables(self):
        directories = [self.root / name for name in ("MSVC", "SDK", "Python", "WindowsApps", "Git")]
        for directory in directories:
            directory.mkdir()
        msvc, sdk, python, store, git = directories
        for directory, executable in ((msvc, "link.exe"), (sdk, "rc.exe"), (python, "python.exe"),
                                      (store, "python.exe"), (git, "link.exe")):
            (directory / executable).write_bytes(b"test-only placeholder; never executed")
        # Empty and duplicate entries can occur in the selected PATH. Earlier actions have already added Python and Cargo.
        selected_path = ";".join(map(str, [*directories, python])) + ";;"
        exported_env, additions = self.export_github_environment({"Path": selected_path, "LIB": "SDK libs;MSVC libs"})
        self.assertEqual(exported_env, "LIB=SDK libs;MSVC libs\n")
        prepend = [str(python), str(self.root / "Cargo")]
        # Independent replay of actions/runner v2.338.2: AddPathFileCommand removes an exact duplicate then appends;
        # Handler.AddPrependPathToEnvironment reverses that list before searching it.
        for entry in additions:
            prepend = [old for old in prepend if old != entry]
            prepend.append(entry)
        effective = list(reversed(prepend))
        self.assertEqual(effective[:len(directories)], list(map(str, directories)))
        path = os.pathsep.join(effective)
        for executable, directory in (("python.exe", python), ("link.exe", msvc), ("rc.exe", sdk)):
            self.assertEqual(shutil.which(executable, path=path), str(directory / executable))

    @unittest.skipUnless(sys.platform == "win32", "Windows build environment")
    def test_github_path_handles_empty_and_single_directory(self):
        for selected, expected in [("", []), (";;", []), (";C:\\one directory;;", ["C:\\one directory"])]:
            with self.subTest(selected=selected):
                exported_env, additions = self.export_github_environment({"Path": selected, "WindowsSDKVersion": "test"})
                self.assertEqual(additions, expected)
                self.assertEqual(exported_env, "WindowsSDKVersion=test\n")

    @unittest.skipUnless(os.environ.get("GITHUB_ACTIONS") == "true", "Checks the real environment after CI's SDK setup")
    def test_ci_resolves_the_selected_python_linker_and_resource_compiler(self):
        expected_python = Path(os.environ["pythonLocation"]) / "python.exe"
        self.assertEqual(Path(sys.executable).resolve(), expected_python.resolve())
        expected_tools = {
            "python.exe": expected_python,
            "link.exe": Path(os.environ["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"]),
            "rc.exe": Path(os.environ["WindowsSdkVerBinPath"]) / "x64" / "rc.exe",
        }
        for executable, expected in expected_tools.items():
            resolved = shutil.which(executable)
            self.assertIsNotNone(resolved, executable)
            self.assertEqual(Path(resolved).resolve(), expected.resolve(), executable)

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
