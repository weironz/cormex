"""Exercise npm publishing end-to-end against a local fake npm executable."""

import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
NODE = shutil.which("node")

# This program replaces npm on PATH. It never opens a network connection and
# stores the fake registry in the test's temporary directory.
FAKE_NPM = r'''
import base64
import hashlib
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
cwd = Path.cwd()
state_path = Path(os.environ["FAKE_NPM_STATE"])
log_path = Path(os.environ["FAKE_NPM_LOG"])
state = json.loads(state_path.read_text())
mode = os.environ.get("FAKE_NPM_MODE", "normal")
pkg_path = cwd / "package.json"
pkg = json.loads(pkg_path.read_text()) if pkg_path.exists() else {}
key = pkg.get("name", "") + "@" + pkg.get("version", "")
record = {"args": args, "package": key, "cwd": str(cwd)}
if args[0] == "publish":
    data = Path(args[1]).read_bytes()
    record["integrity"] = "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode()
with log_path.open("a") as log:
    log.write(json.dumps(record) + "\n")

if args[0] == "pack":
    # Deterministic bytes include all source files and detect accidental rebuilds.
    files = [(str(p.relative_to(cwd)), base64.b64encode(p.read_bytes()).decode())
             for p in sorted(cwd.rglob("*")) if p.is_file() and p.suffix != ".tgz"]
    data = json.dumps(files, separators=(",", ":")).encode()
    (cwd / "fixture.tgz").write_bytes(data)
    if mode == "unsafe_pack":
        print(json.dumps([{"filename": "../fixture.tgz"}]))
    else:
        print(json.dumps([{"filename": "fixture.tgz"}]))
elif args[0] == "view":
    key = args[1]
    if mode in ("network", "auth"):
        print(json.dumps({"error": {"code": "ETIMEDOUT" if mode == "network" else "E401"}}))
        sys.exit(1)
    if mode == "malformed":
        print(json.dumps({"unexpected": "response"}))
    elif key not in state:
        print(json.dumps({"error": {"code": "E404"}}))
        sys.exit(1)
    else:
        print(json.dumps(state[key]))
elif args[0] == "publish":
    state[key] = record["integrity"] if mode != "post_publish_mismatch" else "sha512-corrupted"
    state_path.write_text(json.dumps(state))
else:
    raise SystemExit("unexpected fake npm command: " + repr(args))
'''


@unittest.skipUnless(NODE and shutil.which("tar") and os.name != "nt",
                     "Offline npm fixture requires Node, tar and POSIX executable scripts")
class NpmPublishTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.npm = self.root / "npm"
        self.npm.mkdir()
        for filename in ("publish.mjs", "package.json"):
            shutil.copyfile(ROOT / "npm" / filename, self.npm / filename)
        shutil.copytree(ROOT / "npm" / "bin", self.npm / "bin")
        self.dist = self.root / "dist"
        self.dist.mkdir()
        inner = "cormex-0.1.17-x86_64-unknown-linux-musl"
        with tarfile.open(self.dist / f"{inner}.tar.gz", "w:gz") as archive:
            info = tarfile.TarInfo(f"{inner}/cormex")
            data = b"offline CLI fixture, never executed\n"
            info.size = len(data)
            info.mode = 0o755
            archive.addfile(info, io.BytesIO(data))
        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        fake = self.fake_bin / "npm"
        fake.write_text(f"#!{sys.executable}\n" + FAKE_NPM)
        fake.chmod(0o755)
        self.state = self.root / "registry.json"
        self.state.write_text("{}")
        self.log = self.root / "calls.jsonl"
        self.env = {**os.environ, "PATH": str(self.fake_bin) + os.pathsep + os.environ.get("PATH", ""),
                    "FAKE_NPM_STATE": str(self.state), "FAKE_NPM_LOG": str(self.log),
                    "FAKE_NPM_MODE": "normal"}
        # The fixture never needs real registry credentials.
        self.env.pop("NPM_TOKEN", None)
        self.env.pop("NODE_AUTH_TOKEN", None)

    def publish(self, mode="normal"):
        return subprocess.run([NODE, str(self.npm / "publish.mjs"), "v0.1.17", str(self.dist)],
                              cwd=self.root, env={**self.env, "FAKE_NPM_MODE": mode},
                              text=True, capture_output=True, timeout=30)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def assert_failed_without_publish(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(any(call["args"][0] == "publish" for call in self.calls()),
                         "Unverified registry state must never trigger publication")

    def test_publishes_exact_packed_bytes_and_main_package_last(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.calls()
        self.assertEqual([call["args"][0] for call in calls],
                         ["pack", "view", "publish", "view"] * 2)
        publications = [call for call in calls if call["args"][0] == "publish"]
        self.assertEqual([call["package"] for call in publications],
                         ["@cormex/cli-linux-x64@0.1.17", "cormex@0.1.17"])
        state = json.loads(self.state.read_text())
        for call in publications:
            tarball = Path(call["args"][1])
            self.assertEqual(tarball.name, "fixture.tgz")
            self.assertEqual(tarball.parent, Path(call["cwd"]))
            self.assertEqual(call["integrity"], state[call["package"]])
            self.assertIn("--ignore-scripts", call["args"])
        for call in calls:
            if call["args"][0] == "pack":
                self.assertIn("--ignore-scripts", call["args"])

    def test_retry_skips_packages_with_matching_integrity(self):
        first = self.publish()
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        registry_before = self.state.read_bytes()
        self.log.write_text("")
        second = self.publish()
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual([call["args"][0] for call in self.calls()], ["pack", "view"] * 2)
        self.assertEqual(self.state.read_bytes(), registry_before)
        self.assertEqual(second.stdout.count("already published with matching integrity"), 2)

    def test_conflicting_existing_integrity_blocks_publication(self):
        self.state.write_text(json.dumps({"@cormex/cli-linux-x64@0.1.17": "sha512-different"}))
        result = self.publish()
        self.assert_failed_without_publish(result)
        self.assertIn("refusing to replace different bytes", result.stderr)

    def test_network_failure_is_not_treated_as_missing_package(self):
        self.assert_failed_without_publish(self.publish("network"))

    def test_authentication_failure_is_not_treated_as_missing_package(self):
        self.assert_failed_without_publish(self.publish("auth"))

    def test_malformed_registry_response_blocks_publication(self):
        self.assert_failed_without_publish(self.publish("malformed"))

    def test_post_publish_mismatch_blocks_main_package(self):
        result = self.publish("post_publish_mismatch")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("published integrity differs", result.stderr)
        publications = [call for call in self.calls() if call["args"][0] == "publish"]
        self.assertEqual([call["package"] for call in publications], ["@cormex/cli-linux-x64@0.1.17"])

    def test_unsafe_pack_filename_blocks_registry_actions(self):
        self.assert_failed_without_publish(self.publish("unsafe_pack"))
        self.assertEqual([call["args"][0] for call in self.calls()], ["pack"])

    def test_missing_platform_archives_blocks_main_package(self):
        for archive in self.dist.iterdir():
            archive.unlink()
        result = self.publish()
        self.assert_failed_without_publish(result)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
