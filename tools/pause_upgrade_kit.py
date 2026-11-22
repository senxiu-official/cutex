"""r290 P9: bounded maintenance without lifting the owner's pause.

The predecessor supplies the normal generation-fenced runtime/offline API.
An admitted call on a paused target keeps its original attempt and outcome:
the daemon reconciles its receipt or old generation, never the kit.
"""
import hashlib
import json
from pathlib import Path
import time
import urllib.parse
import uuid

from upgrade_online_kit import load, write, runtime_stop_marker


def already_target(d, *, out=print):
    """Verify the exact installed target before asking about predecessors."""
    host = d.identity()
    build = host['cutexBuild']
    if (build.get('release'), build.get('commit')) != (d.plan['release'], d.plan['commit']):
        return False
    d.integrity()
    # Re-read actual installed bytes; a matching version label alone is not a
    # successful no-op, and no check that copies business data is needed.
    for name, targets in d.plan.get('binaryTargets', {}).items():
        expected = d.plan['files'].get('payload/' + name)
        if expected is None:
            raise RuntimeError('已运行目标版本，但升级包缺少安装文件校验：' + name)
        for target in targets:
            path = Path(target)
            if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
                raise RuntimeError('已运行目标版本，但安装文件与封包不匹配，请运行 status/recover：' + str(path))
    d.service_health()
    out('已经是 ' + d.plan['release'] + '，无需升级', flush=True)
    return True


def pause_identity(session):
    """Durable owner intent, excluding settling phase and per-process tools."""
    pause = session.get('agentPause') or {}
    reasons = session.get('pauseReasons') or []
    return {
        'agentPause': {key: pause[key] for key in
                       ('cutexSessionId', 'requestedAt', 'by', 'byCutexSessionId') if key in pause},
        'pauseReasons': [{key: value for key, value in reason.items()
                          if key not in ('phase', 'pausedAt')} for reason in reasons],
    }


def whole_paused(session):
    # r290 P9: project afterTurn holds admission, not its current turn.
    # Let project-only work finish within the ordinary bounded wait; an
    # individual tool-boundary pause still requires controlled maintenance.
    return bool(session.get('agentPause')) or (not active(session) and any(
        reason.get('kind') == 'project' for reason in session.get('pauseReasons') or [])
    )


def active(session):
    runtime = session.get('runtime') or {}
    status = runtime.get('threadStatus')
    return bool(runtime.get('activeTurnId')) or status == 'active' or (isinstance(status, dict) and status.get('type') == 'active')


def admitted(d):
    # A truncated inventory is not evidence that every in-flight target is held.
    result = d.api('/v2/asks?state=admitted&limit=1000')
    return result.get('asks', []), bool(result.get('hasMore'))


def idle(d, wait=False, *, clock=time.monotonic, sleep=time.sleep, out=print):
    cfg = d.plan.get('pauseUpgrade') or {}
    deadline = clock() + max(0, float(cfg.get('readinessTimeoutSeconds', 600)))
    while True:
        rows = d.sessions()
        paused = {row['cutexSessionId'] for row in rows if whole_paused(row)}
        busy = [{'id': row['cutexSessionId'],
                 'name': row.get('formalAgentName') or row.get('displayName')}
                for row in rows if active(row) and row['cutexSessionId'] not in paused]
        asks, more = admitted(d)
        admissions = [{'id': ask.get('askId', ask.get('id')), 'target': ask.get('target'),
                       'attempt': (ask.get('attempt') or {}).get('state')}
                      for ask in asks]
        unresolved = [ask for ask in admissions if ask['target'] not in paused]
        info = {'waitingFor': busy, 'submitting': bool(unresolved or more),
                'admissions': admissions,
                'pausedForMaintenance': sorted(paused),
                'pausedAdmittedPreserved': [ask for ask in admissions if ask['target'] in paused]}
        if not wait or not (busy or unresolved or more):
            return rows, info
        if clock() >= deadline:
            raise RuntimeError('升级等待已到截止时间；普通活动 turn 或提交仍未收敛。'
                               '尚未下线成员；暂停不会被解除。状态：' + json.dumps(info, ensure_ascii=False))
        out('升级准备：暂停成员将受控下线，之后恢复在线并保留暂停；'
            '正在等待其他 turn / 提交收敛。' + json.dumps(info, ensure_ascii=False), flush=True)
        sleep(min(5, max(0, deadline - clock())))


def offline(d, op, rows, *, clock=time.monotonic, sleep=time.sleep, out=print):
    """Stop exact connected occurrences; no resume, flush or unknown replay."""
    op = Path(op)
    if (op / 'manual-online-original.json').exists():
        raise RuntimeError('本次操作已记录下线请求；请使用 status/recover，不重放停止请求')
    selected = [row for row in rows if (row.get('runtime') or {}).get('appServerConnected') is True]
    inherited = load(op / 'inherited-manual-online.json') if (op / 'inherited-manual-online.json').exists() else []
    named = {row['id']: dict(row) for row in inherited}
    for row in selected:
        sid = row['cutexSessionId']
        old = named.get(sid, {})
        named[sid] = {'id': sid, 'name': row.get('formalAgentName') or row.get('displayName'),
                      'sourceOperations': list(dict.fromkeys(old.get('sourceOperations', []) + [op.name])),
                      'selectedForOfflineByThisOperation': True, 'offlinedByThisOperation': False,
                      'offlineOutcome': 'planned', 'pauseBefore': pause_identity(row),
                      'runtimeGenerationBefore': row['runtime']['runtimeGeneration']}
    write(op / 'manual-online-original.json', list(named.values()))
    write(op / 'manual-online.json', list(named.values()))
    for row in selected:
        sid = row['cutexSessionId']
        fresh = next((current for current in d.sessions() if current['cutexSessionId'] == sid), None)
        if fresh is None:
            raise RuntimeError('下线前成员已不存在；尚未提交该成员的停止请求：' + sid)
        runtime = fresh.get('runtime') or {}
        if runtime.get('runtimeGeneration') != row['runtime']['runtimeGeneration']:
            raise RuntimeError('下线前运行代已改变；请重新检查：' + sid)
        if not runtime.get('appServerConnected'):
            # An owner stop after the snapshot does not authorize a later start.
            named[sid].update(offlineOutcome='owner-already-offline', selectedForOfflineByThisOperation=False)
            write(op / 'manual-online.json', list(named.values()))
            continue
        if active(fresh) and not whole_paused(fresh):
            raise RuntimeError('普通 turn 在下线前开始；请稍后重试，暂停状态保持：' + sid)
        named[sid]['pauseBefore'] = pause_identity(fresh)
        if whole_paused(fresh):
            out('升级维护：' + str(named[sid]['name'] or sid) + ' 将下线；升级后恢复在线并保留暂停。', flush=True)
        request_id = str(uuid.uuid4())
        receipt = op / ('offline-' + hashlib.sha256(sid.encode()).hexdigest() + '.json')
        write(receipt, {'requestId': request_id, 'id': sid,
                        'generation': runtime['runtimeGeneration'], 'state': 'requested'})
        named[sid]['offlineOutcome'] = 'requested'
        write(op / 'manual-online.json', list(named.values()))
        reply = d.api('/v2/sessions/' + urllib.parse.quote(sid, safe='') + '/cutex/requests', 'POST',
                      {'requestId': request_id, 'method': 'cutex/runtime/offline',
                       'params': {'expectedRuntimeGeneration': runtime['runtimeGeneration'],
                                  'force': False, 'reason': 'r290 owner upgrade: preserve pause'}})
        if reply.get('ok') is False or reply.get('error') or reply.get('outcomeUnknown') is True:
            raise RuntimeError('下线被拒绝或结果未知；原请求已保留，禁止重放：' + sid)
    waiting = {sid for sid, row in named.items() if row.get('offlineOutcome') == 'requested'}
    deadline = clock() + max(0, float((d.plan.get('pauseUpgrade') or {}).get('offlineTimeoutSeconds', 180)))
    while waiting:
        current = {row['cutexSessionId']: row for row in d.sessions()}
        for sid in list(waiting):
            runtime = (current.get(sid) or {}).get('runtime') or {}
            if runtime.get('status') == 'offline' and not runtime.get('appServerConnected'):
                after = pause_identity(current[sid])
                named[sid].update(offlineOutcome='confirmed', offlinedByThisOperation=True,
                                  pauseAfterOffline=after, runtimeUpdatedAtAfterOffline=current[sid].get('updatedAt'),
                                  runtimeStopReceiptAfterOffline=runtime_stop_marker(getattr(d, 'home', None), sid, named[sid]['runtimeGenerationBefore']))
                write(op / 'manual-online.json', list(named.values()))
                if after != named[sid]['pauseBefore']:
                    write(op / ('pause-changed-' + hashlib.sha256(sid.encode()).hexdigest() + '.json'),
                          {'before': named[sid]['pauseBefore'], 'after': after})
                    raise RuntimeError('下线期间暂停状态发生变化；已保留前后证据，未尝试恢复或覆盖：' + sid)
                waiting.remove(sid)
        if waiting:
            if clock() >= deadline:
                raise RuntimeError('下线结果未在截止时间内确认；禁止重放原请求：' + ', '.join(sorted(waiting)))
            sleep(min(1, max(0, deadline - clock())))
