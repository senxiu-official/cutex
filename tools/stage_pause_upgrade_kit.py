"""r290 P9: stage into an unsealed kit copy, never a deployed/READY kit."""
import argparse
import ast
from pathlib import Path
import shutil


def stage(root):
    root = Path(root)
    if (root / 'READY.json').exists():
        raise RuntimeError('stage only into an unsealed kit copy')
    path = root / 'deploy.py'
    source = path.read_text(encoding='utf-8-sig')
    cls = next(node for node in ast.parse(source).body if isinstance(node, ast.ClassDef) and node.name == 'Deploy')
    required = ('idle', 'offline', 'process_rows', 'activate')
    methods = [node for node in cls.body if isinstance(node, ast.FunctionDef) and node.name in required]
    if sorted(node.name for node in methods) != sorted(required):
        raise RuntimeError('expected exactly one idle, offline, process_rows and activate method')
    lines = source.splitlines(keepends=True)
    for method in sorted(methods, key=lambda node: node.lineno, reverse=True):
        if method.name == 'activate':
            marker = '# r290 P9: verified installed target is a no-op before predecessor selection.'
            if marker not in ''.join(lines[method.lineno-1:method.end_lineno]):
                lines[method.body[0].lineno-1:method.body[0].lineno-1] = [
                    '        ' + marker + '\n',
                    "        if importlib.import_module('pause_upgrade_kit').already_target(self):return\n"]
            continue
        if method.name == 'process_rows':
            lines[method.lineno-1:method.end_lineno] = [
                '    def process_rows(self):\n',
                '        # r290 P9: Linux writers are scoped to this home and owner.\n',
                "        return importlib.import_module('process_scope_kit').process_rows(self)\n"]
            continue
        arguments = 'self,wait=wait' if method.name == 'idle' else 'self,op,rows'
        signature = 'self,wait=False' if method.name == 'idle' else 'self,op,rows'
        lines[method.lineno-1:method.end_lineno] = [
            f'    def {method.name}({signature}):\n',
            '        # r290 P9: bounded maintenance preserves all owner pause gates.\n',
            f"        return importlib.import_module('pause_upgrade_kit').{method.name}({arguments})\n"]
    result = ''.join(lines)
    ast.parse(result)
    path.write_text(result, encoding='utf-8', newline='\n')
    shutil.copyfile(Path(__file__).with_name('pause_upgrade_kit.py'), root / 'pause_upgrade_kit.py')
    shutil.copyfile(Path(__file__).with_name('process_scope_kit.py'), root / 'process_scope_kit.py')
    # r290 P9: restart admits/unclaims this durable source. Preserve its exact
    # identity and ownerAuthorized fields in the stopped business backup too.
    backup = root / 'upgrade_backup.py'
    if backup.is_file():
        text = backup.read_text(encoding='utf-8-sig')
        marker = '# r290 P9: durable held-owner admission source across restart.'
        if marker not in text:
            parsed = ast.parse(text)
            legacy = next((node for node in parsed.body if isinstance(node, ast.Assign)
                           and any(isinstance(target, ast.Name) and target.id == 'LEGACY' for target in node.targets)), None)
            if legacy is None or not isinstance(ast.literal_eval(legacy.value), list):
                raise RuntimeError('expected literal LEGACY backup inventory')
            text += "\n" + marker + "\nif 'runtime/management-v2/owner-held-inputs.json' not in LEGACY:\n    LEGACY.append('runtime/management-v2/owner-held-inputs.json')\n"
            ast.parse(text)
            backup.write_text(text, encoding='utf-8', newline='\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('root', type=Path)
    stage(parser.parse_args().root)
