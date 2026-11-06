"""S7 session fixtures on the unmodified scale-gate runner at 55b271e2.

Usage: python tools/s7_scale_gate.py --gate-dir <pinned tools/scale_gate> <run.py args>
The shared run retains all routes, visibility validation, memory and concurrency
checks. This adapter adds both independent v1 inputs to its session population.
"""
import argparse
import json
import hashlib
from pathlib import Path
import sys


def seed(home, *, history_multiplier=1):
    root = Path(home) / '.cutex'
    sessions = json.loads((root / 'cutex-sessions.json').read_text())['sessions']
    # Real bounded previews and 64-hex fingerprints, never body/whitespace
    # padding. Independent historical populations reach each audited envelope.
    factor = len(sessions) / 166 * history_multiplier
    def keys(count):
        actual = list(sessions)
        total = max(len(actual), round(count * factor))
        return actual + [f'cutex.s7-inactive-{i:06}' for i in range(total-len(actual))]
    activities, projections = {}, {}
    at = '2026-11-04T01:02:03Z'
    for number, sid in enumerate(keys(470)):
        activities[sid] = dict(
            revision=number + 1, runtimeGeneration=1, lastConversationAt=at,
            lastOutputAt=at, lastOutputCompletedAt=at, lastTurnCompletedAt=at,
            lastTurnCompletedGeneration=1, lastFileChangeAt=at, lastTurnStartedAt=at,
            lastOutput=dict(association=dict(cutex_session_id=sid), **{'class':'final_visible'},
                display_text='Synthetic activity. ' * 25, updated_at=at, runtime_generation=1),
            lastOutputOrder=dict(runtimeGeneration=1, streamId='s7-fixture', sequence=1, receivedAt=at),
        )
    for number, sid in enumerate(keys(1300)):
        projections[sid] = dict(revision=number + 1, fingerprint=hashlib.sha256(sid.encode()).hexdigest(),
            focus=dict(revision=1, owner='pc', mobileMuted=False,
                source='synthetic-desktop', updatedAt=at))
    target = root / 'runtime/management-v2'
    result = {}
    for store, value in [('session-activity', dict(version=1, sessions=activities, deletedSessions=['cutex.s7-deleted'])),
                         ('session-projection', dict(version=1, sessions=projections))]:
        path = target / (store + '-state.json')
        path.write_text(json.dumps(value, separators=(',', ':')), encoding='utf-8')
        result[store] = dict(rows=len(value["sessions"]), related_session_rows=len(sessions), source_bytes=path.stat().st_size)
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument('--gate-dir', type=Path, required=True)
    args, rest = parser.parse_known_args()
    sys.path.insert(0, str(args.gate_dir.resolve()))
    import run
    original = run.generate

    def generate(*args, **kwargs):
        result = original(*args, **kwargs)
        result['s7'] = seed(args[0])
        return result

    run.generate = generate
    previous_validator=run.validate_response
    def validate_response(path,result,expected,anchor):
        if path=='/v2/sessions/cutex.scale-00000':
            if result.get('cutexSessionId')!='cutex.scale-00000' or result.get('activity',{}).get('lastConversationAt')!='2026-11-04T01:02:03Z':
                raise RuntimeError('S7 selected activity result mismatch')
            revision=result.get('focus',{}).get('revision')
            if not isinstance(revision,int) or revision<1:
                raise RuntimeError('S7 selected focus revision missing')
            return dict(id=result['cutexSessionId'],focus_revision=revision,last_conversation_at=result['activity']['lastConversationAt'])
        return previous_validator(path,result,expected,anchor)
    run.validate_response=validate_response
    run.CASES = [*run.CASES, ('s7-session-point', '/v2/sessions/cutex.scale-00000', None)]
    sys.argv = [str(args.gate_dir / 'run.py'), *rest]
    sys.exit(run.main())
