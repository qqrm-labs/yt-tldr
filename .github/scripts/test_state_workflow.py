"""Exercise diagnostic workflow shell and boundaries with an isolated fake bot.

Historical checkpoint tests remain in test_remote_state.py. This workflow must
not restore or publish state, or host a permanent Telegram worker.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

WORKFLOW = Path(__file__).resolve().parents[1] / "workflows/run-bot.yml"
VIDEO = "Process public test video"
DOCTOR = "Check bot readiness"
SETUP = "Configure bot commands"


def step_block(name):
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines()
    start = lines.index("      - name: " + name) + 1
    block = []
    for line in lines[start:]:
        if line.strip() and not line.startswith("        "):
            break
        block.append(line)
    return "\n".join(block)


def step_field(name, field):
    match = re.search(r"^        " + field + r": (.+)$", step_block(name), re.MULTILINE)
    if match is None:
        raise AssertionError(f"step {name!r} has no {field!r} field")
    return match[1]


def step_script(name):
    block = step_block(name)
    value = step_field(name, "run")
    if value != "|":
        return value
    lines = block.splitlines()
    start = lines.index("        run: |") + 1
    script = []
    for line in lines[start:]:
        if line.strip() and not line.startswith("          "):
            break
        script.append(line[10:])
    return "\n".join(script)


def step_environment(name):
    return dict(re.findall(r"^          ([A-Z][A-Z0-9_]*): (.+)$", step_block(name), re.MULTILINE))


class DiagnosticWorkflowTests(unittest.TestCase):
    def run_step(self, name, *, url="https://youtu.be/jNQXAC9IVRw", output="", status=0):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "target/release/yt-tldr"
            binary.parent.mkdir(parents=True)
            binary.write_text("#!" + sys.executable + "\n" + textwrap.dedent('''\
                import json, os, sys
                with open(os.environ["FIXTURE_CALLS"], "a") as log:
                    log.write(json.dumps({
                        "args": sys.argv[1:],
                        "telegram_env": sorted(key for key in os.environ
                            if "TELEGRAM" in key or key == "ALLOWED_CHAT_IDS"),
                    }) + "\\n")
                sys.stdout.write(os.environ["FIXTURE_STDOUT"])
                raise SystemExit(int(os.environ["FIXTURE_STATUS"]))
                '''), encoding="utf-8")
            binary.chmod(0o755)
            # Only synthetic step-scoped values; never inherit real credentials
            # or shell startup configuration from the test runner.
            env = {key: "fixture-" + key.lower() for key in step_environment(name)}
            env.update({
                "PATH": os.defpath, "TEST_URL": url, "GITHUB_SHA": "0123456789abcdef",
                "GITHUB_STEP_SUMMARY": str(root / "summary"),
                "GITHUB_OUTPUT": str(root / "workflow-output"),
                "GITHUB_ENV": str(root / "workflow-env"),
                "FIXTURE_CALLS": str(root / "calls.jsonl"),
                "FIXTURE_STDOUT": output, "FIXTURE_STATUS": str(status),
            })
            result = subprocess.run(
                [os.environ.get("BASH_EXECUTABLE", "bash"), "--noprofile", "--norc",
                 "-c", step_script(name)], cwd=root, env=env,
                capture_output=True, text=True, timeout=10, check=False,
            )
            calls_path = root / "calls.jsonl"
            calls = [json.loads(line) for line in calls_path.read_text().splitlines()] if calls_path.exists() else []
            summary = (root / "summary").read_text() if (root / "summary").exists() else ""
            side_effects = [path.name for path in root.glob("injected-*")]
            side_effects += [name for name in ["workflow-output", "workflow-env"] if (root / name).exists()]
            return result, calls, summary, side_effects

    def test_public_url_is_one_literal_argument_even_with_shell_metacharacters(self):
        url = ('https://youtu.be/jNQXAC9IVRw?x="$(touch injected-dollar)"'
               '&y=`touch injected-backtick`\n; touch injected-line # \' quoted value')
        result, calls, _, side_effects = self.run_step(VIDEO, url=url)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [{"args": ["process", url], "telegram_env": []}])
        self.assertEqual(result.stdout, "")
        self.assertEqual(side_effects, [])

    def test_generated_text_only_reaches_summary_not_runner_commands(self):
        generated = ("TL;DR: fixture output\n::error::untrusted generated text\n"
                     "::set-output name=injected::value\n::add-mask::fixture\n"
                     "$(touch injected-generated)\n")
        result, calls, summary, side_effects = self.run_step(VIDEO, output=generated)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")
        self.assertEqual(summary, "## Public video check\n\nCommit: 0123456789abcdef\n\n" + generated)
        self.assertEqual(side_effects, [])

    def test_public_video_preserves_failing_binary_exit(self):
        result, calls, summary, _ = self.run_step(VIDEO, output="partial\n", status=17)
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertEqual(calls[0]["args"][0], "process")
        self.assertTrue(summary.endswith("partial\n"))
        self.assertEqual(result.stdout, "")

    def test_readiness_runs_doctor_and_copies_report_to_summary(self):
        report = "Telegram bot: @fixture_bot\nWebhook: disabled\n"
        result, calls, summary, _ = self.run_step(DOCTOR, output=report)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call["args"] for call in calls], [["doctor"]])
        self.assertEqual(summary, report)
        self.assertEqual(result.stdout, report)

    def test_readiness_pipefail_preserves_doctor_failure(self):
        result, calls, _, _ = self.run_step(DOCTOR, status=23)
        self.assertEqual(calls[0]["args"], ["doctor"])
        self.assertEqual(result.returncode, 23, "tee must not hide doctor failure")

    def test_setup_runs_only_setup_with_dedicated_identity(self):
        result, calls, summary, side_effects = self.run_step(SETUP)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [{
            "args": ["setup"],
            "telegram_env": ["EXPECTED_TELEGRAM_BOT_USERNAME", "YT_TELEGRAM_BOT_TOKEN"],
        }])
        self.assertEqual(summary, "")
        self.assertEqual(side_effects, [])

    def test_only_manual_diagnostics_are_exposed_without_poll_or_serve(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        triggers = text.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
        self.assertEqual(re.findall(r"^  ([a-z_]+):", triggers, re.MULTILINE), ["workflow_dispatch"])
        self.assertNotRegex(text, r"(?m)^\s*(?:schedule|cron):")
        choices = re.search(r"(?m)^        options:\n((?:          - .+\n)+)", triggers)
        self.assertIsNotNone(choices)
        self.assertEqual(re.findall(r"- (.+)", choices[1]), ["check", "setup"])
        names = re.findall(r"^      - name: (.+)$", text, re.MULTILINE)
        scripts = "\n".join(step_script(name) for name in names if "        run:" in step_block(name))
        self.assertNotRegex(scripts, r"\b(?:poll|serve)\b")
        self.assertEqual(re.findall(r"\byt-tldr\s+([a-z_-]+)", scripts), ["doctor", "setup", "process"])

    def test_workflow_has_read_only_permissions_and_no_state_publication(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        contents = re.findall(r"^\s*contents: (.+)$", text, re.MULTILINE)
        self.assertTrue(contents)
        self.assertEqual(set(contents), {"read"})
        self.assertTrue(all(not value for value in re.findall(r"^\s*permissions:([^\n]*)$", text, re.MULTILINE)))
        self.assertNotRegex(text, r"(?m)^\s*[a-z-]+:\s*write\s*(?:#.*)?$")
        self.assertIn("persist-credentials: false", text)
        for forbidden in ["remote_state.py", "DELIVERY_CHECKPOINT", "STATE_PATH", "bot-state", "delivery-state"]:
            self.assertNotIn(forbidden, text)
        self.assertNotRegex(text, r"\bgit\s+(?:push|worktree)\b")

    def test_telegram_credentials_are_scoped_to_doctor_and_setup(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        names = re.findall(r"^      - name: (.+)$", text, re.MULTILINE)
        with_telegram = set()
        for name in names:
            environment = step_environment(name)
            if any("TELEGRAM" in key or key == "ALLOWED_CHAT_IDS" for key in environment):
                with_telegram.add(name)
                self.assertEqual(environment["YT_TELEGRAM_BOT_TOKEN"], "${{ secrets.YT_TELEGRAM_BOT_TOKEN }}")
                self.assertEqual(environment["EXPECTED_TELEGRAM_BOT_USERNAME"], "${{ vars.EXPECTED_TELEGRAM_BOT_USERNAME }}")
            if name not in {DOCTOR, SETUP}:
                self.assertNotIn("secrets.YT_TELEGRAM_BOT_TOKEN", step_block(name))
        self.assertEqual(with_telegram, {DOCTOR, SETUP})
        self.assertEqual(re.findall(r"^\s*env:.*$", text, re.MULTILINE), ["        env:"] * 3)
        self.assertNotRegex(text, r"\bTELEGRAM_BOT_TOKEN\b")

    def test_public_video_selection_excludes_telegram_steps_and_credentials(self):
        self.assertEqual(step_field(DOCTOR, "if"), "inputs.youtube_url == ''")
        self.assertEqual(step_field(SETUP, "if"), "inputs.operation == 'setup' && inputs.youtube_url == ''")
        self.assertEqual(step_field(VIDEO, "if"), "inputs.youtube_url != ''")
        environment = step_environment(VIDEO)
        self.assertEqual(environment["TEST_URL"], "${{ inputs.youtube_url }}")
        self.assertFalse(any("TELEGRAM" in key or key == "ALLOWED_CHAT_IDS" for key in environment))
        self.assertNotIn("${{", step_script(VIDEO), "untrusted inputs must enter bash through the environment")


if __name__ == "__main__":
    unittest.main()
