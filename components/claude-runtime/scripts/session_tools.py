#!/usr/bin/env python3
"""Inspect the native tool inventory and configured exclusions of a local Claude session."""
import argparse
import asyncio
import json
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1]/'src'))
from cute_claude.client import Client

async def run(args):
    sessions=json.loads((Path.home()/'.cutex/cutex-sessions.json').read_text())['sessions']
    matches=[row for key,row in sessions.items() if args.session in (
        key,row.get('formal_agent_name'),row.get('thread_name'),row.get('display_name_hint'))]
    if len(matches)!=1 or not matches[0].get('claude_session'):
        raise SystemExit('Select one local Claude session by exact name or Cutex session ID.')
    session=matches[0]['claude_session'];state=Path(session['state_dir'])
    ready=json.loads((state/'server.json').read_text())
    client=await Client.open(ready['endpoint'],(state/'rpc-token').read_text().strip())
    try:
        params={'threadId':session['thread_id']}
        print(json.dumps(await client.call('claude/tools/list',params),ensure_ascii=False,indent=2))
    finally:await client.close()

if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('session')
    asyncio.run(run(p.parse_args()))
