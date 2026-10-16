"""Observed SDK quota windows. Never infer a percentage or account recovery."""
from __future__ import annotations
import math
import time
from .store import dump
import json
import hashlib
from datetime import datetime

LABELS = {'five_hour': ('Claude 5h', 300), 'seven_day': ('Claude 7d', 10080),
          'seven_day_opus': ('Claude Opus 7d', 10080), 'seven_day_sonnet': ('Claude Sonnet 7d', 10080),
          'overage': ('Claude extra usage', None)}

def field(value, key, default=None):
    return value.get(key, default) if isinstance(value, dict) else getattr(value, key, default)

class RateLimits:
    def __init__(self, store):
        self.store = store
        store.db.execute('CREATE TABLE IF NOT EXISTS sdk_rate_limits(bucket TEXT PRIMARY KEY, snapshot TEXT NOT NULL)')

    def observe(self, event):
        info = field(event, 'rate_limit_info') or {}
        kind = field(info, 'rate_limit_type') or field(info, 'rateLimitType') or 'unknown'
        status = field(info, 'status')
        if status not in {'allowed', 'allowed_warning', 'rejected'}:
            return None
        utilization = field(info, 'utilization')
        percent = float(utilization)*100 if type(utilization) in {int,float} and math.isfinite(utilization) and 0 <= utilization <= 1 else None
        reset = field(info, 'resets_at') or field(info, 'resetsAt')
        kind = kind if isinstance(kind, str) and kind in LABELS else 'unknown'
        label, minutes = LABELS.get(kind, ('Claude status', None))
        row = {'id': kind if kind in LABELS else 'unknown', 'label':label, 'status':status,
               'usedPercent':percent, 'resetsAt':reset if type(reset) is int and reset > 0 else None,
               'observedAt':int(time.time()), 'windowMinutes':minutes}
        self.store.db.execute('INSERT OR REPLACE INTO sdk_rate_limits VALUES(?,?)', (row['id'],dump(row)))
        return row

    def observe_usage(self, data):
        """get_usage uses percentages (0..100), unlike RateLimitEvent (0..1)."""
        if not isinstance(data, dict) or type(data.get('rate_limits_available')) is not bool:
            return False
        windows = data.get('rate_limits')
        if windows is None or data['rate_limits_available'] is False:
            self.store.db.execute('DELETE FROM sdk_rate_limits')
            return True
        if not isinstance(windows, dict):
            return False
        rows = []
        candidates = [(key, label, minutes, windows.get(key)) for key,(label,minutes) in LABELS.items() if key != 'overage']
        candidates.append(('seven_day_oauth_apps', 'Claude apps 7d', 10080, windows.get('seven_day_oauth_apps')))
        for value in (windows.get('model_scoped') or [])[:16]:
            if isinstance(value, dict) and isinstance(value.get('display_name'), str):
                name = ''.join(c for c in value['display_name'] if c.isprintable())[:60]
                key = 'model_' + hashlib.sha256(name.encode()).hexdigest()[:16]
                candidates.append((key, 'Claude ' + name + ' 7d', 10080, value))
        for key,label,minutes,value in candidates:
            if not isinstance(value, dict):
                continue
            percent = value.get('utilization')
            percent = float(percent) if type(percent) in {int,float} and math.isfinite(percent) and 0 <= percent <= 100 else None
            reset = value.get('resets_at')
            try:
                reset = int(datetime.fromisoformat(reset.replace('Z','+00:00')).timestamp()) if isinstance(reset,str) else None
            except (ValueError, OverflowError, OSError):
                reset = None
            rows.append({'id':key,'label':label,'status':'observed','usedPercent':percent,
                         'resetsAt':reset,'observedAt':int(time.time()),'windowMinutes':minutes})
        # Authoritative snapshot replacement also clears missing windows.
        with self.store.db:
            self.store.db.execute('DELETE FROM sdk_rate_limits')
            self.store.db.executemany('INSERT INTO sdk_rate_limits VALUES(?,?)', [(r['id'],dump(r)) for r in rows])
        return True

    def response(self):
        rows = [json.loads(r[0]) for r in self.store.db.execute('SELECT snapshot FROM sdk_rate_limits ORDER BY bucket')]
        order = {name:index for index,name in enumerate(LABELS)}
        rows.sort(key=lambda r:order.get(r['id'],99))
        snapshots = {}
        for row in rows:
            recent = time.time() - row['observedAt'] <= 900 and (row['resetsAt'] is None or row['resetsAt'] > time.time())
            key = 'claude-' + row['id']
            snapshots[key] = {'limitId':key, 'limitName':row['label'],
                'primary':{'usedPercent':round(row['usedPercent']), 'windowDurationMins':row['windowMinutes'], 'resetsAt':row['resetsAt']} if row['usedPercent'] is not None and recent else None,
                'secondary':None, 'credits':None, 'planType':None}
        # Codex-shaped summary: the 5-hour window is `primary`, the weekly window
        # `secondary` (clients draw these two); every bucket stays by limit id.
        window = lambda key: (snapshots.get('claude-' + key) or {}).get('primary')
        summary = {'limitId':'claude', 'limitName':'Claude', 'primary':window('five_hour'),
                   'secondary':window('seven_day'), 'credits':None, 'planType':None}
        return {'rateLimits':summary, 'rateLimitsByLimitId':snapshots,
                'ordinaryUsageAllowed':None, 'rateLimitResetCredits':None, 'accountId':None,'rateLimitUpsell':None,
                'providerRateLimits':{'version':1,'rows':rows}}
