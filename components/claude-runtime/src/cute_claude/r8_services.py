"""Read-only local usage aggregation in the native account/usage/read shape."""
from __future__ import annotations

import json
from collections import defaultdict
from datetime import date, datetime, timedelta, timezone

from .errors import require, text


def read_usage(store, params: dict, *, today: date | None = None) -> dict:
    require(not set(params) - {'threadId'}, 'unsupported account/usage/read fields')
    tid = params.get('threadId')
    if tid is not None:
        store.thread(text(tid, 'threadId', limit=256))
    rows = store.db.execute('SELECT id,thread_id,started,completed,usage FROM turns WHERE (? IS NULL OR thread_id=?)', (tid, tid))
    buckets: dict[str, int] = defaultdict(int)
    durations = []
    seen: set[str] = set()
    for row in rows:
        try:
            usage = json.loads(row['usage'])
        except (ValueError, TypeError):
            continue
        origin = usage.get('sourceTurnId', row['id'])
        if origin in seen:
            continue
        seen.add(origin)
        calls = usage.get('modelRequests', [])
        if not isinstance(calls, list):
            continue
        total, reported = 0, False
        for call in calls:
            counts = call.get('usage') if isinstance(call, dict) else None
            if not isinstance(counts, dict):
                continue
            reported = True
            for key in ('input_tokens', 'output_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens'):
                count = counts.get(key)
                if type(count) is int and count >= 0:
                    total += count
        if not reported:
            continue
        day = datetime.fromtimestamp(row['started'], timezone.utc).date().isoformat()
        buckets[day] += total
        if row['completed'] is not None:
            durations.append(max(0, int(row['completed'] - row['started'])))
    today = today or datetime.now(timezone.utc).date()
    days = sorted(date.fromisoformat(day) for day in buckets)
    longest = streak = 0
    previous = None
    for day in days:
        streak = streak + 1 if previous is not None and day == previous + timedelta(days=1) else 1
        longest = max(longest, streak)
        previous = day
    current = 0
    active = set(days)
    day = today if today in active else today - timedelta(days=1)
    while day in active:
        current += 1
        day -= timedelta(days=1)
    return {'summary': {'lifetimeTokens': sum(buckets.values()) if buckets else None,
                        'peakDailyTokens': max(buckets.values()) if buckets else None,
                        'longestRunningTurnSec': max(durations) if durations else None,
                        'currentStreakDays': current if buckets else None,
                        'longestStreakDays': longest if buckets else None},
            'dailyUsageBuckets': [{'startDate': day, 'tokens': tokens} for day, tokens in sorted(buckets.items())],
            # Native credit prices do not apply to Claude; do not invent cost estimates.
            'threadUsage': None}
