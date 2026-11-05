import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class HookOutputTests(unittest.TestCase):
    def test_sleep_gate_accepts_an_empty_diff(self) -> None:
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/check-test-sleeps.sh"), "HEAD", "HEAD"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")

    def test_success_is_silent_and_failure_preserves_output_and_status(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            pre_commit = directory / "pre-commit"
            pre_commit.write_text(
                "#!/bin/sh\nprintf 'stdout: %s\\n' \"$*\"\nprintf 'stderr message\\n' >&2\n"
                'exit "$HOOK_TEST_STATUS"\n'
            )
            pre_commit.chmod(0o755)
            env = dict(os.environ, PATH=f"{temp}{os.pathsep}{os.defpath}", TMPDIR=temp)
            for hook in ("pre-commit", "pre-push"):
                for status in (0, 17):
                    with self.subTest(hook=hook, status=status):
                        result = subprocess.run(
                            [str(ROOT / "scripts/hooks" / hook), "origin", "remote"],
                            input="",
                            capture_output=True,
                            text=True,
                            env=dict(env, HOOK_TEST_STATUS=str(status)),
                            check=False,
                        )
                        self.assertEqual(result.returncode, status)
                        self.assertEqual(result.stdout, "")
                        if status:
                            self.assertIn(f"--hook-type={hook}", result.stderr)
                            self.assertIn("-- origin remote", result.stderr)
                            self.assertIn("stderr message", result.stderr)
                        else:
                            self.assertEqual(result.stderr, "")
                        self.assertEqual(
                            sorted(path.name for path in directory.iterdir()), ["pre-commit"]
                        )


if __name__ == "__main__":
    unittest.main()
