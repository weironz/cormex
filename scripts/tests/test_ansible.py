"""Checks for the just -> ansible entry point; never touches a host."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WRAPPER = ROOT / "scripts" / "ansible-deploy.sh"
PLAYBOOK = "deploy/ansible/deploy.yml"


class AnsibleWrapperTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.bin = Path(self.temp.name) / "bin"
        self.bin.mkdir()
        self.log = Path(self.temp.name) / "calls.log"
        self.inventory = Path(self.temp.name) / "inventory.ini"
        self.inventory.write_text("[cormex]\nlocalhost ansible_connection=local\n", encoding="utf-8")

    def fake(self, name):
        path = self.bin / name
        path.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{self.log}"\n', encoding="utf-8")
        path.chmod(0o755)

    def shell_tools(self, *names):
        """A PATH with only what the wrapper itself needs.

        /usr/bin is a symlink to /bin here, so "PATH=/usr/bin:/bin" still contains the
        real docker and the test would deploy for real. Link the few binaries the
        script executes instead.
        """
        core = Path(self.temp.name) / "core"
        core.mkdir(exist_ok=True)
        for name in names:
            link = core / name
            if not link.exists():
                link.symlink_to(f"/usr/bin/{name}")
        return core

    def run_wrapper(self, *args, with_shell=True):
        environment = dict(os.environ)
        parts = [str(self.bin)]
        if with_shell:
            parts.append(str(self.shell_tools("bash", "dirname")))
        environment["PATH"] = ":".join(parts)
        return subprocess.run([str(WRAPPER), *args], cwd=ROOT, env=environment,
                              text=True, capture_output=True, check=False)

    def calls(self):
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def test_runs_the_playbook_natively_when_ansible_is_installed(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(self.inventory))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(len(calls), 1)
        self.assertIn(f"-i {self.inventory}", calls[0])
        self.assertIn(PLAYBOOK, calls[0])
        self.assertIn("-e cormex_version=v0.1.18", calls[0])

    def test_syntax_check_needs_no_inventory_and_no_tag(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("--syntax-check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--syntax-check", self.calls()[0])
        self.assertNotIn("cormex_version", self.calls()[0])

    def test_missing_inventory_explains_what_to_copy(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(Path(self.temp.name) / "absent.ini"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("inventory.example.ini", result.stderr)
        self.assertEqual(self.calls(), [], "a missing inventory must not reach ansible")

    def test_falls_back_to_the_control_node_container_without_ansible(self):
        self.fake("docker")
        result = self.run_wrapper("v0.1.18", str(self.inventory))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertTrue(calls[0].startswith("build -q -t cormex-ansible:control"), calls[0])
        self.assertTrue(calls[1].startswith("run --rm"), calls[1])
        self.assertIn(PLAYBOOK, calls[1])
        self.assertIn("cormex_version=v0.1.18", calls[1])

    def test_without_ansible_or_docker_the_failure_names_both_options(self):
        self.fake("true")  # anything, as long as docker is absent from PATH
        (self.bin / "true").unlink()
        result = self.run_wrapper("v0.1.18", str(self.inventory))
        self.assertEqual(result.returncode, 1)
        self.assertIn("ansible-core", result.stderr)
        self.assertIn("Docker", result.stderr)


if __name__ == "__main__":
    unittest.main()
