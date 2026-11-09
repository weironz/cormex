#!/usr/bin/env python3
"""Back up, pin and verify a published stack without compiling or publishing it.

The receipt is private to the deployment host. A failed start may already have run
migrations, so only failures before `up` restore the old configuration automatically.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time


class DeploymentError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise DeploymentError(message)


def load_module(name):
    spec = importlib.util.spec_from_file_location('cormex_' + name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def now():
    return datetime.now(timezone.utc).isoformat()


def digest(path):
    # Not hashlib.file_digest: it is Python 3.11+, and this file runs on the
    # deployment host's own interpreter. The v0.1.20 upgrade met a 3.10 host and
    # stopped right after its backup
    hasher = hashlib.sha256()
    with Path(path).open('rb') as file:
        for block in iter(lambda: file.read(1 << 20), b''):
            hasher.update(block)
    return hasher.hexdigest()


def write_private(path, data):
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as file:
            file.write(data)
            file.flush()
            os.fsync(file.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def write_json(path, value):
    write_private(path, (json.dumps(value, indent=2, sort_keys=True) + '\n').encode())


@contextmanager
def deployment_lock(directory):
    # The lock file remains in place: unlinking a locked inode permits a second
    # process to acquire a different inode while the first still owns its lock.
    import fcntl
    path = Path(directory) / '.deploy.lock'
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise DeploymentError('another deployment owns this stack') from error
        yield
    finally:
        os.close(fd)


def configuration(stack, directory, project=None):
    return json.loads(stack.compose(directory, 'config', '--format', 'json', capture=True, project=project))


def containers(stack, directory, project=None):
    result = {}
    config = configuration(stack, directory, project)
    for service in ('cormex', 'web', 'postgres', 'rustfs'):
        if service not in config.get('services', {}):
            continue
        identity = stack.compose(directory, 'ps', '-q', service, capture=True, project=project)
        require(identity and len(identity.splitlines()) == 1, 'expected one running container for ' + service)
        info = json.loads(stack.run('docker', 'inspect', identity, capture=True))[0]
        result[service] = info
    require({'cormex', 'web', 'postgres', 'rustfs'} <= result.keys(), 'the stack must have server, Web, database and object store containers')
    return result


def mount_snapshot(items):
    return {name: info['Mounts'] for name, info in items.items() if name in ('postgres', 'rustfs', 'cormex')}


def table_statistics(stack, postgres):
    # Estimates are intentionally labelled. They are useful for an operator's
    # baseline, but are never treated as an exact count or proof of no data loss.
    sql = ("SELECT COALESCE(json_agg(row_to_json(t)), '[]'::json) FROM "
           "(SELECT schemaname,relname,n_live_tup FROM pg_stat_user_tables "
           "WHERE relname IN ('facts','capture_files','capture_chunks','transcript_events') "
           "ORDER BY schemaname,relname) t")
    output = stack.run('docker', 'exec', postgres, 'sh', '-c',
                       'exec psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Atc "$1"', 'sh', sql, capture=True)
    return {'method': 'pg_stat_user_tables.n_live_tup', 'exact': False, 'tables': json.loads(output)}


def backup_database(stack, directory, destination, postgres):
    for name in ('.env', 'docker-compose.yml', 'docker-compose.override.yml', 'compose.override.yaml', 'compose.release.json'):
        source = Path(directory) / name
        if source.is_file():
            write_private(destination / name, source.read_bytes())
    target = destination / 'database.dump'
    with target.open('xb') as output:
        target.chmod(0o600)
        result = subprocess.run(['docker', 'exec', postgres, 'sh', '-c',
                                 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" -Fc'],
                                stdout=output, stderr=subprocess.PIPE, check=False)
    require(result.returncode == 0 and target.stat().st_size > 0, 'database backup failed; the production pin was not changed')
    with target.open('rb') as source, (destination / 'database.list').open('xb') as listing:
        result = subprocess.run(['docker', 'exec', '-i', postgres, 'pg_restore', '--list'],
                                stdin=source, stdout=listing, stderr=subprocess.PIPE, check=False)
    require(result.returncode == 0 and (destination / 'database.list').stat().st_size > 0,
            'pg_restore could not read the backup; the production pin was not changed')
    checksum = digest(target)
    write_private(destination / 'database.sha256', (checksum + '  database.dump\n').encode())
    return {'file': 'database.dump', 'bytes': target.stat().st_size, 'sha256': checksum,
            'validated': 'pg_restore --list', 'restore_exercised': False,
            'scope': 'database and deployment configuration; object volume is not snapshotted'}


def replace_pin(original, previous, target):
    expression = rb'(?m)^CORMEX_VERSION=([^\r\n]+)'
    matches = list(re.finditer(expression, original))
    require(len(matches) == 1, '.env must define CORMEX_VERSION exactly once')
    require(matches[0].group(1).decode().strip() == previous, 'the production pin changed since approval')
    return re.sub(expression, ('CORMEX_VERSION=' + target).encode(), original, count=1)


def validate_configuration_change(before, after):
    require(before.get('name') == after.get('name'), 'compose project changed while preparing the pin')
    require(before.get('volumes', {}) == after.get('volumes', {}), 'persistent volume definitions changed')
    for service in ('postgres', 'rustfs'):
        require(before['services'][service] == after['services'][service], 'storage service configuration changed: ' + service)
    for service in ('cormex', 'web'):
        left, right = dict(before['services'][service]), dict(after['services'][service])
        left.pop('image', None)
        right.pop('image', None)
        require(left == right, 'configuration other than the image changed: ' + service)


def image_expectation(expectations, service):
    name = 'cormex-web' if service == 'web' else service
    require(name in expectations.get('images', {}), 'approved image expectation missing: ' + name)
    return expectations['images'][name]


def digest_overlay(config, expectations, tag):
    services = {}
    for service in ('cormex', 'web'):
        image = config['services'][service]['image'].split('@', 1)[0]
        repository = image.rsplit(':', 1)[0] if ':' in image.rsplit('/', 1)[-1] else image
        reference = repository + ':' + tag
        approved = image_expectation(expectations, service)
        digest_value = approved.get('references', {}).get(reference)
        require(isinstance(digest_value, str) and re.fullmatch(r'sha256:[0-9a-f]{64}', digest_value),
                'the configured registry is absent from the approved image references: ' + service)
        services[service] = {'image': repository + '@' + digest_value}
    return {'services': services}


def check_image(stack, identity, expected):
    module = load_module('image_identity')
    info = json.loads(stack.run('docker', 'image', 'inspect', identity, capture=True))[0]
    actual = module.image_content_identity(info)
    require(actual == expected['content_sha256'], 'pulled image content differs from the approved archive')
    references = set(expected.get('references', {}).values())
    if references:
        require(any(item.rsplit('@', 1)[-1] in references for item in info.get('RepoDigests', [])),
                'image has no approved registry digest')
    return info


def check_containers(stack, directory, base, tag, expectations=None, project=None, timeout=300):
    deadline = time.monotonic() + timeout
    while True:
        items = containers(stack, directory, project)
        pending = []
        for name, item in items.items():
            require(item['State'].get('Running'), 'container stopped: ' + name)
            state = item['State'].get('Health', {}).get('Status')
            require(state is not None, 'container has no health check: ' + name)
            if state != 'healthy':
                pending.append(name)
        if not pending:
            break
        require(time.monotonic() < deadline, 'containers did not become healthy: ' + ', '.join(pending))
        time.sleep(2)
    identities = {}
    for service in ('cormex', 'web'):
        if expectations is not None:
            check_image(stack, items[service]['Image'], image_expectation(expectations, service))
        identities[service] = {'image_id': items[service]['Image'], 'container_id': items[service]['Id'],
                               'restart_count': items[service]['RestartCount']}
    content_type, body = stack.fetch(base + '/version.json')
    require('json' in content_type, 'Web version.json did not return JSON')
    require(json.loads(body).get('version') == tag[1:], 'Web version differs from the requested release')
    expected_js = stack.run('docker', 'exec', items['web']['Id'], 'sha256sum',
                            '/usr/share/nginx/html/main.dart.js', capture=True).split()[0]
    if expectations is not None:
        approved_js = image_expectation(expectations, 'web').get('web_main_js_sha256')
        if approved_js:
            require(expected_js == approved_js, 'Web image bundle differs from the approved build')
    _, body = stack.fetch(base + '/main.dart.js', timeout=30)
    actual_js = hashlib.sha256(body).hexdigest()
    require(actual_js == expected_js, 'public Web bundle differs from the running Web container')
    return {'containers': identities, 'mounts': mount_snapshot(items), 'web_main_js_sha256': actual_js,
            'statistics': table_statistics(stack, items['postgres']['Id'])}


def upgrade(tag, directory, previous, base, expectations, require_encryption=True, project=None):
    stack = load_module('deploy')
    stack.validate_tag(tag)
    stack.validate_tag(previous)
    require(tag != previous, 'upgrade requires distinct old and new versions')
    require(expectations.get('tag') == tag, 'expectations are for another release')
    directory = Path(directory).resolve()
    require(directory.is_dir(), 'deployment directory is missing')
    with deployment_lock(directory):
        original = (directory / '.env').read_bytes()
        updated = replace_pin(original, previous, tag)
        before = configuration(stack, directory, project)
        running = containers(stack, directory, project)
        expectations = json.loads(json.dumps(expectations))
        status = stack.health(base)
        require(status.get('version') == previous[1:] and status.get('role') == 'cormexd',
                'running service does not match the approved old version')
        parent = directory / 'release-backups'
        parent.mkdir(mode=0o700, exist_ok=True)
        destination = Path(tempfile.mkdtemp(prefix=datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-' + tag + '-', dir=parent))
        receipt_path = destination / 'deploy-receipt.json'
        receipt = {'schema_version': 1, 'operation': 'deploy-upgrade', 'from': previous, 'tag': tag,
                   'bundle_sha256': expectations['bundle_sha256'], 'commit': expectations['commit'],
                   'started_at': now(), 'status': 'running', 'stages': [], 'backup_directory': str(destination)}
        started = False
        overlay = directory / 'compose.release.json'
        old_overlay = overlay.read_bytes() if overlay.exists() else None

        def stage(name, action):
            item = {'name': name, 'status': 'running', 'started_at': now()}
            receipt['stages'].append(item)
            write_json(receipt_path, receipt)
            print('deploy stage: ' + name, flush=True)
            begin = time.monotonic()
            try:
                result = action()
            except BaseException:
                item['status'] = 'failed'
                raise
            else:
                item['status'] = 'passed'
                return result
            finally:
                item.update(finished_at=now(), duration_seconds=round(time.monotonic() - begin, 3))
                write_json(receipt_path, receipt)

        try:
            receipt['before'] = {'mounts': mount_snapshot(running),
                                 'statistics': table_statistics(stack, running['postgres']['Id'])}
            receipt['backup'] = stage('backup', lambda: backup_database(stack, directory, destination, running['postgres']['Id']))

            def pin():
                write_private(directory / '.env', updated)
                after = configuration(stack, directory, project)
                validate_configuration_change(before, after)
                return after

            target_config = stage('prepare-pin', pin)

            def pull():
                for service in ('cormex', 'web'):
                    image = target_config['services'][service]['image'].split('@', 1)[0]
                    repository = image.rsplit(':', 1)[0] if ':' in image.rsplit('/', 1)[-1] else image
                    approved = image_expectation(expectations, service)
                    require(repository in approved['repositories'], 'configured image repository is not in the approved release')
                    reference = repository + ':' + tag
                    stack.run('docker', 'pull', reference)
                    info = check_image(stack, reference, approved)
                    matches = [value.rsplit('@', 1)[1] for value in info.get('RepoDigests', [])
                               if value.startswith(repository + '@')]
                    require(len(set(matches)) == 1, 'pulled image has no unambiguous registry digest')
                    approved['references'] = {reference: matches[0]}
                write_json(overlay, digest_overlay(target_config, expectations, tag))
                validate_configuration_change(before, configuration(stack, directory, project))
                receipt['images'] = expectations['images']

            stage('pull-approved-images', pull)
            started = True
            stage('start', lambda: stack.compose(directory, 'up', '-d', '--no-deps', 'cormex', 'web', project=project))
            receipt['health'] = stage('verify', lambda: stack.verify(directory, tag, api=base,
                                                                    require_encryption=require_encryption,
                                                                    expectations=expectations, project=project))
            after_items = containers(stack, directory, project)
            require(mount_snapshot(after_items) == receipt['before']['mounts'], 'persistent mounts changed during deployment')
            receipt['after'] = {'mounts': mount_snapshot(after_items),
                                'statistics': table_statistics(stack, after_items['postgres']['Id'])}
            receipt['status'] = 'passed'
            print('DEPLOYMENT_COMPLETED ' + tag, flush=True)
        except BaseException:
            receipt['status'] = 'failed'
            if not started:
                if (directory / '.env').read_bytes() == updated:
                    write_private(directory / '.env', original)
                if old_overlay is None:
                    overlay.unlink(missing_ok=True)
                else:
                    write_private(overlay, old_overlay)
                receipt['recovery'] = 'old configuration restored before start'
            else:
                receipt['recovery'] = 'start was attempted; inspect schema and receipt before retrying or rolling back'
            raise
        finally:
            receipt['finished_at'] = now()
            write_json(receipt_path, receipt)
            print('DEPLOY_RECEIPT=' + str(receipt_path), flush=True)
        return receipt


def reapply(tag, directory, base, pull=True, require_encryption=True, project=None):
    stack = load_module('deploy')
    stack.validate_tag(tag)
    directory = Path(directory).resolve()
    with deployment_lock(directory):
        require(stack.load_env(directory).get('CORMEX_VERSION') == tag,
                'the host must already pin this version; use deploy-upgrade to change it')
        parent = directory / 'release-backups'
        parent.mkdir(mode=0o700, exist_ok=True)
        destination = Path(tempfile.mkdtemp(prefix='reapply-' + tag + '-', dir=parent))
        path = destination / 'deploy-receipt.json'
        receipt = {'schema_version': 1, 'operation': 'deploy-reapply', 'tag': tag,
                   'started_at': now(), 'status': 'running', 'stages': []}
        before = stack.compose(directory, 'ps', '--all', '-q', 'cormex', 'web', capture=True, project=project).splitlines()
        try:
            actions = []
            if pull:
                actions.append(('pull', lambda: stack.compose(directory, 'pull', 'cormex', 'web', project=project)))
            actions.extend([
                ('start', lambda: stack.compose(directory, 'up', '-d', 'cormex', 'web', project=project)),
                ('verify', lambda: stack.verify(directory, tag, api=base, require_encryption=require_encryption, project=project)),
            ])
            for name, action in actions:
                item = {'name': name, 'status': 'running', 'started_at': now()}
                receipt['stages'].append(item)
                write_json(path, receipt)
                begin = time.monotonic()
                try:
                    action()
                except BaseException:
                    item['status'] = 'failed'
                    raise
                else:
                    item['status'] = 'passed'
                finally:
                    item.update(finished_at=now(), duration_seconds=round(time.monotonic() - begin, 3))
                    write_json(path, receipt)
            after = stack.compose(directory, 'ps', '--all', '-q', 'cormex', 'web', capture=True, project=project).splitlines()
            receipt.update(status='passed', changed=sorted(before) != sorted(after))
        except BaseException:
            receipt['status'] = 'failed'
            raise
        finally:
            receipt['finished_at'] = now()
            write_json(path, receipt)
        return {'tag': tag, 'changed': receipt['changed'], 'receipt': str(path)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    verify = sub.add_parser('verify')
    verify.add_argument('tag')
    verify.add_argument('directory', type=Path)
    verify.add_argument('--base', required=True)
    verify.add_argument('--expectations', type=Path)
    verify.add_argument('--allow-plaintext', action='store_true')
    verify.add_argument('--project')
    operation = sub.add_parser('upgrade')
    operation.add_argument('previous')
    operation.add_argument('tag')
    operation.add_argument('directory', type=Path)
    operation.add_argument('--base', required=True)
    operation.add_argument('--expectations', type=Path, required=True)
    operation.add_argument('--allow-plaintext', action='store_true')
    operation.add_argument('--project')
    apply = sub.add_parser('reapply')
    apply.add_argument('tag')
    apply.add_argument('directory', type=Path)
    apply.add_argument('--base', required=True)
    apply.add_argument('--project')
    apply.add_argument('--no-pull', action='store_true')
    apply.add_argument('--allow-plaintext', action='store_true')
    args = parser.parse_args()
    try:
        expectations_path = getattr(args, 'expectations', None)
        expected = json.loads(expectations_path.read_text()) if expectations_path else None
        if args.command == 'upgrade':
            receipt = upgrade(args.tag, args.directory, args.previous, args.base, expected, not args.allow_plaintext, args.project)
            print(json.dumps({'tag': args.tag, 'changed': True,
                              'receipt': receipt['backup_directory'] + '/deploy-receipt.json'}))
        elif args.command == 'reapply':
            print(json.dumps(reapply(args.tag, args.directory, args.base, not args.no_pull, not args.allow_plaintext, args.project)))
        else:
            result = load_module('deploy').verify(args.directory, args.tag, api=args.base,
                                                   require_encryption=not args.allow_plaintext,
                                                   expectations=expected, project=args.project)
            print(json.dumps(result, sort_keys=True))
    except Exception as error:
        print('deploy: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
