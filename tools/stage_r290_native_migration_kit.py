"""Stage the explicit r290 migration hooks into an unsealed full kit."""
import ast
from pathlib import Path
import shutil

def stage(root):
    root=Path(root)
    if (root / "READY.json").exists():
        raise RuntimeError("cannot stage a sealed kit")
    path=root / "deploy.py"
    result=path.read_text(encoding="utf-8-sig")
    # r290 P9: capture immediately before stopping; undo reviewed moves while
    # the current daemon is alive, then restore selectors with writers stopped.
    capture = "            write(op/'preserved-mcp-before.json',"
    if '# r290 P9: explicit Native migration inventory.' not in result:
        if result.count(capture) != 1:
            raise RuntimeError('expected one pre-stop MCP capture')
        result = result.replace(capture,
            "            # r290 P9: explicit Native migration inventory.\n"
            "            importlib.import_module('r290_native_migration_kit').capture(self,op)\n" + capture)
    parsed = ast.parse(result)
    deploy = next(n for n in parsed.body if isinstance(n, ast.ClassDef) and n.name == 'Deploy')
    rollback = next(n for n in deploy.body if isinstance(n, ast.FunctionDef) and n.name == 'rollback')
    lines = result.splitlines(keepends=True)
    body = ''.join(lines[rollback.lineno-1:rollback.end_lineno])
    if '# r290 P9: undo only recorded Native pin moves.' not in body:
        stop = "            self.j.run('services.stop',lambda:self.service('stop'))\n"
        if body.count(stop) != 1:
            raise RuntimeError('expected one rollback service stop')
        body = body.replace(stop,
            "            # r290 P9: undo only recorded Native pin moves.\n"
            "            pins=importlib.import_module('r290_native_migration_kit').revert(self,act,op,pins)\n" + stop)
    restore = "            pins=importlib.import_module('r290_native_migration_kit').restore_defaults(self,act,op,pins)\n"
    body = body.replace(restore, '')
    fence = "            self.j.run('stopped.fence',self.fence)\n"
    if body.count(fence) != 1:
        raise RuntimeError('expected one rollback stopped-writer fence')
    body = body.replace(fence, fence + restore)
    if '.rollback_preflight(self,act)' not in body:
        marker = "        require(act is not None,'rollback needs a completed activation of this kit')\n"
        if body.count(marker) != 1:
            raise RuntimeError('expected completed activation check')
        body = body.replace(marker, marker + "        importlib.import_module('r290_native_migration_kit').rollback_preflight(self,act)\n")
    lines[rollback.lineno-1:rollback.end_lineno] = [body]
    result = ''.join(lines)
    parsed = ast.parse(result)
    deploy = next(n for n in parsed.body if isinstance(n, ast.ClassDef) and n.name == 'Deploy')
    recover = next(n for n in deploy.body if isinstance(n, ast.FunctionDef) and n.name == 'recover')
    lines = result.splitlines(keepends=True)
    body = ''.join(lines[recover.lineno-1:recover.end_lineno])
    if '.recover_preflight(self,op,r)' not in body:
        marker = '        self.j=Journal(op,self.token)\n'
        if body.count(marker) != 1:
            raise RuntimeError('expected one recovery journal')
        body = body.replace(marker, marker + "        importlib.import_module('r290_native_migration_kit').recover_preflight(self,op,r)\n")
    if '.recover_rollback(self,op,r)' not in body:
        marker = "            require(r['phase']!='rollback-preparing','rollback stopped before any change; run rollback again')\n"
        if body.count(marker) != 1:
            raise RuntimeError('expected rollback recovery entry')
        body = body.replace(marker,
            "            if self.plan.get('nativeMigration') is not None:\n"
            "                importlib.import_module('r290_native_migration_kit').recover_rollback(self,op,r)\n"
            "                return\n" + marker)
    lines[recover.lineno-1:recover.end_lineno] = [body]
    result = ''.join(lines)
    result = result.replace('Runtime updates follow the sealed auto-online policy; all existing session pins are preserved. Failures are listed per agent. --no-auto-online skips this phase.',
        'Runtime updates follow the explicit sealed policy. In r290 active Native r62 pins migrate to r63, including offline-active agents. Archived agents keep their pin and are reported as skipped:archived; Claude pins stay unchanged. Only kit-owned confirmed stops may auto-online. --no-auto-online disables starts, while the recorded Native migration still runs.')
    ast.parse(result)
    path.write_text(result,encoding="utf-8",newline="\n")
    shutil.copyfile(Path(__file__).with_name("r290_native_migration_kit.py"),root / "r290_native_migration_kit.py")
