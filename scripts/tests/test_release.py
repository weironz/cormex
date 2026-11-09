"""Offline release-contract tests; never build or publish real artifacts."""

import copy
from contextlib import ExitStack
import hashlib
import importlib.util
import io
import json
import tarfile
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
                release, name, side_effect=lambda *args, name=name, **kwargs: events.append((name,))))
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

    def loaded_images(self):
        return {name: {"image_id": "sha256:" + "c" * 64, "content_sha256": "a" * 64}
                for name in release.IMAGES}

    def test_push_checks_every_destination_before_first_push(self):
        conflict = {"content_sha256": "b" * 64}
        with mock.patch.object(release, "registry_login"), \
                mock.patch.object(release, "daemon_image_content", return_value="a" * 64), \
                mock.patch.object(release, "registry_image", side_effect=[None, None, None, conflict]) as lookup, \
                mock.patch.object(release, "run") as runner:
            with self.assertRaises(release.ReleaseError):
                release.push_images(self.manifest, self.loaded_images())
            self.assertEqual(lookup.call_count, 4, "Conflict in final registry/image must be checked before mutations")
            runner.assert_not_called()

    def test_push_resumes_matching_content_without_overwriting_and_records_digests(self):
        remote = {"digest": "sha256:" + "d" * 64, "content_sha256": "a" * 64,
                  "reference": "example/cormex@sha256:" + "d" * 64}
        with mock.patch.object(release, "registry_login"), \
                mock.patch.object(release, "daemon_image_content", return_value="a" * 64), \
                mock.patch.object(release, "registry_image", return_value=remote), \
                mock.patch.object(release, "smoke_image") as smoke, \
                mock.patch.object(release, "run") as runner:
            results = release.push_images(self.manifest, self.loaded_images())
            self.assertEqual(smoke.call_count, 4, "Resumed tags are still smoke-tested from the registry")
            self.assertEqual([item["digest"] for item in results], [remote["digest"]] * 4)
            runner.assert_not_called()

    def test_registry_image_pulls_immutable_digest_and_compares_portable_content(self):
        identity = "sha256:" + "d" * 64
        descriptor = {"schemaVersion": 2, "digest": identity, "mediaType": "application/vnd.oci.image.manifest.v1+json"}
        reference = "registry.example/cormex:v0.1.17"
        pinned = "registry.example/cormex@" + identity
        with mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                returncode=0, stdout=json.dumps(descriptor), stderr="")) as probe, \
                mock.patch.object(release, "run") as runner, \
                mock.patch.object(release, "daemon_image_content", return_value="a" * 64) as content:
            self.assertEqual(release.registry_image(reference), {
                "digest": identity, "content_sha256": "a" * 64, "reference": pinned})
        self.assertEqual(probe.call_count, 1, "Read registry descriptor once, then pull its immutable digest")
        runner.assert_called_once_with("docker", "pull", "--platform", "linux/amd64", pinned, capture=True)
        content.assert_called_once_with(pinned)

    def test_registry_image_fails_closed_on_ambiguous_errors(self):
        for message in ("unauthorized", "permission denied", "connection timed out"):
            with self.subTest(message=message), \
                    mock.patch.object(release.time, "sleep") as sleep, \
                    mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                        returncode=1, stdout="", stderr=message)) as lookup:
                with self.assertRaises(release.ReleaseError):
                    release.registry_image("example/cormex:v0.1.17")
                self.assertEqual(lookup.call_count, len(release.REGISTRY_LOOKUP_DELAYS) + 1,
                                 "an ambiguous answer must be asked again before failing closed")
                self.assertEqual(sleep.call_count, len(release.REGISTRY_LOOKUP_DELAYS))

    def test_registry_image_retries_a_spurious_docker_hub_401(self):
        # Docker Hub's real answer for a missing tag, seen intermittently during
        # the v0.1.20 publication, followed by the 404 the next lookup gets
        spurious = mock.Mock(returncode=1, stdout="", stderr=(
            "ERROR: unexpected status from HEAD request to https://registry-1.docker.io/v2/"
            "example/cormex/manifests/v0.1.17: 401 Unauthorized\n"))
        missing = mock.Mock(returncode=1, stdout="", stderr="ERROR: docker.io/example/cormex:v0.1.17: not found\n")
        with mock.patch.object(release.time, "sleep"), \
                mock.patch.object(release.subprocess, "run", side_effect=[spurious, missing]) as lookup:
            self.assertIsNone(release.registry_image("example/cormex:v0.1.17"))
        self.assertEqual(lookup.call_count, 2)

    def test_partial_push_records_verified_digest_before_later_failure(self):
        remote = {"digest": "sha256:" + "d" * 64, "content_sha256": "a" * 64,
                  "reference": "example/cormex@sha256:" + "d" * 64}
        callback = mock.Mock()
        with mock.patch.object(release, "registry_login"), \
                mock.patch.object(release, "daemon_image_content", return_value="a" * 64), \
                mock.patch.object(release, "registry_image", side_effect=[None] * 5 + [remote, release.ReleaseError("offline")]), \
                mock.patch.object(release, "smoke_image"), mock.patch.object(release, "run"):
            with self.assertRaisesRegex(release.ReleaseError, "offline"):
                release.push_images(self.manifest, self.loaded_images(), on_image=callback)
        callback.assert_called_once()
        self.assertEqual(callback.call_args.args[0]["digest"], remote["digest"])

    def test_registry_single_platform_index_is_accepted_but_other_platforms_refused(self):
        identity = "sha256:" + "d" * 64
        for platforms, accepted in (([{"os": "linux", "architecture": "amd64"}], True),
                                    ([{"os": "linux", "architecture": "arm64"}], False),
                                    ([{"os": "linux", "architecture": "amd64"}] * 2, False)):
            descriptor = {"digest": identity, "manifests": [{"platform": platform} for platform in platforms]}
            with self.subTest(platforms=platforms), \
                    mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                        returncode=0, stdout=json.dumps(descriptor), stderr="")), \
                    mock.patch.object(release, "run") as runner, \
                    mock.patch.object(release, "daemon_image_content", return_value="a" * 64):
                if accepted:
                    self.assertEqual(release.registry_image("example/cormex:v0.1.17")["digest"], identity)
                else:
                    with self.assertRaises(release.ReleaseError):
                        release.registry_image("example/cormex:v0.1.17")
                    runner.assert_not_called()

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


def docker_config():
    return {"os": "linux", "architecture": "amd64", "created": "2026-11-08T00:00:00Z",
            "config": {"Env": ["PATH=/bin", "MODE=release"], "Cmd": ["server"],
                       "Labels": {"empty": ""}},
            "rootfs": {"type": "layers", "diff_ids": ["sha256:" + "e" * 64]}}


def docker_archive(path, config, *, containerd=False, tags=None, config_name=None):
    data = json.dumps(config, indent=2 if containerd else None).encode()
    config_name = config_name or (("blobs/sha256/" if containerd else "") + digest(data) + ("" if containerd else ".json"))
    manifest = [{"Config": config_name, "RepoTags": tags, "Layers": ["layer.tar"]}]
    with tarfile.open(path, "w:gz") as archive:
        for name, content in (("manifest.json", json.dumps(manifest).encode()), (config_name, data)):
            member = tarfile.TarInfo(name)
            member.size = len(content)
            archive.addfile(member, io.BytesIO(content))


class PortableImageTests(unittest.TestCase):
    setUp = ReleaseBundleTests.setUp
    seal = ReleaseBundleTests.seal

    def test_classic_and_containerd_archives_and_inspect_share_runtime_identity(self):
        classic, containerd = self.directory / "classic.tar.gz", self.directory / "containerd.tar.gz"
        spec = docker_config()
        rewritten = copy.deepcopy(spec)
        rewritten.update(created="different", docker_version="29.0.0", history=[{"created_by": "builder"}])
        rewritten["config"].update(ArgsEscaped=False, WorkingDir="", Entrypoint=None)
        docker_archive(classic, spec)
        docker_archive(containerd, rewritten, containerd=True)
        self.assertNotEqual(digest(classic.read_bytes()), digest(containerd.read_bytes()))
        expected = release.archive_content_identity(classic)
        self.assertEqual(expected, release.archive_content_identity(containerd))
        self.assertEqual(expected, release.image_content_identity({
            "Id": "sha256:" + "f" * 64, "Os": "linux", "Architecture": "amd64",
            "Config": rewritten["config"], "RootFS": {"Type": "layers", "Layers": spec["rootfs"]["diff_ids"]}}))

    def test_runtime_changes_never_match(self):
        original = docker_config()
        for changed in ("env", "command", "label", "layer", "layer-order", "user", "unknown-field"):
            with self.subTest(changed=changed):
                spec = copy.deepcopy(original)
                if changed == "env":
                    spec["config"]["Env"].reverse()
                elif changed == "command":
                    spec["config"]["Cmd"] = ["another-server"]
                elif changed == "label":
                    spec["config"]["Labels"]["empty"] = "different"
                elif changed == "layer":
                    spec["rootfs"]["diff_ids"] = ["sha256:" + "f" * 64]
                elif changed == "layer-order":
                    spec["rootfs"]["diff_ids"].append("sha256:" + "f" * 64)
                elif changed == "user":
                    spec["config"]["User"] = "1000"
                else:
                    spec["config"]["FutureRuntimeSetting"] = "enabled"
                self.assertNotEqual(release.portable_image_content(original), release.portable_image_content(spec))

    def test_legacy_schema1_load_uses_returned_daemon_id_not_builder_id(self):
        loaded_id = "sha256:" + "b" * 64
        expected = release.portable_image_content(docker_config())
        for containerd in (False, True):
            with self.subTest(containerd=containerd):
                for entry in self.manifest["images"]:
                    docker_archive(self.directory / entry["file"], docker_config(), containerd=containerd)
                    entry["sha256"] = digest((self.directory / entry["file"]).read_bytes())
                self.seal()
                before = {path.name: path.read_bytes() for path in self.directory.iterdir()}
                message = ("Loaded image: " if containerd else "Loaded image ID: ") + loaded_id
                with mock.patch.object(release, "run", return_value=message), \
                        mock.patch.object(release, "image_info", return_value=loaded_id) as info, \
                        mock.patch.object(release, "daemon_image_content", return_value=expected), \
                        mock.patch.object(release, "smoke_image") as smoke:
                    result = release.load_images(self.directory, self.manifest)
                self.assertEqual(result["cormex"]["image_id"], loaded_id)
                self.assertTrue(all(call.args == (loaded_id,) for call in info.call_args_list))
                self.assertTrue(all(call.args[1] == loaded_id for call in smoke.call_args_list))
                self.assertEqual(before, {path.name: path.read_bytes() for path in self.directory.iterdir()},
                                 "Legacy candidate bytes must remain untouched")

    def test_loaded_wrong_content_fails_before_smoke(self):
        entry = self.manifest["images"][0]
        docker_archive(self.directory / entry["file"], docker_config())
        entry["sha256"] = digest((self.directory / entry["file"]).read_bytes())
        with mock.patch.object(release, "run", return_value="Loaded image ID: sha256:" + "b" * 64), \
                mock.patch.object(release, "image_info", return_value="sha256:" + "b" * 64), \
                mock.patch.object(release, "daemon_image_content", return_value="0" * 64), \
                mock.patch.object(release, "smoke_image") as smoke:
            with self.assertRaisesRegex(release.ReleaseError, "content mismatch"):
                release.load_images(self.directory, self.manifest)
            smoke.assert_not_called()

    def test_rejects_tagged_or_unsafe_archives_before_load(self):
        for kwargs in ({"tags": ["example/cormex:latest"]}, {"config_name": "../evil.json"}):
            with self.subTest(kwargs=kwargs):
                path = self.directory / "untrusted.tar.gz"
                docker_archive(path, docker_config(), **kwargs)
                with self.assertRaises(ValueError):
                    release.archive_content_identity(path)


def development_postgres(database="cormex_dev", port="5432"):
    return [{"State": {"Running": True}, "Config": {
        "Labels": {"com.docker.compose.project": release.os.environ.get("COMPOSE_PROJECT_NAME", "cormex"),
                   "com.docker.compose.service": "postgres",
                   "com.docker.compose.project.working_dir": str(release.ROOT),
                   "com.docker.compose.project.config_files": str(release.ROOT / "docker-compose.yml")},
        "Env": ["POSTGRES_DB=" + database, "POSTGRES_USER=dev", "POSTGRES_PASSWORD=never-print-this"]},
        "NetworkSettings": {"Ports": {"5432/tcp": [{"HostIp": "127.0.0.1", "HostPort": port}]}}}]


class DoctorTests(unittest.TestCase):
    def test_flutter_fallback_when_path_does_not_include_sdk(self):
        with mock.patch.object(release.shutil, "which", return_value=None), \
                mock.patch.object(Path, "is_file", return_value=True):
            self.assertEqual(release.flutter_executable(), "/opt/flutter/bin/flutter")

    def test_database_refuses_remote_and_overrides_without_printing_credentials(self):
        for url in ("postgres://user:secret@db.example/dev", "postgres://u:p@localhost/production",
                    "postgres://u:p@localhost/dev?host=remote", "postgres://u:p@localhost/dev?options=x",
                    "postgres://u:p@localhost/dev#extra", "postgres://u:p@localhost/%2Fetc"):
            with self.subTest(url=url), self.assertRaises(release.ReleaseError) as error:
                release.local_database_environment(url)
            self.assertNotIn("secret", str(error.exception))
        env = release.local_database_environment("postgres://dev:p%40ss@127.0.0.1:15432/cormex_dev")
        self.assertEqual(env["PGPASSWORD"], "p@ss")
        self.assertEqual(env["PGHOSTADDR"], "127.0.0.1", "Inherited PGHOSTADDR must not redirect tests to production")
        self.assertEqual(env["PGOPTIONS"], "-c default_transaction_read_only=on")
        self.assertIsNone(env["PGSERVICE"], "libpq service settings must be removed rather than set to an empty string")

    def test_probe_and_run_remove_inherited_libpq_services_from_child_environment(self):
        overrides = release.local_database_environment("postgres://dev:password@localhost/cormex_dev")
        inherited = {"PGSERVICE": "redirect-to-another-database", "PGSERVICEFILE": "/unused/service-file"}
        with mock.patch.dict(release.os.environ, inherited), \
                mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                    returncode=0, stdout="1", stderr="")) as runner:
            self.assertEqual(release.probe("psql", "--command", "SELECT 1", env=overrides), (True, "1"))
            release.run("psql", "--command", "SELECT 1", env=dict(overrides, PGOPTIONS=""), capture=True)
            for call in runner.call_args_list:
                child = call.kwargs["env"]
                self.assertNotIn("PGSERVICE", child, "Even an empty PGSERVICE triggers a libpq service lookup")
                self.assertNotIn("PGSERVICEFILE", child)
                self.assertEqual(child["PGHOSTADDR"], "127.0.0.1")
            self.assertEqual(release.os.environ["PGSERVICE"], inherited["PGSERVICE"], "Child sanitization must not change the caller environment")

    def test_deployment_ssh_defaults_are_allowed_but_explicit_missing_files_fail(self):
        with mock.patch.object(release, "probe", return_value=(False, "unavailable")), \
                mock.patch.object(release, "validate_versions", return_value={"version": "0.1.19"}):
            for value, expected in (("", True), ("/nonexistent-release-test/key", False)):
                with self.subTest(value=value), mock.patch.dict(release.os.environ, {
                        "DEPLOY_SSH_KEY": value, "DEPLOY_KNOWN_HOSTS": value}):
                    report = release.doctor(deployment=True, profile="source", emit=False)
                    checks = {check["name"]: check for check in report["checks"]}
                    for name in ("DEPLOY_SSH_KEY", "DEPLOY_KNOWN_HOSTS"):
                        self.assertEqual(checks[name]["ok"], expected)
                        if expected:
                            self.assertIn("defaults", checks[name]["detail"])

    def test_flutter_version_must_match_the_pinned_sdk(self):
        for payload, expected in (({"frameworkVersion": "3.44.1"}, True),
                                  ({"frameworkVersion": "3.44.0"}, False),
                                  ({"frameworkVersion": "3.45.0"}, False), ({}, False), ([], False)):
            def probe(*args, **kwargs):
                return (True, json.dumps(payload)) if "--machine" in args else (False, "unavailable")
            with self.subTest(payload=payload), mock.patch.object(release, "probe", side_effect=probe), \
                    mock.patch.object(release, "validate_versions", return_value={"version": "0.1.19"}), \
                    mock.patch.object(release, "database_url", return_value="postgres://localhost/cormex_dev"), \
                    mock.patch.object(release, "local_targets", return_value=[]), \
                    mock.patch.object(release, "flutter_executable", return_value="/opt/flutter/bin/flutter"):
                report = release.doctor(emit=False)
                check = next(check for check in report["checks"] if check["name"] == "flutter")
                self.assertEqual(check["ok"], expected)
                self.assertEqual(check["detail"]["required_version"], "3.44.1")

    def test_failed_probe_redacts_raw_errors_and_is_bounded(self):
        with mock.patch.object(release.subprocess, "run", return_value=mock.Mock(
                returncode=1, stdout="", stderr="password=do-not-print")) as runner:
            ok, message = release.probe("tool", "--version")
        self.assertFalse(ok)
        self.assertNotIn("do-not-print", message)
        self.assertEqual(runner.call_args.kwargs["timeout"], 25)

    def test_doctor_is_tag_independent_and_rejects_wrong_daemon_despite_reachable_database(self):
        calls = []
        stack_present = [True]
        def probe(*args, **kwargs):
            calls.append(args)
            if args[:3] == ("docker", "container", "inspect"):
                return (True, json.dumps(development_postgres())) if stack_present[0] else (False, "unavailable")
            if args[:2] == ("docker", "info"):
                return True, json.dumps({"ID": "engine-123", "Name": "engine", "ServerVersion": "29.0.0", "Driver": "overlayfs"})
            if args[:3] == ("docker", "buildx", "inspect"):
                return True, "Name: selected-builder\nDriver: docker\nEndpoint: tcp://user:password@localhost\nStatus: running\nPlatforms: linux/amd64"
            if "--machine" in args:
                return True, json.dumps({"frameworkVersion": "3.44.1", "frameworkRevision": "flutter-sha"})
            if args[0] in ("psql", "pg_dump", "pg_restore") and "--version" in args:
                return True, args[0] + " (PostgreSQL) 17.6"
            if "SELECT 1" in args:
                self.assertEqual(kwargs["env"]["PGHOSTADDR"], "127.0.0.1")
                return True, "1"
            return True, "tool 1.0"
        with mock.patch.object(release, "probe", side_effect=probe), \
                mock.patch.object(release, "validate_versions", return_value={"version": "0.1.19"}), \
                mock.patch.object(release, "database_url", return_value="postgres://dev:supersecret@localhost/cormex_dev"), \
                mock.patch.object(release, "flutter_executable", return_value="/opt/flutter/bin/flutter"), \
                mock.patch.object(release, "source_context", side_effect=AssertionError("doctor must not need a tag")), \
                mock.patch.object(release.shutil, "which", side_effect=lambda name: name), \
                mock.patch.object(release.shutil, "disk_usage", return_value=mock.Mock(free=50 * 1024**3)), \
                mock.patch.object(release, "local_targets", return_value=[]):
            report = release.doctor(emit=False)
            stack_present[0] = False
            wrong_daemon = release.doctor(emit=False)
        self.assertTrue(report["ok"], report)
        self.assertFalse(wrong_daemon["ok"], "A reachable host database cannot prove the selected daemon owns this development stack")
        wrong_checks = {check["name"]: check for check in wrong_daemon["checks"]}
        self.assertTrue(wrong_checks["docker-engine"]["ok"])
        self.assertTrue(wrong_checks["development-database"]["ok"])
        self.assertFalse(wrong_checks["development-stack"]["ok"])
        self.assertIn("correct Docker context", wrong_checks["development-stack"]["detail"])
        output = json.dumps(report)
        self.assertNotIn("supersecret", output)
        self.assertNotIn("user:password", output)
        self.assertFalse(any("--bootstrap" in args or "use" in args or "login" in args for args in calls))
        self.assertTrue(any(args[0] == "/opt/flutter/bin/flutter" for args in calls))

    def test_development_stack_provenance_checks_checkout_project_database_and_port(self):
        environment = release.local_database_environment("postgres://dev:secret@localhost/cormex_dev")
        release.validate_development_stack(development_postgres(), environment)
        for mismatch in ("checkout", "project", "compose-file", "service", "database", "port", "binding", "stopped"):
            inspected = development_postgres()
            container = inspected[0]
            labels = container["Config"]["Labels"]
            if mismatch == "checkout":
                labels["com.docker.compose.project.working_dir"] = "/another-checkout"
            elif mismatch == "project":
                labels["com.docker.compose.project"] = "another-project"
            elif mismatch == "compose-file":
                labels["com.docker.compose.project.config_files"] = str(release.ROOT / "deploy/docker-compose.yml")
            elif mismatch == "service":
                labels["com.docker.compose.service"] = "another-service"
            elif mismatch == "database":
                container["Config"]["Env"][0] = "POSTGRES_DB=another_database"
            elif mismatch == "port":
                container["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostPort"] = "6543"
            elif mismatch == "binding":
                container["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostIp"] = "192.0.2.1"
            else:
                container["State"]["Running"] = False
            with self.subTest(mismatch=mismatch), self.assertRaises(release.ReleaseError) as error:
                release.validate_development_stack(inspected, environment)
            self.assertNotIn("never-print-this", str(error.exception))

    def test_source_profile_skips_local_development_stack_provenance(self):
        with mock.patch.object(release, "probe", return_value=(False, "unavailable")), \
                mock.patch.object(release, "validate_versions", return_value={"version": "0.1.19"}), \
                mock.patch.object(release, "development_stack_check") as check:
            report = release.doctor(profile="source", emit=False)
            check.assert_not_called()
            self.assertNotIn("development-stack", [entry["name"] for entry in report["checks"]])

    def test_preflight_required_failure_stops_before_metadata(self):
        with mock.patch.object(release, "source_context", return_value={"commit": "a" * 40}), \
                mock.patch.object(release, "doctor", return_value={"checks": [
                    {"name": "development-database", "required": True, "ok": False}]}), \
                mock.patch.object(release, "run") as runner, \
                mock.patch.object(release, "remote_commit") as remote:
            with self.assertRaisesRegex(release.ReleaseError, "development-database"):
                release.preflight("v0.1.19", "local")
            runner.assert_not_called()
            remote.assert_not_called()


class VersionPreparationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "app").mkdir()
        (self.root / "pkg").mkdir()
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["pkg"]\n[workspace.package]\nversion = "0.1.19"\nedition = "2024"\n')
        (self.root / "pkg/Cargo.toml").write_text('[package]\nname = "sample"\nversion.workspace = true\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "sample"\nversion = "0.1.19"\ndependencies = ["external 1.0.0"]\n\n[[package]]\nname = "external"\nversion = "1.0.0"\nsource = "registry+https://example.invalid"\nchecksum = "keep"\n')
        (self.root / "app/pubspec.yaml").write_text('name: app\nversion: 0.1.19+8\n')
        (self.root / "CHANGELOG.md").write_text('# Changelog\n\n## Unreleased\n\n- Keep these release notes.\n\n## 0.1.19 — 2026-11-08\n\nPrevious release.\n')
        self.enterContext(mock.patch.object(release, "ROOT", self.root))

    def metadata(self, *args, **kwargs):
        self.assertEqual(args[:2], ("cargo", "metadata"), "Preparation must never commit, tag, push or generate-lockfile")
        if "--locked" not in args:
            path = self.root / "Cargo.lock"
            path.write_text(path.read_text().replace('version = "0.1.19"', 'version = "0.1.20"'))
        return "{}"

    def test_preparation_updates_all_versions_and_moves_unreleased_notes(self):
        with mock.patch.object(release, "run", side_effect=self.metadata) as runner, mock.patch("builtins.print"):
            release.prepare_version("v0.1.20", release_date="2026-11-09")
        self.assertEqual(release.validate_versions(), {"version": "0.1.20", "build_number": 9})
        text = (self.root / "CHANGELOG.md").read_text()
        self.assertIn("## Unreleased\n\n## 0.1.20 — 2026-11-09\n\n- Keep these release notes.", text)
        self.assertIn('version = "1.0.0"', (self.root / "Cargo.lock").read_text())
        self.assertEqual(runner.call_count, 2)

    @unittest.skipUnless(shutil.which("cargo"), "Cargo is needed for the offline metadata integration fixture")
    def test_real_cargo_metadata_updates_only_workspace_versions_offline(self):
        (self.root / "pkg/src").mkdir()
        (self.root / "pkg/src/lib.rs").write_text("pub fn fixture() {}\n")
        (self.root / "dependency/src").mkdir(parents=True)
        (self.root / "dependency/src/lib.rs").write_text("pub fn stable() {}\n")
        (self.root / "dependency/Cargo.toml").write_text('[package]\nname = "stable-dependency"\nversion = "1.0.0"\n')
        path = self.root / "Cargo.toml"
        path.write_text(path.read_text().replace('members = ["pkg"]', 'members = ["pkg"]\nexclude = ["dependency"]'))
        path = self.root / "pkg/Cargo.toml"
        path.write_text(path.read_text() + '\n[dependencies]\nstable-dependency = { path = "../dependency" }\n')
        release.run("cargo", "metadata", "--offline", "--format-version", "1", capture=True)
        with mock.patch("builtins.print"):
            release.prepare_version("v0.1.20")
        locked = release.tomllib.loads((self.root / "Cargo.lock").read_text())["package"]
        self.assertEqual({entry["name"]: entry["version"] for entry in locked},
                         {"sample": "0.1.20", "stable-dependency": "1.0.0"})

    def test_dependency_churn_rolls_back_every_version_file(self):
        originals = {path: path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        def churn(*args, **kwargs):
            self.metadata(*args, **kwargs)
            path = self.root / "Cargo.lock"
            path.write_text(path.read_text().replace('version = "1.0.0"', 'version = "2.0.0"'))
        with mock.patch.object(release, "run", side_effect=churn):
            with self.assertRaisesRegex(release.ReleaseError, "changed dependencies"):
                release.prepare_version("v0.1.20")
        self.assertEqual(originals, {path: path.read_bytes() for path in originals})

    def test_windows_checkout_line_endings_are_supported(self):
        for name in ("Cargo.toml", "app/pubspec.yaml", "CHANGELOG.md"):
            path = self.root / name
            path.write_bytes(path.read_bytes().replace(b"\n", b"\r\n"))
        with mock.patch.object(release, "run", side_effect=self.metadata), mock.patch("builtins.print"):
            release.prepare_version("v0.1.20")
        self.assertEqual(release.validate_versions()["version"], "0.1.20")

    def test_metadata_failure_restores_original_bytes(self):
        originals = {path: path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        with mock.patch.object(release, "run", side_effect=release.ReleaseError("offline cache missing")):
            with self.assertRaises(release.ReleaseError):
                release.prepare_version("v0.1.20")
        self.assertEqual(originals, {path: path.read_bytes() for path in originals})

    def test_preflight_rejects_drift_in_each_version_file(self):
        for name, old, new in (("Cargo.lock", 'version = "0.1.19"', 'version = "0.1.18"'),
                               ("app/pubspec.yaml", "0.1.19+8", "0.1.18+8"),
                               ("CHANGELOG.md", "## 0.1.19", "## 0.1.18")):
            path = self.root / name
            original = path.read_text()
            path.write_text(original.replace(old, new))
            with self.subTest(name=name), self.assertRaises(release.ReleaseError):
                release.validate_versions("0.1.19")
            path.write_text(original)

    def test_rejects_nonincreasing_version_or_build_number_before_writes(self):
        with mock.patch.object(release, "run") as runner:
            for version, number in (("v0.1.19", None), ("v0.1.18", None), ("v0.1.20", 8)):
                with self.subTest(version=version, number=number), self.assertRaises(release.ReleaseError):
                    release.prepare_version(version, number)
            runner.assert_not_called()


class ResumeBuildTests(unittest.TestCase):
    setUp = ReleaseBundleTests.setUp
    seal = ReleaseBundleTests.seal

    def harness(self, stack, fail_web=True):
        root = Path(self.temp.name) / "repo"
        root.mkdir(exist_ok=True)
        context = {key: self.manifest[key] for key in ("tag", "version", "commit", "source")}
        events = []
        def cli(tag, target):
            events.append("cli")
            output = release.output_dir(tag)
            output.mkdir(parents=True, exist_ok=True)
            entry = self.manifest["cli"][0]
            shutil.copyfile(self.directory / entry["file"], output / entry["file"])
            release.write_json(output / f"{target}.cli.json", {**context, "cli": entry})
        failures = [fail_web]
        def images(tag, names):
            name = names[0]
            events.append(name)
            if name == "cormex-web" and failures[0]:
                failures[0] = False
                raise release.ReleaseError("simulated image build failure")
            output = release.output_dir(tag)
            entry = next(entry for entry in self.manifest["images"] if entry["name"] == name)
            shutil.copyfile(self.directory / entry["file"], output / entry["file"])
            release.write_json(output / f"{name}.image.json", {**context, "image": entry})
        stack.enter_context(mock.patch.object(release, "ROOT", root))
        # Starting the builder is not a build step: kept out of `events`, which
        # the resume tests read as "work done", and checked on its own below
        stack.enter_context(mock.patch.object(release, "start_builder"))
        stack.enter_context(mock.patch.object(release, "preflight", return_value={"checks": []}))
        stack.enter_context(mock.patch.object(release, "doctor", return_value={"checks": []}))
        stack.enter_context(mock.patch.object(release, "build_runtime_identity", return_value={"tools": "fixed"}))
        stack.enter_context(mock.patch.object(release, "source_context", return_value=context))
        stack.enter_context(mock.patch.object(release, "local_targets", return_value=[self.target]))
        stack.enter_context(mock.patch.object(release, "flutter_executable", return_value=None))
        stack.enter_context(mock.patch.object(release, "database_url", return_value="postgres://localhost/dev"))
        stack.enter_context(mock.patch.object(release, "build_cli", side_effect=cli))
        stack.enter_context(mock.patch.object(release, "build_images", side_effect=images))
        stack.enter_context(mock.patch.object(release, "run", side_effect=lambda *args, **kwargs: events.append(args)))
        stack.enter_context(mock.patch("builtins.print"))
        return root, events

    def test_resume_reuses_only_verified_artifacts_and_always_reruns_ci(self):
        with ExitStack() as stack:
            root, events = self.harness(stack)
            with self.assertRaisesRegex(release.ReleaseError, "simulated"):
                release.build_local(self.tag)
            receipt = release.read_json(root / "dist" / f".{self.tag}.build-receipt.json")
            self.assertEqual(receipt["stages"][-1]["status"], "failed")
            self.assertIn("duration_seconds", receipt["stages"][-1])
            release.build_local(self.tag, resume=True)
            release.build_local(self.tag, resume=True)
            self.assertEqual(events.count(("just", "ci-full")), 3, "Every resume must rerun mandatory checks")
            self.assertEqual(events.count("cli"), 1)
            self.assertEqual(events.count("cormex"), 1)
            self.assertEqual(events.count("cormex-web"), 2)
            release.verify_bundle(root / "dist" / self.tag, self.tag)

    def test_builder_is_started_before_its_identity_is_recorded(self):
        # A stopped builder reports no BuildKit version until the first image
        # build starts it; recording the identity before that made v0.1.20's
        # end-of-build check report a runtime change that never happened
        with ExitStack() as stack:
            _, events = self.harness(stack, fail_web=False)
            order = []
            stack.enter_context(mock.patch.object(release, "start_builder", side_effect=lambda: order.append("start-builder")))
            stack.enter_context(mock.patch.object(release, "build_runtime_identity",
                                                  side_effect=lambda report: order.append("identity") or {"tools": "fixed"}))
            release.build_local(self.tag)
        self.assertEqual(order[:2], ["start-builder", "identity"], "the builder must run before its identity is read")
        self.assertEqual(order.count("start-builder"), 1)

    def test_start_builder_bootstraps_the_selected_builder(self):
        with mock.patch.object(release, "run") as runner:
            release.start_builder()
        runner.assert_called_once_with("docker", "buildx", "inspect", "--bootstrap", capture=True)

    def test_resume_rejects_artifact_corruption(self):
        with ExitStack() as stack:
            root, _ = self.harness(stack)
            with self.assertRaises(release.ReleaseError):
                release.build_local(self.tag)
            path = root / "dist" / self.tag / self.cli_file
            path.write_bytes(b"changed")
            with self.assertRaisesRegex(release.ReleaseError, "checksum changed"):
                release.build_local(self.tag, resume=True)

    def test_resume_rejects_changed_runtime_before_ci(self):
        with ExitStack() as stack:
            _, events = self.harness(stack)
            with self.assertRaises(release.ReleaseError):
                release.build_local(self.tag)
            before = list(events)
            stack.enter_context(mock.patch.object(release, "build_runtime_identity", return_value={"tools": "changed"}))
            with self.assertRaisesRegex(release.ReleaseError, "environment changed"):
                release.build_local(self.tag, resume=True)
            self.assertEqual(events, before)

    def test_runtime_identity_ignores_launcher_bookkeeping_but_tracks_build_options(self):
        report = {"checks": []}
        with mock.patch.object(release, "database_url", return_value="postgres://localhost/dev"), \
                mock.patch.object(release, "local_targets", return_value=[]):
            before = release.build_runtime_identity(report)
            with mock.patch.dict(release.os.environ, {"_": "different-command", "SHLVL": "100", "JUST_RECIPE": "resume"}):
                self.assertEqual(before, release.build_runtime_identity(report))
            with mock.patch.dict(release.os.environ, {"RUSTFLAGS": "changed-build-option"}):
                self.assertNotEqual(before, release.build_runtime_identity(report))

    def test_receipt_never_records_exception_text_or_environment_values(self):
        root = Path(self.temp.name)
        receipt = release.Receipt(root / "receipt.json", {"tag": self.tag})
        with self.assertRaises(release.ReleaseError):
            receipt.step("test", lambda: (_ for _ in ()).throw(release.ReleaseError("password=secret")))
        text = (root / "receipt.json").read_text()
        self.assertNotIn("password", text)
        self.assertNotIn("secret", text)
        self.assertEqual(json.loads(text)["stages"][0]["status"], "failed")


if __name__ == "__main__":
    unittest.main()
