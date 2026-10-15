"""Persist bounded parent-assistant model evidence, never infer alias resolution.

This optional schema-2 extension is an audit projection. It is deliberately not
copied into forks, and reverted turns cannot supply a visible observation.
"""
from __future__ import annotations

import json
import re
import time

from .store import dump


SOURCE = 'parentAssistantMessage.model'
MAX_MODELS = 32
_MODEL_ID = re.compile(
    r'(?:(?:(?:us|eu|apac|global)\.)?anthropic\.)?'
    r'claude-[a-z0-9]+(?:[._:@-][a-z0-9]+)*'
    r'|(?:opus|sonnet|haiku|fable)(?:\[1m\])?'
)


def model_identifier(value) -> str | None:
    """Unknown free text is not diagnostic data and must not be retained."""
    if isinstance(value, str) and len(value) <= 160 and _MODEL_ID.fullmatch(value):
        return value
    return None


class ModelObservations:
    def __init__(self, store):
        self.store = store
        store.db.execute('''CREATE TABLE IF NOT EXISTS model_observations(
            turn_id TEXT PRIMARY KEY REFERENCES turns(id) ON DELETE CASCADE,
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            requested_model TEXT NOT NULL, connection_id TEXT NOT NULL,
            connection_generation INTEGER NOT NULL, selection_id TEXT NOT NULL,
            observed_models TEXT NOT NULL, last_observed_model TEXT NOT NULL,
            observed_at_ms INTEGER NOT NULL, models_truncated INTEGER NOT NULL DEFAULT 0)''')
        if 'models_truncated' not in {row[1] for row in store.db.execute('PRAGMA table_info(model_observations)')}:
            store.db.execute('ALTER TABLE model_observations ADD COLUMN models_truncated INTEGER NOT NULL DEFAULT 0')
        store.db.execute('''CREATE INDEX IF NOT EXISTS model_observations_thread
            ON model_observations(thread_id)''')

    def record(self, tid: str, turn_id: str, model, *, requested_model: str,
               connection_id: str, connection_generation: int, selection_id: str) -> bool:
        model = model_identifier(model)
        if model is None:
            return False
        with self.store.transaction():
            turn = self.store.db.execute('''SELECT 1 FROM turns
                WHERE id=? AND thread_id=? AND reverted=0 AND state='inProgress' ''',
                (turn_id, tid)).fetchone()
            if turn is None:
                return False
            old = self.store.db.execute('SELECT * FROM model_observations WHERE turn_id=?',
                                        (turn_id,)).fetchone()
            context = (requested_model, connection_id, connection_generation, selection_id)
            if old:
                # One native turn never crosses a connection or model selection.
                if tuple(old[key] for key in ('requested_model', 'connection_id',
                        'connection_generation', 'selection_id')) != context:
                    return False
                if old['last_observed_model'] == model:
                    return False
                models = json.loads(old['observed_models'])
                truncated = bool(old['models_truncated'])
            else:
                models = []
                truncated = False
            if model not in models:
                if len(models) >= MAX_MODELS:
                    models = models[-(MAX_MODELS - 1):]
                    truncated = True
                models.append(model)
            self.store.db.execute('''INSERT INTO model_observations VALUES(?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(turn_id) DO UPDATE SET observed_models=excluded.observed_models,
                    last_observed_model=excluded.last_observed_model,
                    observed_at_ms=excluded.observed_at_ms, models_truncated=excluded.models_truncated''',
                (turn_id, tid, *context, dump(models), model, int(time.time() * 1000), int(truncated)))
        return True

    def status(self, tid: str, owner=None) -> dict:
        requested = self.store.thread(tid)['model']
        result = {'threadId': tid, 'requestedModel': requested,
                  'observedForRequestedModel': None, 'observedModels': [],
                  'observedModelsTruncated': False,
                  'lastObservedModel': None, 'source': None, 'turnId': None,
                  'turnStatus': None, 'observedAtMs': None, 'current': False,
                  'freshness': 'unobserved', 'staleReason': None,
                  'confirmationScope': 'observedMessagesOnly', 'appliesToNextTurn': False}
        row = self.store.db.execute('''SELECT o.*, t.state AS turn_state FROM model_observations o
            JOIN turns t ON t.id=o.turn_id AND t.thread_id=o.thread_id
            WHERE o.thread_id=? AND t.reverted=0 ORDER BY t.started DESC, t.rowid DESC LIMIT 1''',
            (tid,)).fetchone()
        if row is None:
            return result
        result.update(observedForRequestedModel=row['requested_model'],
                      observedModels=json.loads(row['observed_models']),
                      observedModelsTruncated=bool(row['models_truncated']),
                      lastObservedModel=row['last_observed_model'], source=SOURCE,
                      turnId=row['turn_id'], turnStatus=row['turn_state'],
                      observedAtMs=row['observed_at_ms'], freshness='historical')
        latest = self.store.db.execute('''SELECT id FROM turns WHERE thread_id=? AND reverted=0
            ORDER BY started DESC, rowid DESC LIMIT 1''', (tid,)).fetchone()
        if owner is None or owner.task.done() or owner.state not in {'waiting', 'working'}:
            reason = 'runtimeOffline'
        elif requested != row['requested_model']:
            reason = 'requestedModelChanged'
        elif (owner.connection_id != row['connection_id'] or
              owner.native_connection_generation != row['connection_generation']):
            reason = 'connectionChanged'
        elif owner.model_selection_id != row['selection_id']:
            reason = 'modelSelectionChanged'
        elif latest is None or latest[0] != row['turn_id']:
            reason = 'newerTurnUnobserved'
        else:
            reason = None
        result['staleReason'] = reason
        if reason is None:
            result['current'] = True
            active = owner.active
            result['freshness'] = 'activeTurn' if active and active.turn_id == row['turn_id'] else 'latestTurn'
        return result
