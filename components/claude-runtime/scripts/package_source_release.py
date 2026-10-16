"""Build an exact-byte wheel from an LF git export, alongside deployable source.

Run with an isolated build venv containing setuptools and wheel. This performs
no dependency installation and never reads a deployment or account directory.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import zipfile


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--revision', default='HEAD')
    parser.add_argument('--base', default='5116a2b')
    parser.add_argument('--release', default=None, help='Filename label; defaults to the archived features.json release')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    revision = subprocess.check_output(['git', 'rev-parse', args.revision], cwd=repo, text=True).strip()
    features = json.loads(subprocess.check_output(
        ['git', 'show', f'{revision}:components/claude-runtime/src/cute_claude/features.json'], cwd=repo))
    release = args.release or features['release']
    if not isinstance(release, str) or not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}', release):
        parser.error('release must be a simple filename label')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    source_zip = out / f'cutex-{release}-source.zip'
    subprocess.run(['git', 'archive', '--format=zip', f'--output={source_zip}', revision], cwd=repo, check=True)
    source = out / 'source-export'
    with zipfile.ZipFile(source_zip) as archive:
        archive.extractall(source)
    adapter = source / 'components' / 'claude-runtime'
    # git archive preserves blob bytes: older commits can contain CRLF even
    # when the current checkout uses autocrlf. Normalize the exported adapter
    # explicitly and record that distinction from the original Git objects.
    normalized = []
    text_suffixes = {'.py', '.json', '.md', '.toml', '.txt', '.in', '.ps1', '.sh', '.yaml', '.yml'}
    for path in sorted(adapter.rglob('*')):
        if path.is_file() and path.suffix in text_suffixes:
            original = path.read_bytes()
            if b'\r\n' in original:
                path.write_bytes(original.replace(b'\r\n', b'\n'))
                normalized.append(path.relative_to(source).as_posix())
    # Publish exactly the source used for the wheel, before setuptools writes
    # any build or egg-info files into the extracted tree.
    with zipfile.ZipFile(source_zip, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(source.rglob('*')):
            if path.is_file():
                archive.write(path, path.relative_to(source).as_posix())
    subprocess.run([sys.executable, '-m', 'pip', 'wheel', '--no-deps', '--no-build-isolation',
                    '--wheel-dir', str(out), str(adapter)], check=True)
    wheel, = out.glob('*.whl')
    matched = []
    with zipfile.ZipFile(wheel) as archive:
        for path in sorted((adapter / 'src' / 'cute_claude').rglob('*')):
            if not path.is_file() or path.suffix not in {'.py', '.json'}:
                continue
            relative = path.relative_to(adapter / 'src').as_posix()
            expected = path.read_bytes()
            assert b'\r\n' not in expected, f'git export is not LF: {relative}'
            assert archive.read(relative) == expected, f'wheel differs from git export: {relative}'
            matched.append({'path': relative, 'sha256': sha256(path)})
    with (out / 'review-fixes.patch').open('wb') as stream:
        subprocess.run(['git', 'diff', '--binary', args.base, revision], cwd=repo, stdout=stream, check=True)
    manifest = {
        'release': release,
        'revision': revision, 'reviewedBase': subprocess.check_output(
            ['git', 'rev-parse', args.base], cwd=repo, text=True).strip(),
        'sourceArchive': source_zip.name, 'sourceArchiveSha256': sha256(source_zip),
        'sourceArchiveDeployable': True,
        'sourceMatchesGitExactBytes': not normalized,
        'sourceExportNormalization': {'operation': 'CRLF to LF in adapter text files', 'paths': normalized},
        'sourceDeploymentPath': 'components/claude-runtime (contains src/ and scripts/connect_tui.py)',
        'wheel': wheel.name, 'wheelSha256': sha256(wheel), 'wheelDeployableToCutex': False,
        'wheelMatchesSourceExactBytes': True, 'wheelComparisonUsesEolNormalization': False,
        'matchedPackageFiles': matched,
        'note': 'Deploy the LF source directory with its matching Cutex build; the wheel alone lacks the Cutex runtime layout.',
    }
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'revision': revision, 'output': str(out), 'exactFiles': len(matched)}))


if __name__ == '__main__':
    main()
