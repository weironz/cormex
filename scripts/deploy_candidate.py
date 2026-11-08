#!/usr/bin/env python3
"""Read an explicitly approved published candidate for an Ansible deployment."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile


def release_module():
    spec = importlib.util.spec_from_file_location('cormex_release', Path(__file__).with_name('release.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def published_expectations(tag, checksum):
    release = release_module()
    release.validate_tag(tag)
    release.validate_checksum(checksum)
    current = release.find_release(tag)
    release.require(current is not None and not current['draft'], 'deployment requires a published Release')
    snapshot = release.release_snapshot(current)
    with tempfile.TemporaryDirectory(prefix='cormex-deploy-candidate-') as temporary:
        directory = Path(temporary)
        release.run('gh', 'release', 'download', tag, '--repo', release.repository(), '--dir', directory)
        manifest = release.verify_bundle(directory, tag, checksum)
        release.require(release.remote_commit(tag) == manifest['commit'], 'the source tag changed after publication')
        images = {}
        for entry in manifest['images']:
            identity = release.archive_content_identity(directory / entry['file'])
            release.require(entry.get('content_sha256', identity) == identity, 'image content differs from approved metadata')
            image = {'content_sha256': identity,
                     'repositories': [registry + '/' + entry['name'] for registry in release.REGISTRIES]}
            if entry.get('web_main_js_sha256'):
                image['web_main_js_sha256'] = entry['web_main_js_sha256']
            images[entry['name']] = image
        release.require(release.release_snapshot(release.api(f"releases/{current['id']}")) == snapshot,
                        'the Release changed during deployment preparation')
    return {'schema_version': 1, 'tag': tag, 'bundle_sha256': checksum,
            'commit': manifest['commit'], 'images': images}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('tag')
    parser.add_argument('checksum')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        result = {'cormex_expectations': published_expectations(args.tag, args.checksum)}
        fd = os.open(args.output, os.O_WRONLY | os.O_TRUNC | os.O_CREAT, 0o600)
        with os.fdopen(fd, 'w', encoding='utf-8') as file:
            json.dump(result, file, indent=2, sort_keys=True)
            file.write('\n')
    except Exception as error:
        print('deploy candidate: ' + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
