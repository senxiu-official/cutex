"""Read-only runtime metrics; no prompts, native history replay or automatic start."""
import asyncio
import json
import time

class RuntimeMetrics:
    def __init__(self, engine):
        self.engine = engine
        self.context = {}
        self.last_refresh = {}
        self.lock = asyncio.Lock()

    def owner(self, tid):
        return getattr(getattr(self.engine, 'driver', None), 'owners', {}).get(tid)

    async def refresh_context(self, tid, *, expected_turn_id=None):
        owner = self.owner(tid)
        if owner is None or owner.active is not None:
            return
        row = self.engine.store.db.execute('SELECT id,usage FROM turns WHERE thread_id=? AND reverted=0 ORDER BY started DESC,id DESC LIMIT 1', (tid,)).fetchone()
        turn_id = row['id'] if row else None
        if expected_turn_id is not None and turn_id != expected_turn_id:
            return
        binding = (owner.session_id, owner.connection_id, owner.native_connection_generation)
        try:
            raw = await owner.read_metrics('context_usage')
            latest = self.engine.store.db.execute('SELECT id FROM turns WHERE thread_id=? AND reverted=0 ORDER BY started DESC,id DESC LIMIT 1', (tid,)).fetchone()
            if (self.owner(tid) is not owner or owner.active is not None
                    or binding != (owner.session_id, owner.connection_id, owner.native_connection_generation)
                    or (latest['id'] if latest else None) != turn_id):
                return
            used, maximum = raw.get('totalTokens'), raw.get('maxTokens')
            if type(used) is not int or type(maximum) is not int or used < 0 or maximum <= 0:
                return
            self.context[tid] = {'used':used,'maximum':maximum}
            calls = json.loads(row['usage']).get('modelRequests',[]) if row else []
            await self.engine._emit_usage(tid, row['id'] if row else '', calls)
        except Exception:
            # This observational failure must never fail or pause a turn.
            return

    async def refresh_status(self, tids):
        async with self.lock:
            owners = [tid for tid in sorted(tids) if self.owner(tid) is not None]
            if not owners:
                return
            tid = owners[0]
            now = time.monotonic()
            if now - self.last_refresh.get(tid, -100) < 3:
                return
            self.last_refresh[tid] = now
            try:
                raw = await self.owner(tid).read_metrics('plan_usage')
                self.engine.rate_limits.observe_usage(raw)
            except Exception:
                pass
            await self.refresh_context(tid)
