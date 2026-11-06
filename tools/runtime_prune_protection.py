#!/usr/bin/env python3
"""Export only the named current kit operation's offline recovery runtime refs.

Stdlib only. No daemon, environment/home discovery or runtime cleanup. Writes
only the explicitly named proof output.
After stopping cutex, the kit MUST --invalidate the output before activation or
recovery changes runtime selection/files or restarts cutex; abort on failure.
The owner kit must run this AFTER activation/recovery's final complete or
recovered-previous operation record is stable. On first start, cleanup stays
disabled until activation finishes and this index is published. This index is not
authentication of a kit: it checks the kit's existing SHA bindings, not signatures.
The daemon must reject missing/invalid/stale indexes: compare
activeExecutableSha256 with current_exe bytes and re-hash
<kitOperation>/operation.json against provenance.operationSha256 on every read.
Historical operation directories are never scanned.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import sys
import tempfile

SCHEMA = 'cutex/runtime-prune-protection/v1'
ROLES = ('executable', 'cli', 'code_mode_host', 'schema', 'facade')
FIELDS = {
    'claude': ('python', 'adapter_source', 'claude_cli', 'tui_binary'),
    'dsh': ('node', 'adapter', 'dsh_cli', 'install_root'),
    'kimi': ('node', 'adapter', 'kimi_cli', 'install_root'),
}


class ProtectionError(ValueError):
    """Evidence is incomplete, changed or unsupported; do not publish an index."""


def require(ok, message):
    if not ok:
        raise ProtectionError(message)


def object_value(value, label):
    require(isinstance(value, dict), label + ': JSON object required')
    return value


def _unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON key: ' + key)
        result[key] = value
    return result


def read(path):
    try:
        return object_value(json.loads(path.read_text(encoding='utf-8-sig'),
                                       object_pairs_hook=_unique), str(path))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProtectionError('cannot read JSON: ' + str(path)) from error


def digest(path):
    try:
        with path.open('rb') as stream:
            return hashlib.file_digest(stream, 'sha256').hexdigest()
    except OSError as error:
        raise ProtectionError('cannot hash file: ' + str(path)) from error


def verified(path, expected, label):
    require(isinstance(expected, str) and re.fullmatch('[0-9a-f]{64}', expected),
            label + ': lowercase SHA256 required')
    require(digest(path) == expected, label + ': SHA256 mismatch')


def absolute(value, label):
    require(isinstance(value, str) and bool(value) and '\0' not in value,
            label + ': path string required')
    path = Path(value)
    require(path.is_absolute(), label + ': local absolute path required')
    # Preserve lexical source paths, especially current symlinks, not their
    # post-activation targets. The daemon canonicalizes its inventory separately.
    return Path(os.path.normpath(value))


def contained(root, relative):
    require(isinstance(relative, str) and bool(relative) and '\\' not in relative
            and ':' not in relative and '\0' not in relative,
            'invalid snapshot relative path')
    rel = PurePosixPath(relative)
    require(not rel.is_absolute() and '..' not in rel.parts and str(rel) == relative
            and relative != '.', 'snapshot path escapes or is not normalized')
    path = root / relative
    require(path.resolve().is_relative_to(root.resolve()), 'snapshot path escapes directory')
    return path


def snapshot_complete(op):
    path = op / 'steps.jsonl'
    try:
        rows = [json.loads(line, object_pairs_hook=_unique)
                for line in path.read_text(encoding='utf-8-sig').splitlines() if line.strip()]
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProtectionError('cannot read complete steps.jsonl') from error
    steps = [object_value(row, 'journal row') for row in rows]
    steps = [row for row in steps if row.get('step') == 'backup.verify']
    require(len(steps) == 2 and steps[0].get('phase') == 'begin'
            and steps[1].get('phase') == 'end' and type(steps[1].get('exitCode')) is int
            and steps[1]['exitCode'] == 0, 'backup.verify: complete successful snapshot required')


def build_index(operation_dir, active_executable, install_base=None):
    """Read frozen r274.1 schemas; return deterministic v1 index or raise.

    Requires all four deployment backups, component-prepared SHA binding and
    backed-up state.json, native-managed-before SHA/pins binding, and journal
    backup.verify success and a final complete/recovered-previous operation.
    active_executable is an explicit absolute selected cutex binary whose bytes
    bind this index to the daemon image. No environment discovery is performed.
    This deliberately refuses unsupported older kits.
    install_base defaults to the unique originals-manifest components/state.json
    parent. An explicit override must match it. No live install.json is read.
    """
    op = Path(operation_dir).resolve()
    operation_sha = digest(op / 'operation.json')
    record = read(op / 'operation.json')
    require(record.get('phase') in {'complete', 'recovered-previous'},
            'operation must have a stable final complete or recovered-previous record')
    executable = absolute(str(active_executable), '--active-executable')
    executable_sha = digest(executable)
    snapshot_complete(op)
    backups = read(op / 'backup-manifest.json')
    originals = read(op / 'originals-manifest.json')
    require(backups and originals, 'empty snapshot manifests')
    backup_files = {}
    for relative, expected in backups.items():
        file = contained(op / 'backup', relative)
        verified(file, expected, 'backup/' + relative)
        backup_files[relative] = file
    original_files = {}
    for original, row in originals.items():
        source = absolute(original, 'originals-manifest key')
        row = object_value(row, 'originals-manifest entry')
        file = contained(op / 'originals', row.get('file'))
        verified(file, row.get('sha256'), 'originals/' + row['file'])
        require(source not in original_files, 'duplicate normalized original path')
        original_files[source] = file
    states = [p for p in original_files if p.name == 'state.json' and p.parent.name == 'components']
    require(len(states) == 1, 'exactly one backed-up components/state.json required')
    state_path = states[0]
    base = state_path.parent.parent
    if install_base is not None:
        require(absolute(str(install_base), '--install-base') == base,
                '--install-base differs from backed-up components/state.json')
    prepared_file = op / 'component-prepared.json'
    verified(prepared_file, record.get('componentPreparedSha256'), 'component-prepared operation binding')
    prepared = read(prepared_file)
    require(prepared.get('phase') == 'prepared' and prepared == record.get('componentPreparationReport'),
            'component preparation report operation binding mismatch')
    protected = object_value(prepared.get('protected'), 'component-prepared.protected')
    state_proof = object_value(protected.get('state'), 'component-prepared.protected.state')
    require(state_proof.get('exists') is True, 'before component state snapshot required')
    verified(original_files[state_path], state_proof.get('sha256'), 'before component state binding')
    paths = set()

    def add(value, label):
        paths.add(str(absolute(value, label)))

    for name, fields in FIELDS.items():
        relative = 'runtime/' + name + '/deployment.json'
        require(relative in backup_files, 'missing backup reference: ' + relative)
        proof = object_value(protected.get(name + '-runtime'), 'before ' + name + ' binding')
        require(proof.get('exists') is True, 'before ' + name + ' deployment required')
        verified(backup_files[relative], proof.get('sha256'), 'before ' + name + ' deployment binding')
        deployment = read(backup_files[relative])
        for field in fields:
            add(deployment.get(field), name + '.' + field)
        if name == 'kimi' and deployment.get('rg') is not None:
            add(deployment['rg'], 'kimi.rg')
    state = read(original_files[state_path])
    for name in FIELDS:
        component = name + '-runtime'
        row = state.get(component)
        if row is None:
            continue  # A before deployment can be external, without a component.
        row = object_value(row, 'component state ' + component)
        version = row.get('selected')
        require(isinstance(version, str) and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*', version)
                and version not in {'.', '..'}, 'invalid selected component version: ' + component)
        add(str(base / 'components' / component / version), component)

    before_file = op / 'native-managed-before.json'
    verified(before_file, record.get('nativeManagedBeforeSha256'), 'native before operation binding')
    before = read(before_file)
    require(before.get('schema') == 'cutex/native-managed-before/v1', 'unsupported native before schema')
    light = 'runtime/light/deployment.json'
    require(light in backup_files, 'missing backup reference: ' + light)
    verified(backup_files[light], before.get('deploymentSha256'), 'native before deployment binding')
    deployment = read(backup_files[light])
    require(deployment == before.get('deployment'), 'native before deployment content mismatch')
    deployment_path = str(absolute(before.get('deploymentPath'), 'native deploymentPath'))
    pins = object_value(object_value(record.get('pins'), 'operation.pins').get('native'), 'operation.pins.native')
    require(pins.get(deployment_path) == before['deploymentSha256'], 'native deployment operation pins mismatch')
    manifest_path = str(absolute(before.get('manifestPath'), 'native manifestPath'))
    require(deployment.get('bundle_manifest') == before['manifestPath'], 'native selected manifest mismatch')
    require(pins.get(manifest_path) == before.get('manifestSha256'), 'native manifest operation pins mismatch')

    def bundle(path, expected, frozen=None, artifacts=None):
        # Only explicit selected/pinned manifest origins. No directory discovery,
        # registry traversal, shared_config or native_home opens.
        source = original_files.get(Path(path), Path(path))
        verified(source, expected, 'native manifest')
        manifest = read(source)
        if frozen is not None:
            require(manifest == frozen, 'native before manifest content mismatch')
        for role in ROLES:
            ref = object_value(manifest.get(role), 'native artifact ' + role)
            if artifacts is not None:
                require(ref == artifacts.get(role), 'native before artifact mismatch: ' + role)
            require(isinstance(ref.get('sha256'), str) and re.fullmatch('[0-9a-f]{64}', ref['sha256']),
                    'native artifact SHA256 required: ' + role)
            add(ref.get('path'), 'native artifact ' + role)

    bundle(manifest_path, before.get('manifestSha256'), before.get('manifest'),
           object_value(before.get('artifacts'), 'native before artifacts'))
    for path, proof in object_value(before.get('existingBundleFiles'), 'existingBundleFiles').items():
        absolute(path, 'pinned manifest')
        proof = object_value(proof, 'pinned manifest fingerprint')
        require(proof.get('exists') is True, 'pinned native manifest unavailable')
        bundle(path, proof.get('sha256'))
    # Original service images are also recovery refs. Other original metadata,
    # auth/config/state keys and native registry aliases are never exported.
    for original in original_files:
        if original.name in {'cutex', 'cutex.exe', 'cutex-mcp', 'cutex-mcp.exe'}:
            add(str(original), 'original service image')
    verified(op / 'operation.json', operation_sha, 'operation changed during export')
    verified(executable, executable_sha, 'active executable changed during export')
    return {'schema': SCHEMA, 'kitOperation': str(op),
            'activeExecutableSha256': executable_sha, 'protectedPaths': sorted(paths),
            'provenance': {'operationSha256': operation_sha,
                           'backupManifestSha256': digest(op / 'backup-manifest.json'),
                           'originalsManifestSha256': digest(op / 'originals-manifest.json')}}


def export_index(operation_dir, output, active_executable, install_base=None):
    """Validate completely before atomically replacing the explicit output only."""
    index = build_index(operation_dir, active_executable, install_base)
    output = Path(output).absolute()
    require(not output.resolve().is_relative_to(Path(operation_dir).resolve()),
            'output must be outside immutable operation evidence')
    require(output.resolve() != Path(active_executable).resolve(),
            'output must not overwrite active executable')
    output.parent.mkdir(parents=True, exist_ok=True)
    temp = None
    try:
        with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', newline='\n',
                                         dir=output.parent, prefix='.' + output.name + '.',
                                         delete=False) as stream:
            temp = Path(stream.name)
            json.dump(index, stream, ensure_ascii=False, indent=2)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        verified(Path(operation_dir) / 'operation.json', index['provenance']['operationSha256'],
                 'operation changed before publication')
        verified(Path(active_executable), index['activeExecutableSha256'],
                 'active executable changed before publication')
        os.replace(temp, output)
    finally:
        if temp is not None and temp.exists():
            temp.unlink()
    return index


def invalidate_index(output):
    """Kit calls after stopping cutex, before changing runtimes or restarting it."""
    output = Path(output)
    require(output.is_absolute(), 'invalidation output must be absolute')
    require(output.name == 'prune-protection.json' and output.parent.name == 'runtime',
            'invalidation output must be runtime/prune-protection.json')
    require(not output.is_symlink(), 'invalidation output must not be a symlink')
    output.parent.mkdir(parents=True, exist_ok=True)
    temp = None
    try:
        with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', newline='\n',
                                         dir=output.parent, prefix='.' + output.name + '.',
                                         delete=False) as stream:
            temp = Path(stream.name)
            json.dump({'schema': 'cutex/runtime-prune-protection/invalidated'}, stream)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, output)
    finally:
        if temp is not None and temp.exists():
            temp.unlink()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        'Supported evidence: r274.1 backup-manifest {relativePath: sha256}, '
        'originals-manifest {absoluteOriginalPath: {file, sha256}}, '
        'component-prepared.protected and native-managed-before/v1. '
        'Run AFTER activation/recovery final complete or recovered-previous record is stable. '
        'First-start cleanup stays disabled until activation finishes and publishes the index. '
        'On every read daemon must compare activeExecutableSha256 to current_exe bytes '
        'and provenance.operationSha256 to <kitOperation>/operation.json bytes. '
        'Missing or invalid index must disable daemon prune. No automatic cleanup.'))
    parser.add_argument('--invalidate', action='store_true',
                        help='after cutex stops, invalidate old proof before any runtime mutation/restart; abort kit on failure')
    parser.add_argument('--operation-dir', type=Path,
                        help='one current kit operations/<id> directory; never scans history')
    parser.add_argument('--output', required=True, type=Path,
                        help='explicit runtime/prune-protection.json to atomically replace')
    parser.add_argument('--active-executable', type=Path,
                        help='absolute new selected cutex binary (recovered selected binary after recovery); hash its bytes')
    parser.add_argument('--install-base', type=Path,
                        help='optional absolute install base; must match backed-up components/state.json')
    args = parser.parse_args(argv)
    if args.invalidate and any((args.operation_dir, args.active_executable, args.install_base)):
        parser.error('--invalidate accepts only --output')
    if not args.invalidate and (args.operation_dir is None or args.active_executable is None):
        parser.error('--operation-dir and --active-executable are required for export')
    try:
        if args.invalidate:
            invalidate_index(args.output)
            print('runtime prune protection: invalidated')
            return 0
        index = export_index(args.operation_dir, args.output, args.active_executable, args.install_base)
    except (ProtectionError, OSError) as error:
        print('runtime prune protection: ' + str(error), file=sys.stderr)
        return 1
    print('runtime prune protection: exported ' + str(len(index['protectedPaths'])) + ' paths')
    return 0


if __name__ == '__main__':
    sys.exit(main())
