"""Claude Code `apiKeyHelper` for cutex API accounts.

Usage: python -I api_key_helper.py <key-file>

Prints the `apiKey` of one owner-private cutex key file (JSON) to stdout and
nothing else. The official runtime runs this command itself and sends the
output as its API credential, so the key never sits in the runtime's
environment, which the agent's Bash tool, hooks and MCP stdio servers inherit
(and which Claude Code's shell snapshots could capture).

Standalone on purpose (stdlib only, no package imports): it runs under `-I`
from its file path, outside the adapter's import path. Diagnostics go to
stderr and never contain the key.
"""
import json
import sys

MAX_API_KEY_BYTES = 4096  # Same bound as subscription_auth.load_api_account.


def read_key(path: str) -> str | None:
    try:
        with open(path, encoding='utf-8') as source:
            key = json.load(source).get('apiKey')
    except (OSError, ValueError, AttributeError):
        return None
    if not isinstance(key, str):
        return None
    key = key.strip()
    if not key or len(key) > MAX_API_KEY_BYTES or any(c.isspace() for c in key):
        return None
    return key


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print('usage: api_key_helper.py <key-file>', file=sys.stderr)
        return 2
    key = read_key(argv[1])
    if key is None:
        print('cutex API account key file is unreadable or holds no usable key', file=sys.stderr)
        return 1
    sys.stdout.write(key)
    sys.stdout.flush()
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
