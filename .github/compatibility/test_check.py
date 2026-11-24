"""Exercise the release check against a disposable consumer-visible Rust interface."""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class CompatibilityCheck(unittest.TestCase):
    def test_drift_and_failed_generation_preserve_the_reviewed_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / ".github/compatibility"
            tools.mkdir(parents=True)
            script = tools / "check.py"
            shutil.copyfile(Path(__file__).with_name("check.py"), script)
            manifest = root / "Cargo.toml"
            manifest.write_text(
                '[package]\nname = "openadr"\nversion = "0.1.0"\nedition = "2024"\n'
                'rust-version = "1.96"\n[features]\ndefault = ["client"]\nclient = []\n'
            )
            (root / "src").mkdir()
            library = root / "src/lib.rs"
            library.write_text("pub fn original() {}\n")
            baseline = tools / "public-api.txt"
            env = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"))

            def run(*args):
                return subprocess.run(
                    [sys.executable, script, *args],
                    check=False,
                    cwd=root,
                    env=env,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                )

            # Locked generation requires a reviewed lockfile, even in this dependency-free fixture.
            subprocess.run(
                ["cargo", "generate-lockfile", "--offline"], cwd=root, env=env, check=True
            )
            first = run("--update")
            self.assertEqual(first.returncode, 0, first.stdout)
            reviewed = baseline.read_bytes()
            library.write_text("pub fn original() {}\n#[doc(hidden)] pub fn helper() {}\n")
            same = run()
            self.assertEqual(same.returncode, 0, same.stdout)

            library.write_text("pub fn renamed() {}\n")
            with manifest.open("a") as stream:
                stream.write('ven = ["client"]\n')
            drift = run()
            self.assertNotEqual(drift.returncode, 0)
            self.assertIn("-pub fn openadr::original()", drift.stdout)
            self.assertIn("+pub fn openadr::renamed()", drift.stdout)
            self.assertIn("+ven = [client]", drift.stdout)
            self.assertEqual(baseline.read_bytes(), reviewed)

            accepted = run("--update")
            self.assertEqual(accepted.returncode, 0, accepted.stdout)
            reviewed = baseline.read_bytes()
            self.assertIn(b"pub fn openadr::renamed()", reviewed)

            library.write_text("pub fn invalid(\n")
            failed = run("--update")
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(baseline.read_bytes(), reviewed)
