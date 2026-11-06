"""Install prune protection into a COPY of the r276 owner kit, before sealing it.

This is a builder action, not activation or repair of an installed host. The
builder must regenerate the kit script-hash manifest after this step.
"""
import argparse
import ast
import os
from pathlib import Path
import shutil
import subprocess

TOOLS = Path(__file__).resolve().parent


def integrated_hooks(source):
    """Recognize already integrated hooks after later kit edits move patch context."""
    tree = ast.parse(source)
    expected = {'rollback_steps':['invalidate'], 'rollback':['invalidate'],
        'complete_operation':['publish'], 'reconcile':['invalidate','publish'],
        'activate':['invalidate'], 'recover':['invalidate','publish']}
    found = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.FunctionDef) or node.name not in expected:continue
        found[node.name] = [call.func.attr for call in ast.walk(node)
            if isinstance(call,ast.Call) and isinstance(call.func,ast.Attribute)
            and isinstance(call.func.value,ast.Name) and call.func.value.id=='runtime_prune_kit']
    imported = any(isinstance(node,ast.Import) and any(alias.name=='runtime_prune_kit' for alias in node.names) for node in tree.body)
    return imported and found == expected


def stage(kit_tool):
    kit = Path(kit_tool).resolve(strict=True)
    if not (kit / 'deploy.py').is_file():
        raise ValueError('kit-tool/deploy.py required')
    patch = TOOLS / 'release-kit' / 'r276-prune-protection.patch'
    command = ['git', '-C', str(kit), 'apply', '--whitespace=nowarn']
    # Treat this copy as a plain kit directory even when the builder put it
    # inside a checkout: git apply otherwise silently skips paths outside the
    # repository-relative subdirectory.
    env = dict(os.environ, GIT_CEILING_DIRECTORIES=str(kit.parent))
    check = subprocess.run(command + ['--check', str(patch)], capture_output=True, text=True, env=env)
    if check.returncode:
        reverse = subprocess.run(command + ['--reverse', '--check', str(patch)],
                                 capture_output=True, text=True, env=env)
        if reverse.returncode and not integrated_hooks((kit/'deploy.py').read_text(encoding='utf-8')):
            raise ValueError('prune kit patch does not match this deploy.py: ' + check.stderr)
    for name in ['runtime_prune_protection.py', 'runtime_prune_kit.py']:
        shutil.copyfile(TOOLS / name, kit / name)
    if not check.returncode:
        subprocess.run(command + [str(patch)], check=True, env=env)
    return kit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kit-tool-dir', required=True, type=Path)
    args = parser.parse_args()
    stage(args.kit_tool_dir)
    print('Prune protection staged; regenerate script hashes and rehearse before sealing.')


if __name__ == '__main__':
    main()
