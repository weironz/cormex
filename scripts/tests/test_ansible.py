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
        self.inventory = Path(self.temp.name) / "inventory.yml"
        self.inventory.write_text("all:\n  hosts:\n    cormex: {}\n", encoding="utf-8")
        self.key = Path(self.temp.name) / "deploy_key"
        self.key.write_text("not-a-real-key\n", encoding="utf-8")

    def fake(self, name, body=None):
        body = body or f'printf "%s\\n" "$*" >> "{self.log}"\n'
        path = self.bin / name
        path.write_text(f"#!/bin/sh\n{body}", encoding="utf-8")
        path.chmod(0o755)
        return path

    def shell_tools(self, *names):
        """A PATH with only what the wrapper itself executes.

        /usr/bin is a symlink to /bin on this host, so "PATH=/usr/bin:/bin" still
        contains the real docker and a test would deploy for real.
        """
        core = Path(self.temp.name) / "core"
        core.mkdir(exist_ok=True)
        for name in names:
            link = core / name
            if not link.exists():
                link.symlink_to(f"/usr/bin/{name}")
        return core

    def run_wrapper(self, *args, env=None):
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith("CORMEX_") and key not in ("ANSIBLE_SSH_COMMON_ARGS",)}
        environment["PATH"] = ":".join([str(self.bin), str(self.shell_tools("bash", "dirname"))])
        environment["DEPLOY_ENV_FILE"] = str(Path(self.temp.name) / "no-such.env")
        environment.update(env or {})
        return subprocess.run([str(WRAPPER), *args], cwd=ROOT, env=environment,
                              text=True, capture_output=True, check=False)

    def calls(self):
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def test_runs_the_playbook_natively_when_ansible_is_installed(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={"DEPLOY_HOST": "node.example"})
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(len(calls), 1)
        self.assertIn(f"-i {self.inventory}", calls[0])
        self.assertIn(PLAYBOOK, calls[0])
        self.assertIn("-e cormex_version=v0.1.18", calls[0])

    def test_a_missing_host_is_refused_before_connecting(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(self.inventory))
        self.assertEqual(result.returncode, 1)
        self.assertIn("DEPLOY_HOST", result.stderr)
        self.assertEqual(self.calls(), [], "nothing may reach ansible without a host")

    def test_a_local_connection_needs_no_host(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={"DEPLOY_CONNECTION": "local"})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_gitignored_env_file_supplies_the_host(self):
        self.fake("ansible-playbook",
                  f'printf "%s HOST=%s DIR=%s\\n" "$*" "$DEPLOY_HOST" "$DEPLOY_DIR" >> "{self.log}"\n')
        values = Path(self.temp.name) / "deploy.env"
        values.write_text("DEPLOY_HOST=from-file.example\nDEPLOY_DIR=/srv/cormex\n", encoding="utf-8")
        result = self.run_wrapper("v0.1.18", str(self.inventory),
                                  env={"DEPLOY_ENV_FILE": str(values)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("HOST=from-file.example", self.calls()[0])
        self.assertIn("DIR=/srv/cormex", self.calls()[0], "values from the gitignored env file must be exported")

    def test_an_exported_value_beats_the_env_file(self):
        self.fake("ansible-playbook",
                  f'printf "%s HOST=%s\\n" "$*" "$DEPLOY_HOST" >> "{self.log}"\n')
        values = Path(self.temp.name) / "deploy.env"
        values.write_text("DEPLOY_HOST=from-file.example\n", encoding="utf-8")
        result = self.run_wrapper("v0.1.18", str(self.inventory),
                                  env={"DEPLOY_ENV_FILE": str(values),
                                       "DEPLOY_HOST": "from-shell.example"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("HOST=from-shell.example", self.calls()[0],
                      "the shell must win over the file, or a one-off override is impossible")

    def test_syntax_check_needs_no_inventory_variables(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("--syntax-check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--syntax-check", self.calls()[0])
        self.assertNotIn("cormex_version", self.calls()[0])

    def test_container_path_mounts_the_key_read_only(self):
        self.fake("docker")
        known_hosts = Path(self.temp.name) / "known_hosts"
        known_hosts.write_text("node.example ssh-ed25519 AAAA\n", encoding="utf-8")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={
            "DEPLOY_HOST": "node.example",
            "DEPLOY_SSH_KEY": str(self.key),
            "DEPLOY_KNOWN_HOSTS": str(known_hosts),
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertTrue(calls[0].startswith("build -q -t cormex-ansible:control"), calls[0])
        run_call = calls[1]
        self.assertTrue(run_call.startswith("run --rm"), run_call)
        self.assertIn(f"-v {self.key}:/root/.ssh/deploy_key:ro", run_call)
        self.assertIn("-e DEPLOY_SSH_KEY=/root/.ssh/deploy_key", run_call)
        self.assertIn(f"-v {known_hosts}:/root/.ssh/known_hosts_deploy:ro", run_call)
        self.assertIn("UserKnownHostsFile=/root/.ssh/known_hosts_deploy", run_call)
        self.assertIn("-e DEPLOY_HOST=node.example", run_call)
        self.assertIn(PLAYBOOK, run_call)
        self.assertIn("cormex_version=v0.1.18", run_call)

    def test_a_missing_key_is_reported_before_docker_runs(self):
        self.fake("docker")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={
            "DEPLOY_HOST": "node.example",
            "DEPLOY_SSH_KEY": str(Path(self.temp.name) / "absent-key"),
        })
        self.assertEqual(result.returncode, 1)
        self.assertIn("private key not found", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_without_ansible_or_docker_the_failure_names_both_options(self):
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={"DEPLOY_HOST": "node.example"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("ansible-core", result.stderr)
        self.assertIn("Docker", result.stderr)


if __name__ == "__main__":
    unittest.main()
