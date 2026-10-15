#!/usr/bin/env python3
"""Validate a captured transcript against JSON schemas from a local r91 checkout.

Does not download schemas, invent missing schemas, or claim full TUI compatibility.
Only standard messages actually found in the trace are checked; claude/* is a new
extension. Exit nonzero for any missing schema, unmapped method or invalid message.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

import jsonschema

RPC = {
    'initialize': ('v1/InitializeParams', 'v1/InitializeResponse'),
    'model/list': ('v2/ModelListParams', 'v2/ModelListResponse'),
    'config/batchWrite': ('v2/ConfigBatchWriteParams', 'v2/ConfigWriteResponse'),
    'account/read': ('v2/GetAccountParams', 'v2/GetAccountResponse'),
    'config/read': ('v2/ConfigReadParams', 'v2/ConfigReadResponse'),
    'configRequirements/read': ('v2/ConfigRequirementsReadParams', 'v2/ConfigRequirementsReadResponse'),
    'thread/settings/update': ('v2/ThreadSettingsUpdateParams', 'v2/ThreadSettingsUpdateResponse'),
    'thread/start': ('v2/ThreadStartParams', 'v2/ThreadStartResponse'),
    'thread/resume': ('v2/ThreadResumeParams', 'v2/ThreadResumeResponse'),
    'thread/unsubscribe': ('v2/ThreadUnsubscribeParams', 'v2/ThreadUnsubscribeResponse'),
    'thread/read': ('v2/ThreadReadParams', 'v2/ThreadReadResponse'),
    'thread/list': ('v2/ThreadListParams', 'v2/ThreadListResponse'),
    'turn/start': ('v2/TurnStartParams', 'v2/TurnStartResponse'),
    'turn/interrupt': ('v2/TurnInterruptParams', 'v2/TurnInterruptResponse'),
}
NOTIFICATIONS = {
    'thread/settings/updated': 'v2/ThreadSettingsUpdatedNotification',
    'thread/started': 'v2/ThreadStartedNotification',
    'thread/status/changed': 'v2/ThreadStatusChangedNotification',
    'thread/tokenUsage/updated': 'v2/ThreadTokenUsageUpdatedNotification',
    'turn/started': 'v2/TurnStartedNotification',
    'turn/completed': 'v2/TurnCompletedNotification',
    'item/started': 'v2/ItemStartedNotification',
    'item/completed': 'v2/ItemCompletedNotification',
    'item/agentMessage/delta': 'v2/AgentMessageDeltaNotification',
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--schema-dir', required=True, type=Path,
                        help='r91 codex-rs/app-server-protocol/schema/json directory')
    parser.add_argument('--trace', type=Path, default=Path(__file__).resolve().parents[1]/'docs/demo-trace.jsonl')
    args = parser.parse_args()
    if not args.schema_dir.is_dir():
        parser.error('schema directory does not exist; no schemas were validated')
    requests, validators = {}, {}
    checked = extension = skipped = failures = 0

    def validate(name: str, payload: object, line: int) -> None:
        nonlocal checked, failures
        path = args.schema_dir / (name + '.json')
        if not path.is_file():
            failures += 1
            print(f'FAIL line {line}: schema missing: {path}')
            return
        try:
            if name not in validators:
                schema = json.loads(path.read_text(encoding='utf-8'))
                cls = jsonschema.validators.validator_for(schema)
                cls.check_schema(schema)
                validators[name] = cls(schema)
            validators[name].validate(payload)
            checked += 1
        except Exception as exc:
            failures += 1
            # Avoid dumping full private message payloads in validation output.
            location = '/'.join(str(p) for p in getattr(exc, 'absolute_path', []))
            print(f'FAIL line {line}: {name} at {location or "<root>"}: {type(exc).__name__}')

    for number, raw in enumerate(args.trace.read_text(encoding='utf-8').splitlines(), 1):
        entry = json.loads(raw)
        message = entry['message']
        method = message.get('method')
        if entry['direction'] == 'client->server':
            if 'id' in message:
                requests[message['id']] = method
            if method == 'initialized':
                skipped += 1  # Empty notification; no payload schema needed.
            elif method and method.startswith('claude/'):
                extension += 1
            elif method in RPC:
                params = dict(message.get('params') or {})
                if method == 'initialize':
                    # Custom extension opt-in isn't part of the upstream schema.
                    params['capabilities'] = dict(params.get('capabilities') or {})
                    params['capabilities'].pop('claudeExtensions', None)
                validate(RPC[method][0], params, number)
            else:
                failures += 1
                print(f'FAIL line {number}: unmapped request/notification {method}')
        elif 'id' in message:
            method = requests.get(message['id'])
            if method and method.startswith('claude/'):
                extension += 1
            elif 'error' in message:
                skipped += 1  # Generic RPC error envelope is not a success schema.
            elif method in RPC:
                validate(RPC[method][1], message.get('result'), number)
            else:
                failures += 1
                print(f'FAIL line {number}: no schema mapping for response id={message["id"]}')
        elif method and method.startswith('claude/'):
            extension += 1
        elif method in NOTIFICATIONS:
            validate(NOTIFICATIONS[method], message.get('params'), number)
        else:
            failures += 1
            print(f'FAIL line {number}: unmapped server notification {method}')
    print(f'Validated standard payloads: {checked}; extension messages: {extension}; explicit skips: {skipped}; failures: {failures}')
    print('This checks captured payloads only, not the complete TUI bootstrap or CUTEX lifecycle.')
    return 1 if failures or checked == 0 else 0


if __name__ == '__main__':
    sys.exit(main())
