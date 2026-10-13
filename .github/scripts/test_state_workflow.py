"""Exercise the actual workflow shell using an isolated, fake git transport."""
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

WORKFLOW = Path(__file__).resolve().parents[1] / "workflows/run-bot.yml"


def step_script(prefix):
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines()
    start = next(i for i, line in enumerate(lines) if line.startswith("      - name: " + prefix))
    run = next(i for i in range(start, len(lines)) if lines[i] == "        run: |") + 1
    script = []
    for line in lines[run:]:
        if line and not line.startswith("          "):
            break
        script.append(line[10:])
    return "\n".join(script)


class StateWorkflowTests(unittest.TestCase):
    def run_step(self, prefix, mode, restored="old"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "data").mkdir()
            (root / "data/state.json").write_text("{}")
            git = root / "git"
            git.write_text(textwrap.dedent('''\
                #!/usr/bin/env python3
                import os, sys
                args = sys.argv[1:]
                with open(os.environ["GIT_CALLS"], "a") as log:
                    log.write(" ".join(args) + "\\n")
                mode = os.environ["FAKE_MODE"]
                if args[0] == "ls-remote":
                    if mode == "transport_error": sys.exit(128)
                    if mode != "missing": print(("new" if mode == "changed" else "old") + "\\trefs/heads/bot-state")
                elif args[0] == "fetch":
                    if mode == "fetch_error": sys.exit(128)
                elif args[0] == "rev-parse": print("old")
                elif args[0] == "archive": sys.stdout.buffer.write(bytes(10240))
                elif args[0] == "config": pass
                else: sys.exit(42)
                '''))
            git.chmod(0o755)
            (root / "sleep").write_text("#!/bin/sh\nexit 0\n")
            (root / "sleep").chmod(0o755)
            env = {**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                   "FAKE_MODE": mode, "GIT_CALLS": str(root / "calls"),
                   "GITHUB_OUTPUT": str(root / "output"), "RUNNER_TEMP": str(root),
                   "RESTORED_STATE_HEAD": restored}
            result = subprocess.run(["bash", "-c", step_script(prefix)], cwd=root, env=env,
                                    capture_output=True, text=True, timeout=10, check=False)
            calls = (root / "calls").read_text()
            output = (root / "output").read_text() if (root / "output").exists() else ""
            return result, calls, output

    def test_remote_failure_is_not_bootstrap(self):
        result, calls, output = self.run_step("Restore persistent state", "transport_error")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("fetch", calls)
        self.assertEqual(output, "")

    def test_confirmed_missing_branch_bootstraps(self):
        result, calls, output = self.run_step("Restore persistent state", "missing")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "head=\n")
        self.assertNotIn("fetch", calls)

    def test_existing_branch_fetch_failure_is_fatal(self):
        result, calls, _ = self.run_step("Restore persistent state", "fetch_error")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls.count("fetch origin"), 3)

    def test_existing_state_records_restored_commit(self):
        result, _, output = self.run_step("Restore persistent state", "existing")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "head=old\n")

    def test_save_never_pushes_after_remote_error_or_concurrent_update(self):
        for mode in ["transport_error", "changed"]:
            with self.subTest(mode=mode):
                result, calls, _ = self.run_step("Save state", mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("push", calls)
                self.assertNotIn("worktree", calls)


if __name__ == "__main__":
    unittest.main()
