import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
FIXTURES = TOOLS / "testdata" / "winget"
STDOUT = (FIXTURES / "stdout.txt").read_bytes()


class WingetManifestTests(unittest.TestCase):
    def run_manifest(self, source, version="0.3.5"):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "stdout").write_bytes(source)
            result = subprocess.run(
                [sys.executable, str(TOOLS / "winget_manifest.py"), str(root / "stdout"), str(root / "out"),
                 "--version", version],
                capture_output=True,
            )
            files = {p.name: p.read_bytes() for p in (root / "out").glob("*")}
            return result, files

    def test_real_manifests_match_accepted_bytes(self):
        result, files = self.run_manifest(STDOUT)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(files["jcardama.LeopardWM.installer.yaml"], (FIXTURES / "corrected.installer.yaml").read_bytes())
        documents = STDOUT.split(b"\n# Created with ")
        self.assertEqual(files["jcardama.LeopardWM.locale.en-US.yaml"], b"# Created with " + documents[2])
        self.assertEqual(files["jcardama.LeopardWM.yaml"], b"# Created with " + documents[3][:-1])

    def test_different_display_version_is_preserved(self):
        source = STDOUT.replace(b"- DisplayVersion: 0.3.5\r\n", b"- DisplayVersion: '1.2.3'\r\n", 1)
        source = source.replace(b"- DisplayVersion: 0.3.5\r\n", b'- DisplayVersion: "0.3.5"\r\n')
        result, files = self.run_manifest(source)
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = files["jcardama.LeopardWM.installer.yaml"]
        self.assertEqual(manifest.count(b"- DisplayVersion: '1.2.3'\r\n"), 1)
        self.assertNotIn(b'DisplayVersion: "0.3.5"', manifest)

    def test_existing_root_layout_stays_in_place(self):
        corrected = (FIXTURES / "corrected.installer.yaml").read_bytes()
        start = STDOUT.index(b"# Created with ")
        end = STDOUT.index(b"\n# Created with ", start)
        source = STDOUT[:start] + corrected + STDOUT[end:]
        result, files = self.run_manifest(source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(files["jcardama.LeopardWM.installer.yaml"], corrected)

    def test_expected_installer_architectures_are_required(self):
        arm_start = STDOUT.index(b"- Architecture: arm64\r\n")
        arm_end = STDOUT.index(b"ManifestType: installer\r\n", arm_start)
        arm_entry = STDOUT[arm_start:arm_end]
        cases = {
            "relabeled": STDOUT.replace(b"- Architecture: arm64\r\n", b"- Architecture: x86\r\n"),
            "third installer": STDOUT[:arm_end] + arm_entry + STDOUT[arm_end:],
            "missing arm64": STDOUT[:arm_start] + STDOUT[arm_end:],
        }
        for name, source in cases.items():
            with self.subTest(name=name):
                result, files = self.run_manifest(source)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(b"expected one x64 and one arm64 installer", result.stderr)
                self.assertEqual(files, {})

    def test_installer_urls_match_release_and_architecture(self):
        x64 = b"LeopardWM-0.3.5-x86_64.msi"
        arm64 = b"LeopardWM-0.3.5-arm64.msi"
        cases = {
            "swapped": STDOUT.replace(x64, b"SWAP.msi").replace(arm64, x64).replace(b"SWAP.msi", arm64),
            "wrong version": STDOUT.replace(b"/v0.3.5/LeopardWM-0.3.5-arm64.msi",
                                            b"/v0.3.4/LeopardWM-0.3.4-arm64.msi"),
        }
        for name, source in cases.items():
            with self.subTest(name=name):
                result, files = self.run_manifest(source)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(b"installer URL does not match release and architecture", result.stderr)
                self.assertEqual(files, {})

    def test_package_version_matches_requested_release(self):
        result, files = self.run_manifest(STDOUT, version="0.3.4")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(b"manifest version does not match release version", result.stderr)
        self.assertEqual(files, {})

    def test_unknown_installer_shape_fails(self):
        source = STDOUT.replace(b"- Architecture: x64\r\n", b"- Architecture: x86\r\n")
        result, files = self.run_manifest(source)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"expected one x64", result.stderr)
        self.assertEqual(files, {})


if __name__ == "__main__":
    unittest.main()
