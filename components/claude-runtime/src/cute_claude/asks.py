"""Asks (cutex 委托) on the local inbox.

cutex submits ASK / ANSWER / QUESTION events through `claude/input/submit`
with `admission: "exclusive_turn"`: the input must start its own turn now, or
be refused without being queued (`externalInputError: "busy"`, the same shape
as cute-codex N1), so cutex keeps its own FIFO with the owner's queue. The
receipt's `turnId` (allocated at enqueue) is cutex's attempt <-> turn mapping.
Metadata carries the ask ids (`asks`) and, for an ASK, the `attempt`.
"""
from __future__ import annotations

from .errors import RpcError, require

ADMISSION = 'exclusive_turn'
EVENT_TYPES = ('ASK', 'ANSWER', 'QUESTION')
MAX_ASKS = 64


def validate_metadata(event_type: str, metadata: dict) -> None:
    """Ask metadata is checked so a bad id can't reach the model framing."""
    if event_type not in EVENT_TYPES:
        return
    asks = metadata.get('asks', [])
    require(isinstance(asks, list) and len(asks) <= MAX_ASKS
            and all(isinstance(a, str) and 0 < len(a) <= 256 for a in asks), 'metadata.asks must be a list of ask ids')
    attempt = metadata.get('attempt')
    require(attempt is None or (isinstance(attempt, int) and not isinstance(attempt, bool) and attempt > 0),
            'metadata.attempt must be a positive integer')


def busy_reason(server, tid: str) -> str | None:
    """Why an exclusive input can't start its own turn now (None: it can)."""
    thread = server.store.thread(tid)
    if thread['halted'] or tid in getattr(server.engine, 'offline_threads', set()):
        return 'gate_closed'
    if server.store.pending(tid):
        return 'active_turn'
    if server.approvals.planning(tid):
        return 'plan_mode'
    if getattr(server.engine, 'closing', False):
        return 'server_draining'
    return None


def refuse_busy(reason: str) -> RpcError:
    return RpcError(-32600, f'thread busy ({reason}); the exclusive turn was not queued',
                    {'externalInputError': 'busy', 'reason': reason})


def ask_ref(payload: dict) -> dict | None:
    """The ask ids of an ask event (model framing and receipts)."""
    if payload.get('eventType') not in EVENT_TYPES:
        return None
    metadata = payload.get('metadata') or {}
    ref = {'asks': list(metadata.get('asks') or [])}
    if metadata.get('attempt') is not None:
        ref['attempt'] = metadata['attempt']
    return ref


def title(payload: dict, name: str, stamp: str) -> str | None:
    """Presentation title of an ask event (None: not an ask event)."""
    ref = ask_ref(payload)
    if ref is None:
        return None
    asks = ', '.join(ref['asks'][:3]) + (' …' if len(ref['asks']) > 3 else '')
    kind = payload['eventType']
    if kind == 'ASK':
        attempt = f' · attempt {ref["attempt"]}' if ref.get('attempt') else ''
        return f'委托 from {name} · {asks}{attempt} · {stamp}'
    label = 'Answers' if kind == 'ANSWER' else 'Questions'
    return f'委托 {label} · {asks} · {stamp}'
