"""Checks for the just -> ansible entry point; never touches a host."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WRAPPER = ROOT / "scripts" / "ansible-deploy.sh"
PLAYBOOK = "deploy/ansible/deploy.yml"
REQUIRED = {
    "DEPLOY_HOST": "node.example",
    "DEPLOY_USER": "root",
    "DEPLOY_DIR": "/srv/cormex",
    "DEPLOY_API": "https://memory.example",
}


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

    def fake_logging(self, *variables):
        """ansible-playbook stand-in that records its argv and the named variables."""
        fmt = " ".join(["%s"] + [f"{name.removeprefix('DEPLOY_')}=%s" for name in variables])
        arguments = '"$*"' + "".join(f' "${name}"' for name in variables)
        self.fake("ansible-playbook", f'printf "{fmt}\\n" {arguments} >> "{self.log}"\n')

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
                       if not key.startswith("DEPLOY_") and key != "ANSIBLE_SSH_COMMON_ARGS"}
        environment["PATH"] = ":".join([str(self.bin), str(self.shell_tools("bash", "dirname", "mktemp", "rm"))])
        environment.update(env or {})
        return subprocess.run([str(WRAPPER), *args], cwd=ROOT, env=environment,
                              text=True, capture_output=True, check=False)

    def calls(self):
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def test_all_required_values_reach_the_playbook(self):
        self.fake_logging("DEPLOY_HOST", "DEPLOY_USER", "DEPLOY_DIR", "DEPLOY_API")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env=REQUIRED)
        self.assertEqual(result.returncode, 0, result.stderr)
        call = self.calls()[0]
        self.assertIn(f"-i {self.inventory}", call)
        self.assertIn(PLAYBOOK, call)
        self.assertIn("-e cormex_version=v0.1.18", call)
        self.assertIn("HOST=node.example USER=root DIR=/srv/cormex API=https://memory.example", call,
                      "the playbook reads them from the environment; nothing may be dropped")

    def test_a_missing_value_is_named_before_connecting(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(self.inventory))
        self.assertEqual(result.returncode, 1)
        for name in REQUIRED:
            self.assertIn(name, result.stderr, f"{name} must be named in the error")
        self.assertEqual(self.calls(), [], "nothing may reach ansible without its inputs")

    def test_a_local_connection_needs_only_dir_and_api(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={
            "DEPLOY_CONNECTION": "local",
            "DEPLOY_DIR": "/srv/cormex",
            "DEPLOY_API": "http://127.0.0.1:8080",
        })
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_syntax_check_needs_no_environment(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("--syntax-check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--syntax-check", self.calls()[0])
        self.assertNotIn("cormex_version", self.calls()[0])

    def test_syntax_check_ignores_inherited_upgrade_inputs(self):
        self.fake("ansible-playbook")
        result = self.run_wrapper("--syntax-check", env={"DEPLOY_FROM_VERSION": "bad", "DEPLOY_BUNDLE_SHA256": "bad"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--syntax-check", self.calls()[0])

    def test_upgrade_requires_both_approval_inputs_before_ansible(self):
        self.fake("ansible-playbook")
        for extra in ({"DEPLOY_FROM_VERSION": "v0.1.18"}, {"DEPLOY_BUNDLE_SHA256": "a" * 64}):
            result = self.run_wrapper("v0.1.19", str(self.inventory), env={**REQUIRED, **extra})
            self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [], "incomplete approval must never reach the host")

    def test_upgrade_uses_private_extra_vars_and_cleans_up_after_ansible(self):
        self.fake("python3", 'for argument do output="$argument"; done\nprintf \'{"cormex_expectations":{}}\\n\' > "$output"\n')
        self.fake("ansible-playbook")
        result = self.run_wrapper("v0.1.19", str(self.inventory), env={
            **REQUIRED, "DEPLOY_FROM_VERSION": "v0.1.18", "DEPLOY_BUNDLE_SHA256": "a" * 64})
        self.assertEqual(result.returncode, 0, result.stderr)
        call = self.calls()[0]
        self.assertIn("cormex_from_version=v0.1.18", call)
        extra_vars = next(part[1:] for part in call.split() if part.startswith("@.deploy-expectations."))
        self.assertFalse((ROOT / extra_vars).exists(), "approval parameter file must be cleaned up after use")

    def test_container_path_mounts_the_key_read_only(self):
        self.fake("docker")
        known_hosts = Path(self.temp.name) / "known_hosts"
        known_hosts.write_text("node.example ssh-ed25519 AAAA\n", encoding="utf-8")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={
            **REQUIRED,
            "DEPLOY_SSH_KEY": str(self.key),
            "DEPLOY_KNOWN_HOSTS": str(known_hosts),
            "DEPLOY_PULL": "false",
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
        self.assertIn("-e DEPLOY_PULL=false", run_call, "every DEPLOY_* knob has to cross into the container")
        self.assertIn(PLAYBOOK, run_call)
        self.assertIn("cormex_version=v0.1.18", run_call)

    def test_a_missing_key_is_reported_before_docker_runs(self):
        self.fake("docker")
        result = self.run_wrapper("v0.1.18", str(self.inventory), env={
            **REQUIRED,
            "DEPLOY_SSH_KEY": str(Path(self.temp.name) / "absent-key"),
        })
        self.assertEqual(result.returncode, 1)
        self.assertIn("private key not found", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_without_ansible_or_docker_the_failure_names_both_options(self):
        result = self.run_wrapper("v0.1.18", str(self.inventory), env=REQUIRED)
        self.assertEqual(result.returncode, 1)
        self.assertIn("ansible-core", result.stderr)
        self.assertIn("Docker", result.stderr)


if __name__ == "__main__":
    unittest.main()
