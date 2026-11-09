"""Install the auto-online phase into a COPY of an r276-family owner kit tool.

This is a builder action before sealing, not activation or repair of an
installed host. It copies upgrade_online_kit.py beside deploy.py and makes
anchored edits (each anchor must match exactly once; already staged edits are
left alone, so staging twice is a no-op). Anchors rather than a unified diff:
the r276 and r276.1 copies differ in content and line endings, and the edits
must also sit beside the prune-protection patch. The builder must regenerate
the kit script-hash manifest after this step and rehearse before sealing.
"""
import argparse
import hashlib
from pathlib import Path
import re
import shutil

TOOLS = Path(__file__).resolve().parent
# deploy.py puts its own directory first on sys.path (like runtime_prune_kit).
MODULE = MAIN_MODULE = "importlib.import_module('upgrade_online_kit')"

# (name, pattern, replacement, marker proving the edit is already in place)
DEPLOY_EDITS = [
    ('preflight.process-evidence',
     r'^    def process_rows\(self\):$',
     "    def process_rows(self):{nl}        return importlib.import_module('kit_activation_preflight').process_rows(self){nl}"
     "    def service_process_ids(self):{nl}        return importlib.import_module('kit_activation_preflight').service_process_ids(self){nl}"
     "    def orphan_agent_preflight(self):{nl}        return importlib.import_module('kit_activation_preflight').orphan_agent_preflight(self){nl}"
     "    def _legacy_process_rows(self):",
     'def _legacy_process_rows(self):'),
    ('preflight.check-only',
     r'h=self\.identity\(\);self\.service_health\(\);self\.integrity\(\)',
     'h=self.identity();self.service_health();self.orphan_agent_preflight();self.integrity()',
     'self.service_health();self.orphan_agent_preflight();self.integrity()'),
    ('activate.inherited-online-notice',
     r'After activation, auto-online restores only agents this operation confirmed offline\.',
     'After activation, auto-online restores agents confirmed offline by this operation or an earlier failed attempt of this release.',
     'or an earlier failed attempt of this release.'),
    ('activate.after-complete',
     r"(self\.start_selected\(op,pins,selection_fenced=True\);print\('ACTIVATED )(.*?)( \(input ledger on\)); agents remain offline; use the named list in desktop\.'\);self\.claude_default\(\)",
     r"\1\2\3; upgrade complete.');self.claude_default();" + MODULE +".after_activation(self,op)",
     MODULE + '.after_activation(self,op)'),
    ('main.arguments',
     r"'lease-report'\)\);a=ap\.parse_args\(\)",
     "'lease-report','auto-online'));ap.add_argument('--no-auto-online',action='store_true',"
     "help='activate without bringing the stopped agents back online');a=ap.parse_args()",
     "'--no-auto-online'"),
    ('main.skip-flag',
     r"^([ \t]+)d=Deploy\(\)$",
     r"\1d=Deploy();d.skip_auto_online=a.no_auto_online",
     'd.skip_auto_online=a.no_auto_online'),
    ('main.retry-mode',
     r"^([ \t]+)elif a\.mode=='status':pass$",
     r"\1elif a.mode=='status':pass{nl}\1elif a.mode=='auto-online':" + MAIN_MODULE + ".retry(d)",
     "elif a.mode=='auto-online':"),
    ('main.retry-summary',
     r"if d and a\.mode in \('status','recover','rollback'\):d\.summary\(\)",
     "if d and a.mode in ('status','recover','rollback','auto-online'):d.summary()",
     "'rollback','auto-online'):d.summary()"),
    ('summary.back-online',
     r"current=\{r\['cutexSessionId'\]:r for r in self\.sessions\(\)\}",
     "current={r['cutexSessionId']:r for r in self.sessions()};back=" + MODULE + ".back_online_ids(op)",
     '.back_online_ids(op)'),
    ('summary.owner-decision',
     r"'ownerDecisionRequired':True\} for r in manual\]",
     "'ownerDecisionRequired':r['id'] not in back} for r in manual]",
     "'ownerDecisionRequired':r['id'] not in back}"),
    ('summary.status-line',
     r"print\('MANUAL ONLINE '\+json\.dumps\(manual,ensure_ascii=False\)\)",
     "print('MANUAL ONLINE '+json.dumps(manual,ensure_ascii=False))"
     ";print(line) if (line:=" + MODULE + ".status_line(op) if op else None) else None",
     '.status_line(op) if op'),
]
OWNER_SH_EDITS = [
    ('owner.sh.arguments',
     r'^exec python3 -I -B \./deploy\.py "\$\{1:-activate\}"$',
     '[ "$#" -gt 0 ] || set -- activate{nl}exec python3 -I -B ./deploy.py "$@"',
     'set -- activate'),
]


def edit(text, edits, where):
    nl = '\r\n' if '\r\n' in text else '\n'
    applied = []
    for name, pattern, replacement, marker in edits:
        if marker in text:
            continue
        # Older r276 tools predate this notice; there is nothing to update.
        if name == 'activate.inherited-online-notice' and 'After activation, auto-online restores' not in text:
            continue
        # A line-end anchor must not leave the CR of a CRLF file behind.
        if pattern.endswith('$') and not pattern.endswith('\\$'):
            pattern = pattern[:-1] + r'(?=\r?$)'
        regex = re.compile(pattern, re.M)
        found = regex.findall(text)
        if len(found) != 1:
            raise ValueError('%s: anchor %s matched %d times' % (where, name, len(found)))
        text = regex.sub(replacement.replace('{nl}', nl.replace('\r', '\\r').replace('\n', '\\n')), text, count=1)
        applied.append(name)
    return text, applied


def rewrite(path, edits):
    raw = path.read_bytes().decode('utf-8')
    text, applied = edit(raw, edits, path.name)
    if applied:
        path.write_bytes(text.encode('utf-8'))
    return applied


def stage(kit_tool):
    kit = Path(kit_tool).resolve(strict=True)
    if not (kit / 'deploy.py').is_file():
        raise ValueError('kit-tool/deploy.py required')
    # Validate every anchor before changing anything.
    edit((kit / 'deploy.py').read_bytes().decode('utf-8'), DEPLOY_EDITS, 'deploy.py')
    if (kit / 'owner.sh').is_file():
        edit((kit / 'owner.sh').read_bytes().decode('utf-8'), OWNER_SH_EDITS, 'owner.sh')
    shutil.copyfile(TOOLS / 'upgrade_online_kit.py', kit / 'upgrade_online_kit.py')
    shutil.copyfile(TOOLS / 'kit_activation_preflight.py', kit / 'kit_activation_preflight.py')
    applied = rewrite(kit / 'deploy.py', DEPLOY_EDITS)
    if (kit / 'owner.sh').is_file():
        applied += rewrite(kit / 'owner.sh', OWNER_SH_EDITS)
    return applied


# Builder's kit unit tests that pin deploy.py source text (r276.1 suite).
TEST_EDITS = {
    'test_r2742_claude_r35.py': [(r"use the named list in desktop.');self.claude_default()\n",
                                  "upgrade complete.');self.claude_default();")],
    'test_r275_stores.py': [("choices=('check','activate','recover','rollback','status','lease-report')",
                             "choices=('check','activate','recover','rollback','status','lease-report','auto-online')")],
}


def stage_tests(kit_tests):
    """Stage the suite's own kit-tool copy and update its source pins."""
    tests = Path(kit_tests).resolve(strict=True)
    deploy = tests / 'kit-tool' / 'deploy.py'
    before = hashlib.sha256(deploy.read_bytes()).hexdigest()
    applied = stage(deploy.parent)
    after = hashlib.sha256(deploy.read_bytes()).hexdigest()
    for path in tests.glob('test_*.py'):
        text = path.read_bytes().decode('utf-8')
        new = text.replace(before, after)
        for old, replacement in TEST_EDITS.get(path.name, []):
            new = new.replace(old, replacement)
        if new != text:
            path.write_bytes(new.encode('utf-8'))
            applied.append(path.name)
    return applied


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument('--kit-tool-dir', type=Path)
    target.add_argument('--kit-tests-dir', type=Path, help="the builder's kit-tests copy (stages its kit-tool, updates pins)")
    args = parser.parse_args()
    applied = stage(args.kit_tool_dir) if args.kit_tool_dir else stage_tests(args.kit_tests_dir)
    print('Auto-online staged (%s); regenerate script hashes and rehearse before sealing.'
          % (', '.join(applied) or 'already present'))


if __name__ == '__main__':
    main()
