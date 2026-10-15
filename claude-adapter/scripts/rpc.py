#!/usr/bin/env python3
"""Send one controller RPC with the token loaded from a file or environment."""
import argparse
import asyncio
import json
import os
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1]/'src'))
from cute_claude.client import Client


async def run(args):
    token = args.token_file.read_text().strip() if args.token_file else os.environ.get('CUTE_CLAUDE_RPC_TOKEN', '')
    if not token:
        raise SystemExit('Set CUTE_CLAUDE_RPC_TOKEN or --token-file')
    params = json.loads(args.params_file.read_text() if args.params_file else args.params)
    client = await Client.open(args.endpoint, token)
    try:
        result = await client.call(args.method, params)
        print(json.dumps(result, ensure_ascii=False, indent=2))
    finally:
        await client.close()


if __name__ == '__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('method')
    p.add_argument('--endpoint', default='ws://127.0.0.1:4501')
    p.add_argument('--token-file', type=Path)
    p.add_argument('--params', default='{}')
    p.add_argument('--params-file', type=Path)
    asyncio.run(run(p.parse_args()))
