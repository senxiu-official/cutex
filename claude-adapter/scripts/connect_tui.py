#!/usr/bin/env python3
"""Launch the existing cute-codex TUI against this backend in an isolated CODEX_HOME.

This helper is supplied for target-machine integration; the actual cute-codex
binary was not available for end-to-end testing when this demo was produced.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', default='cute-codex')
    p.add_argument('--endpoint', default='ws://127.0.0.1:4501')
    p.add_argument('--model', required=True)
    p.add_argument('--workspace', required=True, type=Path)
    p.add_argument('--token-file', type=Path)
    p.add_argument('--ui-config', type=Path, help='Cutex-owned frontend settings')
    p.add_argument('--resume', help='existing backend thread ID')
    args=p.parse_args()
    token = args.token_file.read_text().strip() if args.token_file else os.environ.get('CUTE_CLAUDE_RPC_TOKEN', '')
    if not token:
        p.error('Provide --token-file or CUTE_CLAUDE_RPC_TOKEN')
    # The exact --remote auth flag should be present on r91. Fail before launch if not.
    help_text = subprocess.run([args.binary, '--help'], capture_output=True, text=True, check=True).stdout
    for flag in ('--remote', '--remote-auth-token-env'):
        if flag not in help_text:
            raise SystemExit(f'Target binary lacks {flag}; consult docs/HANDOFF.md. No unsafe no-auth fallback.')
    cache = Path.home() / '.cache' / 'cute-claude-tui'
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.TemporaryDirectory(prefix='home-', dir=cache) as home:
        cfg = f'''model = {json.dumps(args.model)}
model_provider = "cute-claude"
approval_policy = "never"
sandbox_mode = "danger-full-access"
web_search = "disabled"

# Local UI bootstrap descriptor only. The remote app-server performs all inference.
# The closed loopback port makes accidental local fallback fail rather than bill OpenAI.
[model_providers.cute-claude]
name = "Claude app-server UI descriptor"
wire_api = "responses"
base_url = "http://127.0.0.1:1"
requires_openai_auth = false

[projects.{json.dumps(str(args.workspace.resolve()))}]
trust_level = "trusted"
'''
        ui = json.loads(args.ui_config.read_text()) if args.ui_config else None
        if ui:
            cfg += '\n[tui]\nstatus_line = ' + json.dumps(ui['statusLine']) + '\nstatus_line_use_colors = ' + str(ui['useColors']).lower() + '\n'
        Path(home, 'config.toml').write_text(cfg)
        env = {**os.environ, 'CODEX_HOME': home, 'CUTE_CLAUDE_RPC_TOKEN': token}
        command = [args.binary, '--remote', args.endpoint, '--remote-auth-token-env', 'CUTE_CLAUDE_RPC_TOKEN',
                   '-m', args.model, '-C', str(args.workspace.resolve())]
        if ui:
            env['CUTEX_NOTIFICATION_CONTROL'] = ui['cutexBinary']
            env['CUTEX_STATUS_LINE_CONFIG'] = ui['preferencesPath']
            env.setdefault('COLORTERM', 'truecolor')
            command += ['--status-items-file', ui['statusFile']]
        if args.resume:
            command += ['resume', args.resume]
        raise SystemExit(subprocess.call(command, env=env))


if __name__ == '__main__':
    main()
