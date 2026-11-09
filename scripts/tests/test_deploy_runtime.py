"""Exercise deployment failures without a registry, host or real user data."""
import hashlib
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest import mock


def module(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).resolve().parents[1] / (name + '.py'))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


runtime = module('deploy_runtime')


def config():
    return {'name': 'isolated', 'volumes': {'pg': {'name': 'keep-pg', 'external': True}},
            'services': {'cormex': {'image': 'example/cormex:v0.1.18', 'environment': {'KEY': 'keep'}},
                         'web': {'image': 'example/cormex-web:v0.1.18'},
                         'postgres': {'image': 'pgvector/pgvector:pg17'}, 'rustfs': {'image': 'rustfs:fixed'}}}


def expected():
    return {'tag': 'v0.1.19', 'commit': 'a' * 40, 'bundle_sha256': 'b' * 64,
            'images': {name: {'content_sha256': 'c' * 64, 'repositories': ['example/' + name]}
                       for name in ('cormex', 'cormex-web')}}


def items():
    return {name: {'Id': name + '-id', 'Image': name + '-image', 'Mounts': [{'Name': 'keep-' + name}],
                   'State': {'Running': True, 'Health': {'Status': 'healthy'}}, 'RestartCount': 0}
            for name in ('cormex', 'web', 'postgres', 'rustfs')}


class PinAndConfigurationTests(unittest.TestCase):
    def test_pin_changes_only_version_and_preserves_crlf_and_other_values(self):
        original = b'CORMEX_VERSION=v0.1.18\r\nSECRET=do-not-change\r\n'
        result = runtime.replace_pin(original, 'v0.1.18', 'v0.1.19')
        self.assertEqual(result, original.replace(b'v0.1.18', b'v0.1.19'))

    def test_duplicate_or_changed_pin_is_rejected(self):
        for data in (b'CORMEX_VERSION=v0.1.17\n', b'CORMEX_VERSION=v0.1.18\nCORMEX_VERSION=v0.1.18\n'):
            with self.subTest(data=data), self.assertRaises(runtime.DeploymentError):
                runtime.replace_pin(data, 'v0.1.18', 'v0.1.19')

    def test_volume_and_secret_changes_are_rejected(self):
        for change in ('volume', 'secret', 'project', 'database'):
            before, after = config(), config()
            if change == 'volume':
                after['volumes']['pg']['name'] = 'empty-new-volume'
            elif change == 'secret':
                after['services']['cormex']['environment']['KEY'] = 'changed'
            elif change == 'project':
                after['name'] = 'other'
            else:
                after['services']['postgres']['image'] = 'postgres:other'
            with self.subTest(change=change), self.assertRaises(runtime.DeploymentError):
                runtime.validate_configuration_change(before, after)

    def test_digest_overlay_uses_the_configured_approved_repository(self):
        approved = expected()
        for name, value in approved['images'].items():
            value['references'] = {'example/' + name + ':v0.1.19': 'sha256:' + 'd' * 64}
        result = runtime.digest_overlay(config(), approved, 'v0.1.19')
        self.assertEqual(result['services']['web']['image'], 'example/cormex-web@sha256:' + 'd' * 64)
        with self.assertRaises(runtime.DeploymentError):
            runtime.digest_overlay(config(), expected(), 'v0.1.19')

    def test_second_operation_cannot_take_the_same_stack_lock(self):
        with tempfile.TemporaryDirectory() as temp, runtime.deployment_lock(temp):
            with self.assertRaises(runtime.DeploymentError):
                with runtime.deployment_lock(temp):
                    self.fail('a second lock must not succeed')


class RuntimeVerificationTests(unittest.TestCase):
    def stack(self, version='0.1.19', public=b'approved-script'):
        checksum = hashlib.sha256(b'approved-script').hexdigest()
        def fetch(url, timeout=10):
            if url.endswith('/version.json'):
                return 'application/json', json.dumps({'version': version}).encode()
            return 'application/javascript', public
        return SimpleNamespace(fetch=fetch, run=mock.Mock(return_value=checksum + '  main.dart.js'))

    def test_stale_web_version_or_bytes_fail_even_when_the_server_is_ready(self):
        for stack in (self.stack(version='0.1.18'), self.stack(public=b'old-script')):
            with mock.patch.object(runtime, 'containers', return_value=items()):
                with self.assertRaises(runtime.DeploymentError):
                    runtime.check_containers(stack, '.', 'http://example', 'v0.1.19', timeout=0)

    def test_unhealthy_dependency_fails(self):
        running = items()
        running['rustfs']['State']['Health']['Status'] = 'unhealthy'
        with mock.patch.object(runtime, 'containers', return_value=running):
            with self.assertRaises(runtime.DeploymentError):
                runtime.check_containers(self.stack(), '.', 'http://example', 'v0.1.19', timeout=0)

    def test_success_records_statistics_method_and_public_hash(self):
        estimates = {'method': 'pg_stat_user_tables.n_live_tup', 'exact': False, 'tables': []}
        with mock.patch.object(runtime, 'containers', return_value=items()), mock.patch.object(
                runtime, 'table_statistics', return_value=estimates):
            report = runtime.check_containers(self.stack(), '.', 'http://example', 'v0.1.19', timeout=0)
        self.assertFalse(report['statistics']['exact'])
        self.assertEqual(report['web_main_js_sha256'], hashlib.sha256(b'approved-script').hexdigest())


class UpgradeFailureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.original = b'CORMEX_VERSION=v0.1.18\nSECRET=keep-me\n'
        (self.directory / '.env').write_bytes(self.original)
        self.stack = SimpleNamespace(validate_tag=lambda value: value, health=lambda base: {'role': 'cormexd', 'version': '0.1.18'},
                                     compose=mock.Mock(), run=mock.Mock(), verify=mock.Mock(return_value={'version': '0.1.19'}))
        self.patches = [mock.patch.object(runtime, 'load_module', return_value=self.stack),
                        mock.patch.object(runtime, 'containers', side_effect=lambda *args: items()),
                        mock.patch.object(runtime, 'table_statistics', return_value={'exact': False, 'tables': []}),
                        mock.patch.object(runtime, 'configuration', side_effect=self.configuration),
                        mock.patch.object(runtime, 'backup_database', return_value={'sha256': 'a' * 64}),
                        mock.patch.object(runtime, 'check_image', side_effect=self.image),
                        mock.patch('builtins.print')]
        for patch in self.patches:
            patch.start()
            self.addCleanup(patch.stop)

    def configuration(self, *args):
        value = config()
        tag = 'v0.1.19' if b'v0.1.19' in (self.directory / '.env').read_bytes() else 'v0.1.18'
        for service in ('cormex', 'web'):
            name = 'cormex-web' if service == 'web' else service
            value['services'][service]['image'] = 'example/' + name + ':' + tag
        overlay = self.directory / 'compose.release.json'
        if overlay.exists():
            for service, entry in json.loads(overlay.read_text())['services'].items():
                value['services'][service]['image'] = entry['image']
        return value

    def image(self, stack, reference, approved):
        return {'RepoDigests': [reference.rsplit(':', 1)[0] + '@sha256:' + 'd' * 64]}

    def run_upgrade(self):
        return runtime.upgrade('v0.1.19', self.directory, 'v0.1.18', 'http://example', expected())

    def receipt(self):
        paths = list((self.directory / 'release-backups').glob('*/deploy-receipt.json'))
        self.assertEqual(len(paths), 1)
        self.assertEqual(paths[0].stat().st_mode & 0o777, 0o600)
        data = json.loads(paths[0].read_text())
        self.assertNotIn('keep-me', paths[0].read_text(), 'receipts must never include dotenv values')
        return data

    def test_backup_failure_leaves_the_pin_untouched(self):
        with mock.patch.object(runtime, 'backup_database', side_effect=runtime.DeploymentError('backup failed')):
            with self.assertRaises(runtime.DeploymentError):
                self.run_upgrade()
        self.assertEqual((self.directory / '.env').read_bytes(), self.original)
        self.assertEqual(self.receipt()['stages'][0]['status'], 'failed')
        self.stack.compose.assert_not_called()

    def test_unapproved_image_restores_pre_start_configuration(self):
        old_overlay = b'{"services":{}}\n'
        (self.directory / 'compose.release.json').write_bytes(old_overlay)
        with mock.patch.object(runtime, 'check_image', side_effect=runtime.DeploymentError('wrong image')):
            with self.assertRaises(runtime.DeploymentError):
                self.run_upgrade()
        self.assertEqual((self.directory / '.env').read_bytes(), self.original)
        self.assertEqual((self.directory / 'compose.release.json').read_bytes(), old_overlay)
        self.assertIn('before start', self.receipt()['recovery'])
        self.stack.compose.assert_not_called()

    def test_start_failure_never_automatically_rolls_back_schema_or_pin(self):
        self.stack.compose.side_effect = runtime.DeploymentError('partial start')
        with self.assertRaises(runtime.DeploymentError):
            self.run_upgrade()
        self.assertIn(b'v0.1.19', (self.directory / '.env').read_bytes())
        result = self.receipt()
        self.assertEqual(result['status'], 'failed')
        self.assertIn('inspect schema', result['recovery'])

    def test_success_pins_registry_digests_and_only_starts_application_services(self):
        result = self.run_upgrade()
        self.assertEqual(result['status'], 'passed')
        self.stack.compose.assert_called_once_with(self.directory, 'up', '-d', '--no-deps', 'cormex', 'web', project=None)
        self.assertIn('@sha256:', (self.directory / 'compose.release.json').read_text())
        self.assertEqual([stage['name'] for stage in self.receipt()['stages']],
                         ['backup', 'prepare-pin', 'pull-approved-images', 'start', 'verify'])
        self.assertTrue(all('duration_seconds' in stage for stage in result['stages']))


class CandidateApprovalTests(unittest.TestCase):
    def release(self, draft=False):
        def require(ok, message):
            if not ok:
                raise runtime.DeploymentError(message)
        entry = {'name': 'cormex', 'file': 'server.tar.gz', 'image_id': 'daemon-specific'}
        return SimpleNamespace(validate_tag=lambda tag: tag, validate_checksum=lambda checksum: checksum,
                               require=require, find_release=mock.Mock(return_value={'id': 1, 'draft': draft}),
                               release_snapshot=lambda release: (1,), run=mock.Mock(), repository=lambda: 'owner/project',
                               verify_bundle=mock.Mock(return_value={'commit': 'a' * 40, 'images': [entry]}),
                               remote_commit=lambda tag: 'a' * 40, archive_content_identity=lambda path: 'c' * 64,
                               REGISTRIES=('example',), api=mock.Mock(return_value={'id': 1}))

    def test_draft_is_rejected_before_download(self):
        candidate = module('deploy_candidate')
        release = self.release(draft=True)
        with mock.patch.object(candidate, 'release_module', return_value=release):
            with self.assertRaises(runtime.DeploymentError):
                candidate.published_expectations('v0.1.19', 'b' * 64)
        release.run.assert_not_called()

    def test_old_candidate_uses_archive_content_and_explicit_approved_checksum(self):
        candidate = module('deploy_candidate')
        release = self.release()
        with mock.patch.object(candidate, 'release_module', return_value=release):
            result = candidate.published_expectations('v0.1.19', 'b' * 64)
        self.assertEqual(result['bundle_sha256'], 'b' * 64)
        self.assertEqual(result['images']['cormex']['content_sha256'], 'c' * 64)
        self.assertEqual(release.verify_bundle.call_args.args[2], 'b' * 64)
        self.assertNotIn('image_id', result['images']['cormex'])


class ReapplyTests(unittest.TestCase):
    def test_changed_uses_container_ids_instead_of_stdout_text(self):
        for after, changed in (('old-id', False), ('new-id', True)):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as temp:
                stack = SimpleNamespace(validate_tag=lambda tag: tag, load_env=lambda directory: {'CORMEX_VERSION': 'v0.1.19'},
                                        compose=mock.Mock(side_effect=['old-id', '', after]), verify=mock.Mock())
                with mock.patch.object(runtime, 'load_module', return_value=stack):
                    result = runtime.reapply('v0.1.19', temp, 'http://example', pull=False)
                self.assertEqual(result['changed'], changed)


if __name__ == '__main__':
    unittest.main()
