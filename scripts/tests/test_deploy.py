"""Offline deploy-contract tests; never talk to a real stack or a registry."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "deploy.py"
spec = importlib.util.spec_from_file_location("cormex_deploy", SCRIPT)
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)


class DeployEnvTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def write_env(self, text):
        (self.directory / ".env").write_text(text, encoding="utf-8")

    def test_values_are_read_literally(self):
        self.write_env("# comment\nCORMEX_VERSION=v0.1.18\n\nPOSTGRES_PASSWORD=\"p@ss=word\"\n")
        values = deploy.load_env(self.directory)
        self.assertEqual(values["CORMEX_VERSION"], "v0.1.18")
        self.assertEqual(values["POSTGRES_PASSWORD"], "p@ss=word", "an = inside a value is data, not a separator")

    def test_missing_env_names_the_file_to_copy(self):
        with self.assertRaises(deploy.DeployError) as caught:
            deploy.load_env(self.directory)
        self.assertIn(".env", str(caught.exception))

    def test_up_refuses_a_pin_that_differs_from_the_requested_version(self):
        self.write_env("CORMEX_VERSION=v0.1.17\nCORMEX_AUTH_TOKEN_SHA256=" + "a" * 64
                       + "\nPOSTGRES_PASSWORD=p\nRUSTFS_SECRET_KEY=s\n")
        with mock.patch.object(deploy, "compose") as compose:
            with self.assertRaises(deploy.DeployError) as caught:
                deploy.up("v0.1.18", self.directory)
        self.assertIn("pins v0.1.17", str(caught.exception))
        compose.assert_not_called()

    def test_up_refuses_an_empty_credential(self):
        self.write_env("CORMEX_VERSION=v0.1.18\nCORMEX_AUTH_TOKEN_SHA256=\nPOSTGRES_PASSWORD=p\nRUSTFS_SECRET_KEY=s\n")
        with mock.patch.object(deploy, "compose") as compose:
            with self.assertRaises(deploy.DeployError) as caught:
                deploy.up("v0.1.18", self.directory)
        self.assertIn("CORMEX_AUTH_TOKEN_SHA256", str(caught.exception))
        compose.assert_not_called()

    def test_up_pulls_both_images_and_verifies_the_result(self):
        self.write_env("CORMEX_VERSION=v0.1.18\nCORMEX_AUTH_TOKEN_SHA256=" + "a" * 64
                       + "\nPOSTGRES_PASSWORD=p\nRUSTFS_SECRET_KEY=s\n")
        with mock.patch.object(deploy, "compose") as compose, \
                mock.patch.object(deploy, "verify", return_value={}) as verify, \
                mock.patch("builtins.print"):
            deploy.up("v0.1.18", self.directory)
        self.assertEqual([call.args[1:] for call in compose.call_args_list],
                         [("pull", "cormex", "web"), ("up", "-d")])
        verify.assert_called_once_with(self.directory, "v0.1.18", project=None)


class DeployComposeTests(unittest.TestCase):
    def test_the_stack_env_beats_an_exported_variable(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "docker-compose.yml").write_text("services: {}\n", encoding="utf-8")
            (directory / ".env").write_text("CORMEX_VERSION=v0.1.18\n", encoding="utf-8")
            with mock.patch.object(deploy, "run") as runner, \
                    mock.patch.dict(deploy.os.environ, {"CORMEX_VERSION": "v0.0.9", "OTHER": "kept"}):
                deploy.compose(directory, "up", "-d")
        environment = runner.call_args.kwargs["env"]
        self.assertNotIn("CORMEX_VERSION", environment,
                         "an exported pin must not override the .env the operator wrote")
        self.assertEqual(environment["OTHER"], "kept")

    def test_compose_includes_both_override_names(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "docker-compose.yml").write_text("services: {}\n", encoding="utf-8")
            (directory / "docker-compose.override.yml").write_text("services: {}\n", encoding="utf-8")
            (directory / "compose.override.yaml").write_text("services: {}\n", encoding="utf-8")
            with mock.patch.object(deploy, "run") as runner:
                deploy.compose(directory, "up", "-d")
        args = list(runner.call_args.args[1:])
        self.assertEqual(args, ["compose", "-f", "docker-compose.yml",
                                "-f", "docker-compose.override.yml", "-f", "compose.override.yaml",
                                "up", "-d"],
                         "an explicit -f turns off compose's own override auto-loading, so both "
                         "names have to be listed")


class DeployVerifyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        runtime = mock.patch.object(deploy, "verify_runtime", return_value={})
        runtime.start()
        self.addCleanup(runtime.stop)
        (self.directory / ".env").write_text(
            "CORMEX_VERSION=v0.1.18\nCORMEX_PORT=18080\n", encoding="utf-8")

    def responses(self, health, root=(b"<html><title>Cormex</title></html>", "text/html")):
        def fetch(url, timeout=10):
            if url.endswith("/api/health"):
                return "application/json", json.dumps(health).encode()
            return root[1], root[0]
        return fetch

    def healthy(self, **overrides):
        health = {"status": "ok", "role": "cormexd", "version": "0.1.18",
                  "database": "ok", "blob_backend": "s3", "auth": "token",
                  "capture_encryption": "encrypted", "embedding": {"model": "api:bge-m3"}}
        health.update(overrides)
        return health

    def test_verify_accepts_the_deployed_version(self):
        with mock.patch.object(deploy, "fetch", side_effect=self.responses(self.healthy())), \
                mock.patch("builtins.print"):
            status = deploy.verify(self.directory, "v0.1.18", timeout=0)
        self.assertEqual(status["version"], "0.1.18")

    def test_verify_rejects_failed_database_local_blobs_and_missing_encryption(self):
        for overrides in ({"database": "error"}, {"blob_backend": "local"}, {"capture_encryption": "plaintext"}):
            with self.subTest(overrides=overrides), mock.patch.object(
                    deploy, "fetch", side_effect=self.responses(self.healthy(**overrides))):
                with self.assertRaises(deploy.DeployError):
                    deploy.verify(self.directory, "v0.1.18", timeout=0)

    def test_verify_rejects_a_different_running_version(self):
        with mock.patch.object(deploy, "fetch", side_effect=self.responses(self.healthy(version="0.1.17"))), \
                mock.patch("builtins.print"):
            with self.assertRaises(deploy.DeployError) as caught:
                deploy.verify(self.directory, "v0.1.18", timeout=0)
        self.assertIn("running version", str(caught.exception))

    def test_verify_rejects_a_stack_that_is_not_cormexd(self):
        with mock.patch.object(deploy, "fetch", side_effect=self.responses(self.healthy(role="cormex"))), \
                mock.patch("builtins.print"):
            with self.assertRaises(deploy.DeployError):
                deploy.verify(self.directory, "v0.1.18", timeout=0)

    def test_verify_rejects_api_answers_served_by_the_web_image(self):
        def fetch(url, timeout=10):
            return "text/html", b"<html><title>Cormex</title></html>"
        with mock.patch.object(deploy, "fetch", side_effect=fetch), mock.patch("builtins.print"):
            with self.assertRaises(deploy.DeployError) as caught:
                deploy.verify(self.directory, "v0.1.18", timeout=0)
        self.assertIn("proxy", str(caught.exception))

    def test_verify_rejects_a_root_that_is_not_html(self):
        with mock.patch.object(deploy, "fetch", side_effect=self.responses(
                self.healthy(), root=(b'{"error":true}', "application/json"))), mock.patch("builtins.print"):
            with self.assertRaises(deploy.DeployError):
                deploy.verify(self.directory, "v0.1.18", timeout=0)


class DeployRehearsalTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.rehearsal = Path(self.temp.name) / "rehearsal"

    def test_rehearsal_keeps_only_the_digest_and_writes_private_files(self):
        with mock.patch.object(deploy, "REHEARSAL", self.rehearsal), \
                mock.patch.object(deploy, "port_free", return_value=True), \
                mock.patch.object(deploy, "token_hash", return_value="a" * 64), \
                mock.patch("builtins.print"):
            deploy.rehearsal_files("v0.1.18")
        env = (self.rehearsal / ".env").read_text(encoding="utf-8")
        self.assertIn("CORMEX_AUTH_TOKEN_SHA256=" + "a" * 64, env)
        self.assertNotIn("CORMEXD_TOKEN", env, "the plaintext token must not be written anywhere")
        self.assertEqual((self.rehearsal / ".env").stat().st_mode & 0o777, 0o600)
        override = (self.rehearsal / "compose.override.yaml").read_text(encoding="utf-8")
        self.assertIn("container_name: cormex-rehearsal-cormex", override)
        self.assertIn("CORMEX_SECRET_KEY", override)
        self.assertTrue((self.rehearsal / "docker-compose.yml").is_file())
        self.assertTrue((self.rehearsal / "proxy.conf").is_file())

    def test_rehearsal_reuses_existing_credentials_but_moves_the_pin(self):
        self.rehearsal.mkdir(parents=True)
        (self.rehearsal / ".env").write_text(
            "CORMEX_VERSION=v0.1.17\nPOSTGRES_PASSWORD=keep-me\nCORMEX_AUTH_TOKEN_SHA256=" + "b" * 64 + "\n",
            encoding="utf-8")
        with mock.patch.object(deploy, "REHEARSAL", self.rehearsal), \
                mock.patch.object(deploy, "port_free", return_value=True), \
                mock.patch.object(deploy, "token_hash", side_effect=AssertionError("must not regenerate")), \
                mock.patch("builtins.print"):
            deploy.rehearsal_files("v0.1.18")
        env = deploy.load_env(self.rehearsal)
        self.assertEqual(env["CORMEX_VERSION"], "v0.1.18")
        self.assertEqual(env["POSTGRES_PASSWORD"], "keep-me", "regenerating it would orphan the volumes")
        self.assertEqual(env["CORMEX_AUTH_TOKEN_SHA256"], "b" * 64)

    def test_rehearsal_refuses_a_port_that_is_taken(self):
        with mock.patch.object(deploy, "REHEARSAL", self.rehearsal), \
                mock.patch.object(deploy, "port_free", return_value=False), \
                mock.patch("builtins.print"):
            with self.assertRaises(deploy.DeployError) as caught:
                deploy.rehearsal_files("v0.1.18")
        self.assertIn("already in use", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
