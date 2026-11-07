"""Offline release-contract tests; never build or publish real artifacts."""

import copy
from contextlib import ExitStack
import hashlib
import importlib.util
import json
import shutil
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "release.py"
spec = importlib.util.spec_from_file_location("cormex_release", SCRIPT)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def digest(data):
    return hashlib.sha256(data).hexdigest()


class ReleaseBundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / "bundle"
        self.directory.mkdir()
        self.tag = "v0.1.17"
        self.target = release.TARGETS[0]
        suffix = "zip" if "windows" in self.target else "tar.gz"
        self.cli_file = f"cormex-0.1.17-{self.target}.{suffix}"
        payloads = {self.cli_file: b"offline CLI fixture\n"}
        images = []
        for name in ("cormex", "cormex-web"):
            filename = f"{name}-0.1.17-linux-amd64.docker.tar.gz"
            payloads[filename] = f"offline {name} fixture\n".encode()
            images.append({"name": name, "file": filename,
                           "sha256": digest(payloads[filename]),
                           "image_id": "sha256:" + "a" * 64,
                           "platform": "linux/amd64"})
        self.manifest = {
            "schema_version": 1, "project": "cormex", "tag": self.tag,
            "version": "0.1.17", "commit": "1" * 40, "source": "local",
            "targets": [self.target],
            "missing_targets": sorted(t for t in release.TARGETS if t != self.target),
            "cli": [{"target": self.target, "file": self.cli_file,
                     "sha256": digest(payloads[self.cli_file])}],
            "images": images,
        }
        for filename, data in payloads.items():
            (self.directory / filename).write_bytes(data)
        self.seal()

    def seal(self):
        """Rehash intentionally malformed metadata, so schema checks must catch it."""
        (self.directory / "manifest.json").write_text(
            json.dumps(self.manifest, sort_keys=True) + "\n", encoding="utf-8")
        names = ["manifest.json"] + [entry["file"] for kind in ("cli", "images")
                                      for entry in self.manifest[kind]]
        lines = []
        for filename in sorted(set(names)):
            path = self.directory / filename
            sha = (digest(path.read_bytes())
                   if path.parent == self.directory and path.is_file() else "0" * 64)
            lines.append(f"{sha}  {filename}\n")
        (self.directory / "SHA256SUMS").write_text("".join(lines), encoding="utf-8")

    def verify(self, checksum=None):
        return release.verify_bundle(self.directory, self.tag, checksum)

    def test_valid_partial_bundle_with_pinned_checksum(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        self.assertEqual(self.verify(checksum), self.manifest)

    def test_valid_complete_target_set(self):
        for target in release.TARGETS[1:]:
            suffix = "zip" if "windows" in target else "tar.gz"
            filename = f"cormex-0.1.17-{target}.{suffix}"
            payload = target.encode()
            (self.directory / filename).write_bytes(payload)
            self.manifest["cli"].append({"target": target, "file": filename,
                                         "sha256": digest(payload)})
        self.manifest["targets"] = sorted(release.TARGETS)
        self.manifest["missing_targets"] = []
        self.seal()
        self.assertEqual(self.verify()["missing_targets"], [])

    def test_rejects_version_mismatched_archive_name(self):
        entry = self.manifest["cli"][0]
        filename = entry["file"].replace("0.1.17", "0.1.16")
        (self.directory / entry["file"]).rename(self.directory / filename)
        entry["file"] = filename
        self.seal()
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_rejects_missing_checksum_coverage(self):
        path = self.directory / "SHA256SUMS"
        lines = path.read_text().splitlines(keepends=True)
        path.write_text("".join(line for line in lines if "manifest.json" not in line))
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_github_source_is_supported(self):
        self.manifest["source"] = "github"
        self.seal()
        self.assertEqual(self.verify()["source"], "github")

    def test_rejects_corrupt_payload(self):
        (self.directory / self.cli_file).write_bytes(b"tampered")
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_rejects_wrong_pinned_checksum(self):
        with self.assertRaises(release.ReleaseError):
            self.verify("0" * 64)

    def test_rejects_missing_payload(self):
        (self.directory / self.cli_file).unlink()
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_rejects_extra_file_and_directory(self):
        for is_directory in (False, True):
            with self.subTest(directory=is_directory):
                path = self.directory / "unexpected"
                path.mkdir() if is_directory else path.write_text("extra")
                try:
                    with self.assertRaises(release.ReleaseError):
                        self.verify()
                finally:
                    path.rmdir() if is_directory else path.unlink()

    def test_rejects_symlink_payload(self):
        path = self.directory / self.cli_file
        outside = Path(self.temp.name) / "outside"
        outside.write_bytes(path.read_bytes())
        path.unlink()
        try:
            path.symlink_to(outside)
        except (OSError, NotImplementedError):
            self.skipTest("Creating symlinks is unavailable on this host")
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_rejects_traversal_and_absolute_asset_paths(self):
        original = copy.deepcopy(self.manifest)
        for filename in ("../outside.tar.gz", "/tmp/outside.tar.gz", "nested/file.tar.gz",
                         "..\\outside.zip"):
            with self.subTest(filename=filename):
                self.manifest = copy.deepcopy(original)
                self.manifest["cli"][0]["file"] = filename
                self.seal()
                with self.assertRaises(release.ReleaseError):
                    self.verify()

    def test_rejects_duplicate_checksum_entry(self):
        path = self.directory / "SHA256SUMS"
        content = path.read_text()
        path.write_text(content + content.splitlines(keepends=True)[0])
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_rejects_malformed_checksum_lines(self):
        path = self.directory / "SHA256SUMS"
        original = path.read_text()
        for content in (original.replace("  ", " "), original.rstrip("\n"),
                        original + "\n", original.replace("\n", "\r\n"),
                        "not-a-hash  manifest.json\n"):
            with self.subTest(content=content[:90]):
                path.write_text(content)
                with self.assertRaises(release.ReleaseError):
                    self.verify()

    def test_rejects_manifest_hash_mismatch(self):
        with (self.directory / "manifest.json").open("a") as stream:
            stream.write(" ")
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_rejects_invalid_manifest_fields(self):
        original = copy.deepcopy(self.manifest)
        for key, value in (("schema_version", 2), ("schema_version", True), ("project", "other"),
                           ("tag", "v0.1.18"), ("version", "0.1.18"),
                           ("commit", "A" * 40), ("commit", "abc"),
                           ("source", "untrusted")):
            with self.subTest(key=key, value=value):
                self.manifest = copy.deepcopy(original)
                self.manifest[key] = value
                self.seal()
                with self.assertRaises(release.ReleaseError):
                    self.verify()

    def test_rejects_unsupported_target(self):
        self.manifest["cli"][0]["target"] = "unsupported-target"
        self.manifest["targets"] = ["unsupported-target"]
        self.seal()
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_requires_explicit_consistent_target_coverage(self):
        original = copy.deepcopy(self.manifest)
        for key, value in (("missing_targets", []), ("targets", []),
                           ("targets", [self.target, self.target]),
                           ("missing_targets", list(release.TARGETS))):
            with self.subTest(key=key, value=value):
                self.manifest = copy.deepcopy(original)
                self.manifest[key] = value
                self.seal()
                with self.assertRaises(release.ReleaseError):
                    self.verify()

    def test_rejects_duplicate_cli_entry(self):
        self.manifest["cli"].append(copy.deepcopy(self.manifest["cli"][0]))
        self.seal()
        with self.assertRaises(release.ReleaseError):
            self.verify()

    def test_requires_cli_and_both_images(self):
        original = copy.deepcopy(self.manifest)
        for kind in ("cli", "images"):
            with self.subTest(kind=kind):
                self.manifest = copy.deepcopy(original)
                removed = self.manifest[kind].pop()
                path = self.directory / removed["file"]
                payload = path.read_bytes()
                path.unlink()
                if kind == "cli":
                    self.manifest["targets"] = []
                    self.manifest["missing_targets"] = sorted(release.TARGETS)
                self.seal()
                try:
                    with self.assertRaises(release.ReleaseError):
                        self.verify()
                finally:
                    path.write_bytes(payload)

    def test_rejects_invalid_image_metadata(self):
        original = copy.deepcopy(self.manifest)
        for key, value in (("name", "other"), ("image_id", "sha256:bad"),
                           ("image_id", "sha256:" + "A" * 64),
                           ("platform", "linux/arm64"), ("sha256", "0" * 64)):
            with self.subTest(key=key, value=value):
                self.manifest = copy.deepcopy(original)
                self.manifest["images"][0][key] = value
                self.seal()
                with self.assertRaises(release.ReleaseError):
                    self.verify()

    def test_rejects_duplicate_image_entry(self):
        self.manifest["images"][1] = copy.deepcopy(self.manifest["images"][0])
        self.seal()
        with self.assertRaises(release.ReleaseError):
            self.verify()


class ReleaseOperationTests(unittest.TestCase):
    setUp = ReleaseBundleTests.setUp
    seal = ReleaseBundleTests.seal

    def prepare_sidecars(self):
        root = Path(self.temp.name) / "repo"
        output = root / "dist" / self.tag
        output.parent.mkdir(parents=True)
        self.directory.rename(output)
        self.directory = output
        (output / "manifest.json").unlink()
        (output / "SHA256SUMS").unlink()
        self.context = {key: self.manifest[key]
                        for key in ("tag", "version", "commit", "source")}
        for kind, singular in (("cli", "cli"), ("images", "image")):
            for entry in self.manifest[kind]:
                identifier = entry.get("target", entry.get("name"))
                (output / f"{identifier}.{singular}.json").write_text(
                    json.dumps({**self.context, singular: entry}))
        self.enterContext(mock.patch.object(release, "ROOT", root))
        self.enterContext(mock.patch.object(release, "source_context", return_value=self.context))
        self.enterContext(mock.patch.object(release, "print_summary"))
        self.enterContext(mock.patch.object(release, "run", side_effect=AssertionError("No builds allowed")))
        return output

    def test_create_bundle_consumes_sidecars_and_verifies(self):
        output = self.prepare_sidecars()
        release.create_bundle(self.tag)
        manifest = release.verify_bundle(output, self.tag)
        self.assertEqual(manifest["commit"], self.context["commit"])
        self.assertEqual(manifest["missing_targets"], self.manifest["missing_targets"])
        self.assertFalse(list(output.glob("*.cli.json")), "CLI sidecars must not become release assets")
        self.assertFalse(list(output.glob("*.image.json")), "Image sidecars must not become release assets")

    def test_create_bundle_all_targets_requirement_preserves_inputs(self):
        output = self.prepare_sidecars()
        before = {p.name: p.read_bytes() for p in output.iterdir()}
        with self.assertRaises(release.ReleaseError):
            release.create_bundle(self.tag, require_all_targets=True)
        self.assertEqual({p.name: p.read_bytes() for p in output.iterdir()}, before)

    def test_create_bundle_rejects_mixed_commit_without_consuming_sidecars(self):
        output = self.prepare_sidecars()
        sidecar = next(output.glob("*.image.json"))
        data = json.loads(sidecar.read_text())
        data["commit"] = "2" * 40
        sidecar.write_text(json.dumps(data))
        before = {p.name: p.read_bytes() for p in output.iterdir()}
        with self.assertRaises(release.ReleaseError):
            release.create_bundle(self.tag)
        self.assertEqual({p.name: p.read_bytes() for p in output.iterdir()}, before)

    def test_create_bundle_rejects_unexpected_files_before_consuming_sidecars(self):
        output = self.prepare_sidecars()
        (output / "unexpected.txt").write_text("not an asset")
        before = {p.name: p.read_bytes() for p in output.iterdir()}
        with self.assertRaises(release.ReleaseError):
            release.create_bundle(self.tag)
        self.assertEqual({p.name: p.read_bytes() for p in output.iterdir()}, before)

    def publish_harness(self, stack, *, changed=False, remote_commit=None):
        events = []
        record = {"id": 123, "tag_name": self.tag, "draft": True,
                  "assets": [{"id": i, "name": p.name, "size": p.stat().st_size,
                              "updated_at": "2026-11-07T00:00:00Z"}
                             for i, p in enumerate(sorted(self.directory.iterdir()))]}
        published = False

        def run(*args, **kwargs):
            nonlocal published
            events.append(tuple(str(arg) for arg in args))
            if args[:3] == ("gh", "release", "download"):
                destination = Path(args[args.index("--dir") + 1])
                for path in self.directory.iterdir():
                    shutil.copyfile(path, destination / path.name)
            elif args[:3] == ("gh", "release", "edit"):
                published = True
            else:
                self.fail(f"Unexpected external command: {args}")
            return ""

        def api(path):
            result = copy.deepcopy(record)
            result["draft"] = not published
            if changed:
                result["assets"][0]["size"] += 1
            return result

        stack.enter_context(mock.patch("builtins.print"))
        stack.enter_context(mock.patch.object(release, "ROOT", Path(self.temp.name) / "repo"))
        stack.enter_context(mock.patch.dict(release.os.environ, {"PUBLISH_NPM": "false"}))
        stack.enter_context(mock.patch.object(release, "find_release", return_value=record))
        stack.enter_context(mock.patch.object(release, "api", side_effect=api))
        stack.enter_context(mock.patch.object(release, "remote_commit",
                                              return_value=remote_commit or self.manifest["commit"]))
        stack.enter_context(mock.patch.object(release, "run", side_effect=run))
        stack.enter_context(mock.patch.object(release.subprocess, "run",
                                              side_effect=AssertionError("Real commands forbidden")))
        effects = {}
        for name in ("load_images", "push_images", "publish_npm"):
            effects[name] = stack.enter_context(mock.patch.object(
                release, name, side_effect=lambda *args, name=name: events.append((name,))))
        return events, effects

    def test_publish_corruption_causes_no_docker_or_release_mutation(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        (self.directory / self.cli_file).write_bytes(b"tampered after review")
        with ExitStack() as stack:
            events, effects = self.publish_harness(stack)
            with self.assertRaises(release.ReleaseError):
                release.publish(self.tag, checksum)
            for effect in effects.values():
                effect.assert_not_called()
        self.assertEqual([event[:3] for event in events], [("gh", "release", "download")])

    def test_publish_changed_candidate_stops_before_loading_images(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        with ExitStack() as stack:
            events, effects = self.publish_harness(stack, changed=True)
            with self.assertRaises(release.ReleaseError):
                release.publish(self.tag, checksum)
            for effect in effects.values():
                effect.assert_not_called()
        self.assertNotIn(("gh", "release", "edit"), [event[:3] for event in events])

    def test_publish_moved_tag_stops_before_loading_images(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        with ExitStack() as stack:
            events, effects = self.publish_harness(stack, remote_commit="2" * 40)
            with self.assertRaises(release.ReleaseError):
                release.publish(self.tag, checksum)
            for effect in effects.values():
                effect.assert_not_called()
        self.assertNotIn(("gh", "release", "edit"), [event[:3] for event in events])

    def test_publish_uses_reviewed_bytes_without_rebuild_and_edits_release_last(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        with ExitStack() as stack:
            events, effects = self.publish_harness(stack)
            verifier = stack.enter_context(mock.patch.object(release, "verify_bundle", wraps=release.verify_bundle))
            release.publish(self.tag, checksum)
            self.assertEqual(verifier.call_count, 2, "Verify initial download and re-download before publication")
            for call in verifier.call_args_list:
                self.assertEqual(call.args[1:], (self.tag, checksum))
            for effect in effects.values():
                effect.assert_called_once()
            self.assertEqual(effects["push_images"].call_args.args[0], self.manifest)
        self.assertEqual([event[:3] for event in events], [
            ("gh", "release", "download"), ("load_images",), ("push_images",),
            ("publish_npm",), ("gh", "release", "download"), ("gh", "release", "edit")])
        self.assertFalse(any("cargo" in event or "build" in event for event in events),
                         "Publishing must never rebuild artifacts")

    def test_stage_lookup_failure_never_creates_a_release(self):
        root = Path(self.temp.name) / "repo"
        output = root / "dist" / self.tag
        output.parent.mkdir(parents=True)
        shutil.copytree(self.directory, output)
        with mock.patch.object(release, "ROOT", root), \
                mock.patch.object(release, "remote_commit", return_value=self.manifest["commit"]), \
                mock.patch.object(release, "find_release", side_effect=release.ReleaseError("network failure")), \
                mock.patch.object(release, "run") as runner:
            with self.assertRaises(release.ReleaseError):
                release.stage(self.tag)
            runner.assert_not_called()

    def test_stage_existing_draft_or_published_release_never_overwrites_assets(self):
        root = Path(self.temp.name) / "repo"
        output = root / "dist" / self.tag
        output.parent.mkdir(parents=True)
        shutil.copytree(self.directory, output)
        for draft in (True, False):
            with self.subTest(draft=draft), \
                    mock.patch.object(release, "ROOT", root), \
                    mock.patch.object(release, "remote_commit", return_value=self.manifest["commit"]), \
                    mock.patch.object(release, "find_release", return_value={"draft": draft}), \
                    mock.patch.object(release, "run") as runner:
                with self.assertRaises(release.ReleaseError):
                    release.stage(self.tag)
                runner.assert_not_called()

    def test_publish_already_published_release_causes_no_mutation(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        with ExitStack() as stack:
            events, effects = self.publish_harness(stack)
            stack.enter_context(mock.patch.object(release, "find_release", return_value={"draft": False}))
            with self.assertRaises(release.ReleaseError):
                release.publish(self.tag, checksum)
            for effect in effects.values():
                effect.assert_not_called()
        self.assertEqual(events, [], "Published releases must not be redownloaded or changed")

    def test_push_checks_every_destination_before_first_push(self):
        with mock.patch.object(release, "registry_login"), \
                mock.patch.object(release, "registry_image", side_effect=[None, None, None, "sha256:" + "b" * 64]) as lookup, \
                mock.patch.object(release, "run") as runner:
            with self.assertRaises(release.ReleaseError):
                release.push_images(self.manifest)
            self.assertEqual(lookup.call_count, 4, "Conflict in final registry/image must be checked before mutations")
            runner.assert_not_called()

    def test_push_resumes_matching_images_without_overwriting(self):
        image_id = self.manifest["images"][0]["image_id"]
        with mock.patch.object(release, "registry_login"), \
                mock.patch.object(release, "registry_image", return_value=image_id), \
                mock.patch.object(release, "smoke_image") as smoke, \
                mock.patch.object(release, "run") as runner:
            release.push_images(self.manifest)
            self.assertEqual(smoke.call_count, 4, "Resumed tags are still smoke-tested from the registry")
            runner.assert_not_called()

    def test_registry_image_removes_the_local_tag_before_reading_it(self):
        image_id = "sha256:" + "c" * 64
        probes = []

        def fake_subprocess(args, **kwargs):
            probes.append(args[:3])
            if args[:2] == ["docker", "manifest"]:
                return mock.Mock(returncode=0, stdout=json.dumps(
                    {"schemaVersion": 2, "config": {"digest": "sha256:" + "d" * 64}}), stderr="")
            return mock.Mock(returncode=0, stdout="", stderr="")

        events = []
        with mock.patch.object(release.subprocess, "run", side_effect=fake_subprocess), \
                mock.patch.object(release, "run", side_effect=lambda *args, **kwargs: events.append(args)) as runner, \
                mock.patch.object(release, "image_info", return_value=image_id):
            self.assertEqual(release.registry_image("registry.example/cormex:v0.1.17"), image_id)
        self.assertEqual(probes, [["docker", "manifest", "inspect"], ["docker", "image", "rm"]],
                         "The stale local tag must go before the registry's copy is pulled")
        self.assertEqual(len(events), 1, "One pull, then the identity is read from the daemon")
        self.assertEqual(runner.call_count, 1)
        self.assertEqual(runner.call_args.args[:2], ("docker", "pull"),
                         "The registry's copy must be fetched again, not read from a stale local tag")

    def test_registry_image_fails_closed_on_ambiguous_errors(self):
        for message in ("unauthorized", "permission denied", "connection timed out"):
            with self.subTest(message=message), \
                    mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                        returncode=1, stdout="", stderr=message)):
                with self.assertRaises(release.ReleaseError):
                    release.registry_image("example/cormex:v0.1.17")

    def test_registry_image_reports_a_missing_tag_as_absent(self):
        with mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                returncode=1, stdout="", stderr="no such manifest: example/cormex:v0.1.17")):
            self.assertIsNone(release.registry_image("example/cormex:v0.1.17"))

    def test_failed_push_never_publishes_release(self):
        checksum = digest((self.directory / "SHA256SUMS").read_bytes())
        with ExitStack() as stack:
            events, effects = self.publish_harness(stack)
            effects["push_images"].side_effect = release.ReleaseError("registry failure")
            with self.assertRaises(release.ReleaseError):
                release.publish(self.tag, checksum)
            effects["publish_npm"].assert_not_called()
        self.assertNotIn(("gh", "release", "edit"), [event[:3] for event in events])


class ReleaseInputTests(unittest.TestCase):
    def test_valid_tag(self):
        self.assertEqual(release.validate_tag("v0.1.17"), "0.1.17")

    def test_rejects_injection_and_non_release_tags(self):
        for tag in ("0.1.17", "v1.2", "v1.2.3-rc1", "v1.2.3+build", "../v1.2.3",
                    "v1.2.3;touch /tmp/injected", "v1.2.3\n", "--help", ""):
            with self.subTest(tag=tag), self.assertRaises(release.ReleaseError):
                release.validate_tag(tag)

    def test_valid_checksum(self):
        self.assertEqual(release.validate_checksum("a" * 64), "a" * 64)

    def test_rejects_invalid_checksum(self):
        for checksum in ("A" * 64, "a" * 63, "a" * 65, "g" * 64, "a" * 64 + "\n", ""):
            with self.subTest(checksum=checksum), self.assertRaises(release.ReleaseError):
                release.validate_checksum(checksum)


class LocalBuildTargetTests(unittest.TestCase):
    def test_builder_per_target(self):
        self.assertEqual(release.cli_builder("x86_64-unknown-linux-musl"), (["zigbuild"], "cargo-zigbuild"))
        self.assertEqual(release.cli_builder("aarch64-pc-windows-msvc"), (["xwin", "build"], "cargo-xwin"))
        self.assertEqual(release.cli_builder("x86_64-apple-darwin"), (["build"], None))
        self.assertEqual(release.cli_builder("x86_64-pc-windows-msvc", "win32"), (["build"], None),
                         "the Windows Actions runner builds MSVC natively and has no cargo-xwin")
        self.assertEqual(release.cli_builder("x86_64-pc-windows-msvc", "linux"), (["xwin", "build"], "cargo-xwin"))

    def test_default_local_targets_stay_x86_64(self):
        for platform, expected in release.DEFAULT_LOCAL_TARGETS.items():
            with self.subTest(platform=platform), \
                    mock.patch.object(release.sys, "platform", platform), \
                    mock.patch.dict(release.os.environ, {"CORMEX_CLI_TARGETS": ""}):
                targets = release.local_targets()
                self.assertEqual(targets, expected)
                self.assertFalse([t for t in targets if t.startswith("aarch64")],
                                 "arm targets must stay off until CORMEX_CLI_TARGETS enables them")
        self.assertEqual(release.DEFAULT_LOCAL_TARGETS["linux"],
                         ["x86_64-unknown-linux-musl", "x86_64-pc-windows-msvc"],
                         "a Linux local build must produce the Windows x86_64 CLI")

    def test_env_override_enables_the_other_targets(self):
        wanted = "aarch64-unknown-linux-musl, x86_64-apple-darwin"
        with mock.patch.dict(release.os.environ, {"CORMEX_CLI_TARGETS": wanted}):
            self.assertEqual(release.local_targets(),
                             ["aarch64-unknown-linux-musl", "x86_64-apple-darwin"])

    def test_env_override_rejects_unknown_and_duplicate_targets(self):
        for value in ("aarch64-unknown-linux", "x86_64-unknown-linux-musl,x86_64-unknown-linux-musl"):
            with self.subTest(value=value), \
                    mock.patch.dict(release.os.environ, {"CORMEX_CLI_TARGETS": value}):
                with self.assertRaises(release.ReleaseError):
                    release.local_targets()


if __name__ == "__main__":
    unittest.main()
