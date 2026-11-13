"""Recover context certificates from the selected native request/output chain.

Local prompt items and synthetic quota errors are not context proof. Recovery
never submits input or starts a turn. Bounds fail closed to normal uncertainty.
"""
from __future__ import annotations

from collections import OrderedDict
import json

MAX_BYTES = 256 * 1024 * 1024
MAX_LINE_BYTES = 16 * 1024 * 1024
MAX_BLOBS = 100_000
MAX_SEQUENCE = 20_000


def frames(value):
    """Only text within user-role request messages can carry an input frame."""
    if isinstance(value, str):
        offset = 0
        marker = 'EXTERNAL_INPUT_JSON\n'
        while (offset := value.find(marker, offset)) >= 0:
            offset += len(marker)
            try:
                frame, _ = json.JSONDecoder().raw_decode(value[offset:])
                if isinstance(frame, dict):
                    yield frame
            except ValueError:
                pass
    elif isinstance(value, list):
        for part in value:
            yield from frames(part)
    elif isinstance(value, dict):
        # Tool results may contain the PostToolUse additional context, but
        # tool-call arguments and assistant text are not input delivery proof.
        if value.get('type') == 'text':
            yield from frames(value.get('text'))
        elif value.get('type') == 'tool_result':
            yield from frames(value.get('content'))


def recover(path, session_id, expected, mapped_outputs, bindings):
    """Return message -> (carrying turn, request id, native output UUID).

    `expected` contains exact frames constructed from durable inbox payloads;
    `mapped_outputs` contains SDK UUID -> visible, non-reverted cutex turn.
    """
    if path.stat().st_size > MAX_BYTES:
        return {}
    blobs, parents, sequences, candidates, origins = {}, {}, OrderedDict(), {}, {}
    latest = None
    active_request = None
    total = 0
    with path.open('rb') as source:
        while line := source.readline(MAX_LINE_BYTES + 1):
            total += len(line)
            if len(line) > MAX_LINE_BYTES or total > MAX_BYTES:
                return {}
            if not line.endswith(b'\n'):
                break  # A native writer may still be appending its last record.
            entry = json.loads(line)
            if not isinstance(entry, dict) or entry.get('sessionId') != session_id or entry.get('isSidechain'):
                continue
            kind = entry.get('type')
            native_id = entry.get('uuid')
            if kind in {'user', 'assistant', 'system'} and isinstance(native_id, str):
                parents[native_id] = entry.get('parentUuid')
                latest = native_id
            if kind == 'user':
                active_request = None
                present = list(frames((entry.get('message') or {}).get('content')))
                for mid, frame in expected.items():
                    if native_id in bindings.get(mid, {}).get('user_uuids', ()) and frame in present:
                        origins[mid] = native_id
            if kind == 'api-request-blob':
                message = entry.get('message') or {}
                matched = set()
                if message.get('role') == 'user':
                    present = list(frames(message.get('content')))
                    matched = {mid for mid, frame in expected.items() if frame in present}
                blobs[entry['hash']] = matched
                if len(blobs) > MAX_BLOBS:
                    return {}
            elif kind == 'api-request':
                active_request = None
                if entry.get('querySource') != 'sdk':
                    continue
                base, keep, tail = entry.get('base'), entry.get('keep'), entry.get('tail')
                if type(keep) is not int or not isinstance(tail, list) or not all(isinstance(h, str) for h in tail):
                    continue
                prefix = sequences.get(base, []) if base is not None else []
                if (base is not None and base not in sequences) or not 0 <= keep <= len(prefix):
                    continue
                sequence = prefix[:keep] + tail
                if len(sequence) > MAX_SEQUENCE or not all(h in blobs for h in sequence):
                    continue
                request_id = entry.get('id')
                if not isinstance(request_id, str):
                    continue
                sequences[request_id] = sequence
                while len(sequences) > 8:
                    sequences.popitem(last=False)
                active_request = (request_id, set().union(*(blobs[h] for h in sequence)))
            elif kind == 'assistant':
                request = active_request
                active_request = None
                if not request or native_id not in mapped_outputs:
                    continue
                message = entry.get('message') or {}
                usage = message.get('usage') or {}
                tokens = [usage.get(k, 0) for k in ('input_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens')]
                if (entry.get('isApiErrorMessage') or entry.get('error') or entry.get('apiError')
                        or not isinstance(entry.get('requestId'), str)
                        or not isinstance(message.get('model'), str) or message['model'] == '<synthetic>'
                        or not all(type(n) is int and n >= 0 for n in tokens) or sum(tokens) == 0
                        or not message.get('content')):
                    continue
                for mid in request[1]:
                    binding = bindings.get(mid, {})
                    turn = mapped_outputs[native_id]
                    origin = origins.get(mid)
                    if origin is not None or binding.get('context_turn') == turn:
                        candidates[mid] = (turn, request[0], native_id, origin)
    # Only the selected main chain proves the current context. Reverted and
    # abandoned branches retain audit text without becoming delivery receipts.
    chain = set()
    while latest is not None:
        if latest in chain or latest not in parents:
            return {}
        chain.add(latest)
        latest = parents[latest]
    return {mid: proof[:3] for mid, proof in candidates.items()
            if proof[2] in chain and (proof[3] is None or proof[3] in chain)}
