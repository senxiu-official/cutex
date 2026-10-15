"""Official-runtime authentication only; never read/copy OAuth credential files.

Run `python -m cute_claude.subscription_auth login` on the user's own machine.
The login command inherits the terminal and delegates to the unmodified official
binary. The adapter only reads the official CLI's NON-SECRET auth-status JSON.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Mapping


class SubscriptionSetupError(ValueError):
    pass


# Reject ambiguous billing/credential routing in this optional backend. This is
# an application preflight, NOT a modification of the official binary or login.
CONFLICTING_ENV = (
    'ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL',
    'ANTHROPIC_CUSTOM_HEADERS', 'CLAUDE_CODE_OAUTH_TOKEN',
    'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY',
    'CLAUDE_CODE_USE_ANTHROPIC_AWS', 'CLAUDE_CODE_USE_MANTLE',
)


def check_subscription_environment(env: Mapping[str, str] | None = None) -> None:
    env = os.environ if env is None else env
    conflicts = [key for key in CONFLICTING_ENV if env.get(key, '').strip()]
    if conflicts:
        raise SubscriptionSetupError(
            'Subscription backend will not use API/provider/token overrides. Unset: '
            + ', '.join(conflicts) + '. Values are never printed; no API fallback.')


# API accounts (cutex `claude_accounts`, launch option --api-account-config):
# the key reaches only the official runtime process of this adapter, through
# Claude Code's `apiKeyHelper` setting (api_key_helper.py reads the key file).
# It is never put in any environment: the runtime's env is inherited by the
# agent's Bash tool, hooks and MCP stdio servers and captured by shell
# snapshots. The adapter's own environment stays free of CONFLICTING_ENV
# (checked as for subscriptions).
API_KEY_ENVS = ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN')
API_ENV_ALLOWED = (
    'ANTHROPIC_DEFAULT_OPUS_MODEL', 'ANTHROPIC_DEFAULT_SONNET_MODEL', 'ANTHROPIC_DEFAULT_HAIKU_MODEL',
    'ANTHROPIC_DEFAULT_FABLE_MODEL', 'ANTHROPIC_SMALL_FAST_MODEL', 'CLAUDE_CODE_SUBAGENT_MODEL',
    'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC', 'CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS',
    'DISABLE_TELEMETRY', 'DISABLE_ERROR_REPORTING', 'DISABLE_PROMPT_CACHING',
    'CLAUDE_CODE_MAX_OUTPUT_TOKENS', 'MAX_THINKING_TOKENS',
)
MAX_API_KEY_BYTES = 4096


HELPER_SCRIPT = Path(__file__).with_name('api_key_helper.py')
# Characters that would need shell-specific escaping in the helper command.
UNQUOTABLE = set('"$`\'\\\n\r%!')


def helper_arg(value: str | Path) -> str:
    """One helper command word. Claude Code runs the helper through a shell;
    forward slashes and (only when needed) double quotes read the same under
    sh, bash and cmd.exe. Refuse what would need shell-specific escaping."""
    text = Path(value).as_posix()
    if any(c in UNQUOTABLE for c in text):
        raise SubscriptionSetupError('API account key helper path holds a character that cannot be quoted safely. Values are never printed.')
    return f'"{text}"' if any(c.isspace() or c in "&|<>()^;'*?[]{}#~" for c in text) else text


class ApiAccount:
    """One API account: key env name, optional base URL, extra settings, the
    owner-private key file and the key read from it (never printed)."""
    __slots__ = ('account_id', 'key_env', 'base_url', 'extra_env', '_key', 'key_file')

    def __init__(self, account_id: str, key_env: str, base_url: str | None, extra_env: dict[str, str], key: str,
                 key_file: str | Path | None = None):
        self.account_id, self.key_env, self.base_url, self.extra_env, self._key = account_id, key_env, base_url, extra_env, key
        self.key_file = Path(key_file).resolve() if key_file else None

    def __repr__(self) -> str:
        return f'ApiAccount({self.account_id!r}, key_env={self.key_env!r}, base_url={self.base_url!r}, key=<redacted>)'

    def environment(self) -> dict[str, str]:
        """Runtime env for this account: base URL and allowed settings only.
        The key is NOT in it (see cli_settings)."""
        env = dict(self.extra_env)
        if self.base_url:
            env['ANTHROPIC_BASE_URL'] = self.base_url
        return env

    def cli_settings(self, python: str | Path | None = None) -> dict[str, str]:
        """Flag settings for the official runtime: `apiKeyHelper` prints the key
        from the key file. Claude Code sends the helper's output both as
        `x-api-key` and as `Authorization: Bearer`, covering either keyEnv."""
        if self.key_file is None:
            raise SubscriptionSetupError('API account has no key file for the key helper. Values are never printed.')
        command = [helper_arg(python or sys.executable), '-I', helper_arg(HELPER_SCRIPT), helper_arg(self.key_file)]
        return {'apiKeyHelper': ' '.join(command)}

    def redact(self, text: str) -> str:
        return text.replace(self._key, '<redacted>') if self._key else text

    def status(self) -> dict:
        return {'loggedIn': True, 'authMethod': 'api_key', 'accountId': self.account_id,
                'apiProvider': 'anthropicCompatible' if self.base_url else 'firstParty',
                'keyEnv': self.key_env, 'entitlementVerified': False,
                'note': 'API account: the key is checked by the first model request.'}


def load_api_account(path: str | Path) -> ApiAccount:
    """Read the launcher's API account config and its key file."""
    def bad(reason: str) -> SubscriptionSetupError:
        return SubscriptionSetupError(f'API account config {path}: {reason}. Values are never printed.')
    try:
        config = json.loads(Path(path).read_text(encoding='utf-8'))
    except (OSError, ValueError):
        raise bad('unreadable') from None
    if not isinstance(config, dict) or config.get('version') != 1:
        raise bad('unsupported version')
    account_id = config.get('accountId')
    key_env = config.get('keyEnv') or 'ANTHROPIC_API_KEY'
    base_url = config.get('baseUrl')
    extra = config.get('env') or {}
    if not isinstance(account_id, str) or not account_id:
        raise bad('accountId missing')
    if key_env not in API_KEY_ENVS:
        raise bad('keyEnv must be ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN')
    if base_url is not None and (not isinstance(base_url, str) or not base_url.startswith(('https://', 'http://'))):
        raise bad('baseUrl must be an http(s) URL')
    if not isinstance(extra, dict) or any(k not in API_ENV_ALLOWED or not isinstance(v, str) for k, v in extra.items()):
        raise bad('env holds a setting that is not allowed')
    key_file = config.get('keyFile')
    if not isinstance(key_file, str) or not key_file:
        raise bad('keyFile missing')
    try:
        key = json.loads(Path(key_file).read_text(encoding='utf-8')).get('apiKey')
    except (OSError, ValueError, AttributeError):
        raise bad('key file unreadable') from None
    if not isinstance(key, str) or not key.strip() or len(key.strip()) > MAX_API_KEY_BYTES or any(c.isspace() for c in key.strip()):
        raise bad('no usable API key')
    return ApiAccount(account_id, key_env, base_url, dict(extra), key.strip(), key_file)


def official_cli(explicit: str | Path | None = None) -> Path:
    if explicit:
        path = Path(explicit).expanduser().resolve(strict=True)
        if not path.is_file():
            raise SubscriptionSetupError('--claude-cli must name an unmodified official executable')
        return path
    found = shutil.which('claude')
    if found:
        return Path(found).resolve()
    raise SubscriptionSetupError(
        'Official runtime not found on PATH. Select your one installed unmodified '
        'Claude Code using --claude-cli. The adapter does not select the SDK-bundled copy.')


def resolve_config_dir(explicit: str | Path | None = None) -> Path:
    """Single native account home; no credential copying or per-agent profiles."""
    value = explicit or os.environ.get('CLAUDE_CONFIG_DIR') or (Path.home() / '.cutex-claude-compatible' / '.claude')
    return Path(value).expanduser().resolve()


def official_environment(config_dir: Path | None = None) -> dict[str, str]:
    check_subscription_environment()
    return {**os.environ, 'CLAUDE_CONFIG_DIR': str(resolve_config_dir(config_dir))}


def validate_auth_status(payload: object) -> dict:
    if not isinstance(payload, dict) or payload.get('loggedIn') is not True:
        raise SubscriptionSetupError('Not logged in. Run python -m cute_claude.subscription_auth login on your own machine.')
    # Unknown JSON shape is a compatibility failure, not permission to guess.
    if payload.get('authMethod') != 'claude.ai':
        raise SubscriptionSetupError(
            'Official auth status does not report authMethod=claude.ai. '
            'Select your own Claude subscription/organization through official login. No API fallback.')
    if payload.get('apiProvider') not in (None, 'firstParty') or payload.get('apiKeySource') not in (None, '', 'none'):
        raise SubscriptionSetupError('Official auth status reports API/provider routing; choose your Claude account without API overrides.')
    plan = payload.get('subscriptionType')
    return {'loggedIn': True, 'authMethod': 'claude.ai',
            'subscriptionType': plan if isinstance(plan, str) else None,
            'entitlementVerified': False, 'extraChargesDisabled': False,
            'note': 'Login mode only; organization seat access and billing are not verified.'}


async def check_official_login(cli: Path, config_dir: Path | None = None) -> dict:
    env = official_environment(config_dir)
    # `auth status` is JSON by default. Do not add an undocumented --json flag.
    proc = await asyncio.create_subprocess_exec(str(cli), 'auth', 'status',
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=env)
    try:
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=30)
    except BaseException:
        if proc.returncode is None:
            proc.kill()
        await proc.wait()
        raise
    if proc.returncode != 0:
        raise SubscriptionSetupError('Official auth status failed. Complete official login; no model request or API fallback was attempted.')
    try:
        payload = json.loads(stdout)
    except (ValueError, UnicodeDecodeError):
        raise SubscriptionSetupError('Unexpected official auth-status JSON; check runtime compatibility. Raw output withheld.') from None
    return validate_auth_status(payload)


def main() -> None:
    p = argparse.ArgumentParser(description='Delegate login to the one installed unmodified official Claude runtime')
    p.add_argument('action', choices=['login', 'status', 'path'])
    p.add_argument('--claude-cli', type=Path)
    p.add_argument('--claude-config-dir', type=Path, help='same directory used for login/status/SDK execution')
    args = p.parse_args()
    try:
        cli = official_cli(args.claude_cli)
        config_dir = resolve_config_dir(args.claude_config_dir)
        if args.action == 'path':
            print(cli)
        elif args.action == 'login':
            # Do not intercept the OAuth flow or restrict the binary's auth methods.
            print('Opening the official login flow. Select your own lab-provided Claude account/seat.', file=sys.stderr)
            raise SystemExit(subprocess.call([str(cli), 'auth', 'login'], env=official_environment(config_dir)))
        else:
            print(json.dumps(asyncio.run(check_official_login(cli, config_dir)), ensure_ascii=False, indent=2))
    except (ValueError, OSError) as exc:
        print(f'Subscription setup: {exc}', file=sys.stderr)
        raise SystemExit(2) from None


if __name__ == '__main__':
    main()
