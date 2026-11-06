"""Restore a verified signature cache without changing an already published release."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys


def restore(source: Path, target: Path, public_key: Path) -> int:
    manifest_path = source / 'release/manifest.json'
    subprocess.run(['minisign', '-V', '-q', '-p', str(public_key), '-m', str(manifest_path)], check=True)
    manifest = json.loads(manifest_path.read_text())
    timestamp = manifest['publishedAt']
    if not re.fullmatch(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z', timestamp):
        raise ValueError('Invalid signed publication timestamp')
    hashes = {}
    for part in [manifest['cutex'], manifest.get('native', {}), *manifest.get('components', {}).values()]:
        for item in part.get('artifacts', {}).values():
            name, digest = item['name'], item['sha256']
            if Path(name).name != name or not re.fullmatch(r'[0-9a-f]{64}', digest):
                raise ValueError('Invalid signed artifact identity')
            if name in hashes and hashes[name] != digest:
                raise ValueError('Conflicting signed artifact hashes')
            hashes[name] = digest
    for path in source.rglob('*'):
        if path.is_file() and not path.name.endswith('.minisig'):
            subprocess.run(['minisign', '-V', '-q', '-p', str(public_key), '-m', str(path)], check=True)
            hashes[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
    cache = target / 'signatures'
    cache.mkdir(parents=True, exist_ok=False)
    count = 0
    for signature in source.rglob('*.minisig'):
        name = signature.name.removesuffix('.minisig')
        if name not in hashes:
            raise ValueError('Signature lacks a signed artifact hash: ' + name)
        shutil.copyfile(signature, cache / (hashes[name] + '-' + signature.name))
        count += 1
    (target / 'published-at').write_text(timestamp + '\n', encoding='utf-8')
    print(f'Restored {count} signatures; signing must run with --signed-only')
    return count


if __name__ == '__main__':
    restore(*(Path(value) for value in sys.argv[1:]))
