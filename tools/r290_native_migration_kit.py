"""Owner-authorized r290 Native r62 -> r63 migration, with recorded-only undo.

All session writes use the daemon's reviewed explicit-launch API. Requests with
unknown outcomes are never replayed. This module never starts or stops agents.
"""
import copy
import hashlib
import json
from pathlib import Path
import urllib.parse

SOURCE = '0.167.0-r62'
TARGET = '0.169.0-r63'
SOURCE_COMMIT = '7bcc96366534b93353de8e8ba16b545f41299ca4'
API = '/v2/agent-management/explicit-launch'
NAME = 'native-r290-migration.json'
INVENTORY = 'native-r290-inventory.json'
DEFAULTS_PROOF = 'native-r290-defaults.json'


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def load(path):
    return json.loads(Path(path).read_text(encoding='utf-8-sig'))


def write(path, value):
    from upgrade_online_kit import write as atomic_write
    atomic_write(path, value)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def bind_operation(op, **fields):
    path = Path(op) / 'operation.json'
    record = load(path)
    record.update(fields)
    write(path, record)


def inventory_view(report):
    return {'policy': report['policy'], 'beforeDefaults': report['beforeDefaults'],
            'agents': [{k: row.get(k) for k in ('id', 'previous', 'originalOnline', 'nativePatchCommit', 'archivedAtCapture')}
                       for row in report['agents']]}


def verify_inventory(op, report):
    record = load(Path(op) / 'operation.json')
    path = Path(op) / INVENTORY
    require(path.is_file() and sha(path) == record.get('nativeMigrationInventorySha256'),
            'Native migration inventory changed; inspect before rollback or retry')
    require(load(path) == inventory_view(report), 'Native migration inventory changed')


def read_records(home):
    # Use the same bounded, read-only SQLite snapshot reader as deploy.py.
    from shared import sqlite_snapshot as snap
    before = snap.read_database(Path(home) / 'cutex-sessions.sqlite3',
                                legacy_paths=(Path(home) / 'cutex-sessions.json',))
    if before[0] is None:
        return list(json.loads(before[2] or b'{"sessions":{}}')['sessions'].values())
    db, _ = snap.connect_snapshot(before[0], before[1])
    try:
        return [json.loads(r[0]) for r in db.execute('SELECT record_json FROM sessions')]
    finally:
        db.close()


def settle_requested(report, records=None, home=None):
    """A receipt plus the committed SQL contract/revision proves a lost reply.

    The daemon writes the receipt BEFORE committing SQL, so the receipt alone
    never authorizes a move, restart or rollback. No request is sent here.
    """
    changed = False
    by_id = None if records is None else {r['cutex_session_id']: r for r in records}
    for row in report['agents']:
        if row['state'] != 'requested':
            continue
        review = row['review']
        path = Path(review['target_contract']['native_home']) / 'runtime-package-upgrades' / (
            hashlib.sha256(row['actionId'].encode()).hexdigest() + '.json')
        try:
            before = path.stat()
            require(before.st_size <= 1024 * 1024, 'Native migration receipt too large')
            raw = path.read_bytes()
            after = path.stat()
            identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns)
            require(identity(before) == identity(after), 'Native migration receipt changing')
            receipt = json.loads(raw)
            require(receipt['action_id'] == row['actionId'] and receipt['review'] == review,
                    'Native migration receipt payload mismatch')
            require(isinstance(receipt.get('committed_at'), str) and receipt['committed_at'],
                    'Native migration receipt lacks commit timestamp')
            if by_id is None:
                by_id = {r['cutex_session_id']: r for r in read_records(home)}
            record = by_id.get(row['id']) or {}
            require(record.get('explicit_launch') == review['target_contract']
                    and record.get('revision') == receipt['activated_revision'],
                    'Native receipt is not proven committed in the session store')
        except (OSError, ValueError, KeyError, RuntimeError):
            continue
        row.update(state='moved', receipt=receipt, settledByReadOnlyReceipt=True,
                   receiptFileSha256=hashlib.sha256(raw).hexdigest(), reason=None)
        changed = True
    return changed


def enabled(plan):
    policy = plan.get('nativeMigration')
    if policy is None:
        return False
    require(plan.get('release') == 'r290' and policy == {
        'from': SOURCE, 'to': TARGET, 'scope': 'all-existing-r62',
        'rollback': 'recorded-moves-only'}, 'unrecognized Native migration policy')
    require(plan['nativeUpdate']['version'] == TARGET, 'migration target version changed')
    return True


def metadata_paths(d):
    return [d.home / 'runtime/light/deployment.json',
            d.home / 'runtime/native-version-registry-v1.json',
            d.home / 'runtime/claude/deployment.json',
            Path(d.plan['installBase']) / 'components/state.json']


def metadata(d):
    return {str(p): load(p) if p.is_file() else None for p in metadata_paths(d)}


def pin_lenses(d):
    d.persisted()
    result = {}
    for r in d.session_records:
        c = r.get('claude_session') or {}
        result[r['cutex_session_id']] = {
            'nativeId': r.get('codex_session_id'), 'backend': r.get('runtime_backend'),
            'explicitLaunch': copy.deepcopy(r.get('explicit_launch')),
            'profileBinding': r.get('profile_binding'),
            'claude': {k: c.get(k) for k in ('deployment', 'thread_id', 'state_dir')},
            'acp': r.get('dsh_session'), 'kimi': r.get('kimi_session')}
    return result


def capture(d, op):
    if not enabled(d.plan):
        return
    path = Path(op) / NAME
    require(not path.exists(), 'Native migration inventory already exists; inspect before retry')
    sessions = {s['cutexSessionId']: s for s in d.sessions()}
    d.persisted()
    rows = []
    for record in d.session_records:
        # The daemon's package eligibility is persistent-only too. Missing
        # registration_class defaults to local_only, never to managed identity.
        if record.get('registration_class') != 'persistent':
            continue
        contract = record.get('explicit_launch')
        # Explicit-launch contracts are the local Native package authority.
        if not contract:
            continue
        sid = record['cutex_session_id']
        resource = sessions.get(sid, {})
        row = {'id': sid, 'previous': contract,
               'originalOnline': bool((resource.get('runtime') or {}).get('appServerConnected')
                                      or (resource.get('runtime') or {}).get('status') == 'online'),
               'archivedAtCapture': bool(record.get('retired_at') or record.get('archive_state', record.get('lifecycle')) == 'retired'
                                         or resource.get('lifecycle') == 'retired' or resource.get('retiredAt')),
               'state': 'skipped', 'reason': 'older-or-unverified-native-pin'}
        manifest = Path(contract['bundle_manifest'])
        if manifest.is_file() and sha(manifest) == contract['bundle_sha256']:
            bundle = load(manifest)
            row['nativePatchCommit'] = bundle.get('native_patch_commit')
            if bundle.get('native_patch_commit') == SOURCE_COMMIT:
                row.update(state='skipped' if row['archivedAtCapture'] else 'pending',
                           reason='archived' if row['archivedAtCapture'] else None)
        rows.append(row)
    report = {'schema': 'cutex/r290-native-migration/v1', 'policy': d.plan['nativeMigration'],
              'beforeDefaults': metadata(d), 'agents': rows}
    write(path, report)
    write(Path(op) / INVENTORY, inventory_view(report))
    d.record(op, load(Path(op) / 'operation.json')['phase'],
             nativeMigrationInventorySha256=sha(Path(op) / INVENTORY))


def counts(report):
    return {'migratedOnline': sum(r['state'] == 'moved' and r['originalOnline'] for r in report['agents']),
            'migratedOffline': sum(r['state'] == 'moved' and not r['originalOnline'] for r in report['agents']),
            'skipped': sum(r['state'] == 'skipped' for r in report['agents']),
            'skippedArchived': sum(r['state'] == 'skipped' and r.get('reason') == 'archived' for r in report['agents']),
            'skippedOther': sum(r['state'] == 'skipped' and r.get('reason') != 'archived' for r in report['agents']),
            'unknown': sum(r['state'] == 'requested' for r in report['agents']),
            'failed': sum(r['state'] == 'failed' for r in report['agents'])}


def same_identity(old, new):
    return all(old.get(k) == new.get(k) for k in ('native_home', 'native_id', 'version', 'migration_action_id'))


def forward(phase):
    """Only a durable matching receipt may advance the conditional-online fence."""
    from upgrade_online_kit import runtime_stop_marker, scrub_text
    path = phase.op / NAME
    report = load(path)
    verify_inventory(phase.op, report)
    capture_defaults(phase, report)
    verify_defaults(phase.op, report)
    if settle_requested(report, home=phase.home):
        write(path, report)
    if any(row['state'] == 'requested' for row in report['agents']):
        phase.blocked.update(row['id'] for row in report['agents'] if row['state'] in ('pending', 'failed', 'requested'))
        report['preflight'] = {'state': 'blocked-unknown', 'reviewedAgents': []}
        return save_forward(phase, report)
    reviews = {}
    refused = []
    for row in report['agents']:
        if row['state'] == 'failed':
            row.update(state='pending', reason=None)
        if row['state'] != 'pending':
            continue
        try:
            before, review = review_candidate(phase, row)
            if review is None:
                row.update(state='skipped', reason='archived')
            else:
                reviews[row['id']] = (before, review)
        except Exception as error:
            row.update(state='failed', reason=scrub_text(repr(error), phase.token))
            refused.append(row['id'])
    report['preflight'] = {'state': 'refused' if refused else 'passed',
                           'reviewedAgents': list(reviews), 'refusedAgents': refused}
    write(path, report)
    if refused:
        # No apply (nor its durable requested marker) precedes a successful
        # review of EVERY candidate. An invalid later agent means zero moves.
        phase.blocked.update(row['id'] for row in report['agents'] if row['state'] in ('pending', 'failed'))
        return save_forward(phase, report)
    for row in report['agents']:
        sid = row['id']
        phase.blocked.discard(sid)
        candidate = next((a for a in phase.result['agents'] if a['id'] == sid), None)
        if row['state'] == 'requested':
            phase.blocked.add(sid)
            continue
        if row['state'] == 'failed':
            # The failure was before the durable requested state; no apply was
            # sent, so a fresh review on a later explicit retry is safe.
            row.update(state='pending', reason=None)
            write(path, report)
        if row['state'] == 'pending':
            try:
                before, review = reviews[sid]
                marker = candidate.get('runtimeStopReceiptAfterOffline') if candidate else None
                eligible = bool(candidate and candidate.get('runtimeUpdatedAtAfterOffline') == before.get('updatedAt')
                                and marker and marker.get('available') and runtime_stop_marker(phase.home, sid, marker['generation']) == marker)
                action = 'kit-r290-native-' + hashlib.sha256((str(phase.op.resolve()) + '\0' + sid).encode()).hexdigest()
                row.update(state='requested', actionId=action, review=review, onlineFenceEligible=eligible)
                write(path, report)  # Before POST; an uncertain call can never be replayed.
                receipt = phase.http(API, 'POST', {'operation': 'apply_runtime_package_upgrade', 'action_id': action, 'review': review})
                require(receipt['action_id'] == action and receipt['review'] == review, 'migration receipt mismatch')
                row.update(state='moved', receipt=receipt)
                write(path, report)
            except Exception as error:
                if row['state'] == 'pending':
                    row['state'] = 'failed'
                row['reason'] = scrub_text(repr(error), phase.token)
                phase.blocked.add(sid)
                write(path, report)
                continue
        if row['state'] == 'moved' and candidate:
            receipt = row['receipt']
            current = phase.session(sid)
            marker = candidate.get('runtimeStopReceiptAfterOffline')
            if (row.get('onlineFenceEligible') and current and current.get('updatedAt') == receipt['committed_at']
                    and marker and runtime_stop_marker(phase.home, sid, marker['generation']) == marker
                    and (current.get('runtime') or {}).get('runtimeGeneration') == row['review']['subject']['runtime_generation']):
                phase.set(sid, runtimeUpdatedAtAfterOffline=receipt['committed_at'],
                          runtime={'state': 'moved', 'from': SOURCE, 'to': TARGET, 'actionId': row['actionId']})
            elif not candidate.get('earlierRun'):
                phase.blocked.add(sid)
    return save_forward(phase, report)


def review_candidate(phase, row):
    """Authority stays in the daemon; no model/effort logic is copied here."""
    sid = row['id']
    before = phase.session(sid)
    require(before is not None and not (before.get('runtime') or {}).get('appServerConnected'),
            'agent is online or missing; leave its pin unchanged')
    if before.get('lifecycle') == 'retired' or before.get('retiredAt'):
        return before, None
    review = phase.http(API, 'POST', {'operation': 'review_runtime_package_upgrade', 'cutex_session_id': sid})
    require(review['subject']['runtime_generation'] == (before.get('runtime') or {}).get('runtimeGeneration'),
            'Native runtime generation changed between GET and review')
    require(review['current_contract'] == row['previous'], 'Native pin changed since inventory')
    require(review['current_runtime_version'] == SOURCE and review['target_runtime_version'] == TARGET,
            'review is not exactly Native r62 -> r63')
    require(not review.get('repair') and same_identity(row['previous'], review['target_contract']),
            'migration would change Native identity/home or repair files')
    selected = Path(review['selected_manifest'])
    require(sha(selected) == review['selected_manifest_sha256'], 'selected Native manifest changed')
    bundle = load(selected)
    require(bundle.get('native_patch_commit') == phase.plan['nativeUpdate']['commit'], 'wrong selected Native commit')
    for role, artifact in phase.plan['nativeUpdate']['artifacts'].items():
        require(bundle[role]['sha256'] == artifact['sha256'], 'wrong selected Native artifact: ' + role)
    return before, review


def save_forward(phase, report):
    path = phase.op / NAME
    report['counts'] = counts(report)
    write(path, report)
    phase.result['runtimePackages'] = {'policy': 'r290-owner-authorized-native-migration', **report['counts']}
    phase.save()
    phase.out('NATIVE MIGRATION ' + json.dumps(report['counts'], sort_keys=True))


def metadata_paths_for_phase(phase):
    class Context:
        home = phase.home
        plan = phase.plan
    return metadata_paths(Context())


def capture_defaults(phase, report):
    """Finish local checkpoints without silently taking a newer snapshot.

    New order saves the intent in the migration report before the proof/hash.
    Also recover the old proof-before-report order. A missing checkpoint may
    only be repaired while the observed defaults still equal the saved intent.
    """
    defaults = phase.op / DEFAULTS_PROOF
    record = load(phase.op / 'operation.json')
    bound = record.get('nativeMigrationDefaultsSha256')
    proof = load(defaults) if defaults.is_file() else None
    if bound:
        require(proof is not None and sha(defaults) == bound, 'Native defaults capture changed')
    if report.get('afterDefaults') is not None and bound:
        return  # Normal later runs do not recapture owner-changed defaults.
    live = {str(p): load(p) if p.is_file() else None for p in metadata_paths_for_phase(phase)}
    after = report.get('afterDefaults')
    if proof is not None:
        require(proof.get('before') == report['beforeDefaults'] and isinstance(proof.get('after'), dict),
                'Native defaults capture changed')
        require(after is None or after == proof['after'], 'Native defaults capture intent mismatch')
        after = proof['after']
    if after is None:
        after = live
    require(live == after, 'Native defaults changed during interrupted capture; inspect before retry')
    if report.get('afterDefaults') is None:
        report['afterDefaults'] = after
        write(phase.op / NAME, report)
    expected = {'before': report['beforeDefaults'], 'after': after}
    if proof is None:
        write(defaults, expected)
    else:
        require(proof == expected, 'Native defaults capture changed')
    if not bound:
        bind_operation(phase.op, nativeMigrationDefaultsSha256=sha(defaults))


def verify_defaults(act, report):
    path = Path(act) / DEFAULTS_PROOF
    record = load(Path(act) / 'operation.json')
    require(report.get('afterDefaults') is not None, 'missing post-upgrade Native default proof')
    require(path.is_file() and sha(path) == record.get('nativeMigrationDefaultsSha256'),
            'Native defaults capture changed or is incomplete')
    require(load(path) == {'before': report['beforeDefaults'], 'after': report['afterDefaults']},
            'Native defaults capture changed')


def rollback_preflight(d, act, recovering=None):
    """Known input refusals happen before controlled offline or service stop."""
    if not enabled(d.plan):
        return
    # A new rollback operation cannot become an implicit retry of an earlier
    # non-idempotent revert whose outcome was not settled.
    for path in (d.root / 'operations').glob('*/operation.json'):
        previous = load(path)
        if (previous.get('kind') == 'rollback' and previous.get('activation') == Path(act).name
                and previous.get('phase') != 'rolled-back'
                and (path.parent / 'native-r290-revert.json').is_file()
                and path.parent != recovering):
            raise RuntimeError('A previous Native rollback has a revert receipt; run recover for that operation, or inspect its unknown outcome; do not retry rollback')
    report = load(Path(act) / NAME)
    verify_inventory(act, report)
    verify_defaults(act, report)
    d.persisted()
    if settle_requested(report, records=d.session_records):
        report['counts'] = counts(report)
        write(Path(act) / NAME, report)
    require(not any(r['state'] == 'requested' for r in report['agents']),
            'Native migration outcome unknown; inspect exact request before rollback')
    paths = metadata_paths(d)
    for values in (report['beforeDefaults'], report['afterDefaults'], metadata(d)):
        require(all(isinstance(values.get(str(p)), dict) for p in paths),
                'missing Native/Claude selector metadata; inspect before rollback')
        require(all(field in values[str(paths[1])] for field in ('latest_cli', 'latest_runtime')),
                'missing Native version selectors; inspect before rollback')
    for row in report['agents']:
        require(row['state'] in ('pending', 'failed', 'skipped', 'moved'), 'invalid Native migration state')
        if row['state'] != 'moved':
            continue
        receipt = row['receipt']
        require(receipt['action_id'] == row['actionId'] and receipt['review'] == row['review']
                and row['review']['current_contract'] == row['previous'], 'Native move receipt changed')
        for contract in (row['previous'], row['review']['target_contract']):
            require(sha(contract['bundle_manifest']) == contract['bundle_sha256'], 'recorded Native contract changed')


def recover_preflight(d, op, record):
    """Never let generic rollback recovery bypass Native refusal/partial undo."""
    if record.get('kind') != 'rollback' or not enabled(d.plan):
        return
    act = d.root / 'operations' / record['activation']
    rollback_preflight(d, act, recovering=Path(op))
    path = Path(op) / 'native-r290-revert.json'
    require(path.is_file(), 'Native revert has no receipt; recover will not switch backend. Run rollback again.')
    require(not record.get('nativeRevertReceiptSha256') or sha(path) == record['nativeRevertReceiptSha256'],
            'Native revert receipt changed')
    undo = load(path)
    if not undo.get('sessionPinsValidated'):
        settle_revert(d, act, op, undo)
        undo = load(path)
        # Only never-sent rows may remain. recover_rollback will send those
        # after this read-only preflight; an unknown request is never replayed.
        if undo.get('beforePins') is not None and not any(r['state'] == 'requested' for r in undo['agents']):
            require(undo['activationMigrationSha256'] == sha(act / NAME), 'activation Native migration changed')
            return
    require(undo.get('sessionPinsValidated') is True,
            'Native revert is incomplete or outcome unknown; inspect exact receipts before recovery; do not replay')
    require(undo['activationMigrationSha256'] == sha(act / NAME), 'activation Native migration changed')
    require(not record.get('nativeRevertReceiptSha256') or sha(path) == record['nativeRevertReceiptSha256'],
            'Native revert receipt changed')
    expected = undo['pins']
    current = d.persisted()
    require((current['sessions'], current['count']) == (expected['sessions'], expected['count']),
            'session pins changed after validated Native revert; inspect before recovery')
    # Close the local checkpoint gap if the receipt write completed but the
    # operation record write failed. There is no daemon request to replay.
    if not record.get('nativeRevertReceiptSha256'):
        bind_operation(op, pins=expected, nativeRevertReceiptSha256=sha(path))


def recover_rollback(d, op, record):
    """Resume a validated undo, with selectors restored behind a zero-writer fence."""
    recover_preflight(d, op, record)
    record = load(Path(op) / 'operation.json')
    act = d.root / 'operations' / record['activation']
    undo = load(Path(op) / 'native-r290-revert.json')
    if not undo.get('sessionPinsValidated'):
        continue_revert(d, act, op, undo)
        record = load(Path(op) / 'operation.json')
    pins = record['pins']
    if record['phase'] == 'rolled-back':
        d.daemon_guard().verify(d, op)
        d.j.run('recover.services.start', lambda: d.service('start'))
        d.j.run('recover.previous.health', lambda: d.previous_health(pins, d.plan['previousCommit']))
        return
    d.j.run('recover.rollback.stop', lambda: d.service('stop'))
    d.j.run('recover.rollback.fence', d.fence)
    if record.get('nativeSelectorRestoreSha256'):
        proof = Path(op) / 'native-r290-selector-restore.json'
        require(proof.is_file() and sha(proof) == record['nativeSelectorRestoreSha256'], 'selector restore proof changed')
        require(d.persisted() == pins, 'pins/defaults changed after validated selector restore')
        d.daemon_guard().verify(d, op)
    else:
        pins = restore_defaults(d, act, op, pins)
    d.rollback_steps(op, act, pins)


def revert(d, act, op, pins):
    """Called after controlled offline, before stopping the current daemon."""
    if not enabled(d.plan):
        return pins
    source = Path(act) / NAME
    report = load(source)
    require(not any(r['state'] == 'requested' for r in report['agents']),
            'Native migration outcome unknown; inspect exact request before rollback')
    path = Path(op) / 'native-r290-revert.json'
    require(not path.exists(), 'Native revert already attempted; inspect exact receipt, never replay')
    require(d.persisted() == pins, 'pins changed after controlled rollback stop')
    before = pin_lenses(d)
    records = {r['cutex_session_id']: copy.deepcopy(r) for r in d.session_records}
    undo = {'activationMigrationSha256': sha(source), 'agents': [], 'beforePins': before}
    for row in report['agents']:
        if row['state'] != 'moved':
            continue
        sid = row['id']
        target = row['review']['target_contract']
        if sid not in before or before[sid]['explicitLaunch'] != target:
            undo['agents'].append({'id': sid, 'state': 'skipped', 'reason': 'owner-changed-or-removed-pin'})
            continue
        require(sid in records, 'Native revert needs the exact pre-request session record')
        item = {'id': sid, 'state': 'pending', 'expected': target, 'restore': row['previous'],
                'beforeRecord': records[sid]}
        undo['agents'].append(item)
    write(path, undo)
    return continue_revert(d, act, op, undo)


def settle_revert(d, act, op, undo):
    """Read-only proof of an uncertain contract swap; never POST a retry.

    Revert has no durable backend action receipt. Require its exact SQL write
    shape: old record + one revision, only explicit_launch/updated_at changed.
    Preserve every other field and every unrelated session's pin lens. Old
    journals without this pre-request proof remain conservatively blocked.
    """
    if undo.get('beforePins') is None:
        return
    require(undo['activationMigrationSha256'] == sha(Path(act) / NAME), 'activation Native migration changed')
    d.persisted()
    records = {r['cutex_session_id']: r for r in d.session_records}
    expected = copy.deepcopy(undo['beforePins'])
    changed = False
    for item in undo['agents']:
        sid = item['id']
        if item['state'] == 'skipped':
            continue
        before = item['beforeRecord']
        current = records.get(sid)
        if item['state'] == 'requested':
            require(current is not None, 'Native revert session disappeared; inspect before recovery')
            if current != before:
                proven = copy.deepcopy(before)
                # Match CutexSessionRecord::durable_revision: zero/missing
                # legacy revision is normalized to one before the increment.
                proven.update(explicit_launch=item['restore'], revision=max(before.get('revision', 0), 1) + 1,
                              updated_at=current.get('updated_at'))
                require(current == proven and isinstance(current.get('updated_at'), str)
                        and current['updated_at'] and current['updated_at'] != before.get('updated_at'),
                        'unknown Native revert has conflicting session changes; inspect before recovery')
                item.update(state='reverted', settledByReadOnlyStore=True,
                            receipt={'cutex_session_id': sid, 'replaced_contract': item['expected'],
                                     'restored_contract': item['restore'], 'activated_revision': current['revision'],
                                     'committed_at': current['updated_at']})
                changed = True
        if item['state'] == 'reverted':
            expected[sid]['explicitLaunch'] = item['restore']
        elif item['state'] == 'pending':
            require(current == before, 'never-sent Native revert session changed; inspect before recovery')
    require(pin_lenses(d) == expected, 'unexpected session pin change during Native revert recovery')
    if changed:
        write(Path(op) / 'native-r290-revert.json', undo)
    if not any(item['state'] in ('requested', 'pending') for item in undo['agents']):
        finish_revert(d, op, undo, expected)


def continue_revert(d, act, op, undo):
    settle_revert(d, act, op, undo)
    require(not any(item['state'] == 'requested' for item in undo['agents']),
            'Native revert outcome unknown; inspect exact receipts before recovery; do not replay')
    if undo.get('sessionPinsValidated'):
        return undo['pins']
    path = Path(op) / 'native-r290-revert.json'
    expected = copy.deepcopy(undo['beforePins'])
    for item in undo['agents']:
        if item['state'] == 'reverted':
            expected[item['id']]['explicitLaunch'] = item['restore']
    for item in undo['agents']:
        if item['state'] != 'pending':
            continue
        sid = item['id']
        require(pin_lenses(d) == expected, 'pins changed before never-sent Native revert')
        current = next((r for r in d.session_records if r['cutex_session_id'] == sid), None)
        require(current == item['beforeRecord'], 'session changed before never-sent Native revert')
        item['state'] = 'requested'
        write(path, undo)  # Before POST. A lost reply is settled only from SQL.
        receipt = d.api(API, 'POST', {'operation': 'revert_runtime_package_upgrade', 'cutex_session_id': sid,
                                     'expected_contract': item['expected'], 'restore_contract': item['restore']})
        require(receipt['cutex_session_id'] == sid and receipt['replaced_contract'] == item['expected']
                and receipt['restored_contract'] == item['restore'], 'Native revert receipt mismatch')
        item.update(state='reverted', receipt=receipt)
        expected[sid]['explicitLaunch'] = item['restore']
        write(path, undo)
    return finish_revert(d, op, undo, expected)


def finish_revert(d, op, undo, expected):
    require(pin_lenses(d) == expected, 'unexpected session pin change during Native revert')
    undo['sessionPinsValidated'] = True
    undo['pins'] = d.persisted()
    write(Path(op) / 'native-r290-revert.json', undo)
    d.record(op, load(Path(op) / 'operation.json')['phase'],
             pins=undo['pins'], nativeRevertReceiptSha256=sha(Path(op) / 'native-r290-revert.json'))
    return undo['pins']


def restore_defaults(d, act, op, pins):
    """Stopped writers: restore selectors, retaining both installed packages."""
    if not enabled(d.plan):
        return pins
    report = load(Path(act) / NAME)
    verify_inventory(act, report)
    verify_defaults(act, report)
    record = load(Path(op) / 'operation.json')
    undo_path = Path(op) / 'native-r290-revert.json'
    require(sha(undo_path) == record.get('nativeRevertReceiptSha256')
            and load(undo_path).get('sessionPinsValidated') is True,
            'selector restore requires the validated Native revert receipt')
    intent_path = Path(op) / 'native-r290-selector-intent.json'
    original_path = Path(op) / 'runtime-selections-original.json'
    if intent_path.is_file():
        require(sha(intent_path) == record.get('nativeSelectorIntentSha256'), 'selector restore intent changed')
        intent = load(intent_path)
        require(sha(original_path) == intent['originalSnapshotSha256'], 'original preservation snapshot changed')
        before_guard = load(original_path)
    else:
        before_guard = load(Path(op) / 'runtime-selections-before.json')
        require(sha(Path(op) / 'runtime-selections-before.json') == record['runtimeSelectionsBeforeSha256'],
                'rollback preservation snapshot changed')
    before = report['beforeDefaults']
    after = report.get('afterDefaults')
    require(after is not None, 'missing post-upgrade Native default proof')
    current = metadata(d)
    desired = copy.deepcopy(current)
    paths = metadata_paths(d)
    skipped = []
    for p in (paths[0], paths[2]):
        key = str(p)
        if current[key] == after[key]:
            desired[key] = before[key]
        else:
            skipped.append(key)
    key = str(paths[1])
    # Retain r63 artifacts/path registrations for sessions born after upgrade.
    for field in ('latest_cli', 'latest_runtime'):
        if current[str(paths[0])] == after[str(paths[0])] and current[key].get(field) == after[key].get(field):
            desired[key][field] = before[key][field]
        else:
            skipped.append(key + ':' + field)
    key = str(paths[3])
    if (current[key] or {}).get('claude-runtime') == (after[key] or {}).get('claude-runtime'):
        if 'claude-runtime' in (before[key] or {}):
            desired[key]['claude-runtime'] = before[key]['claude-runtime']
        else:
            desired[key].pop('claude-runtime', None)
    else:
        skipped.append(key + ':claude-runtime')
    if intent_path.is_file():
        require(intent['activationMigrationSha256'] == sha(Path(act) / NAME), 'activation migration changed')
        require(all(current[key] in (intent['before'][key], intent['after'][key]) for key in current),
                'selector changed outside the recorded partial restore; inspect before recovery')
        desired = intent['after']
        skipped = intent['ownerChangedSelectorsKept']
        require((d.persisted()['sessions'], d.persisted()['count']) == (pins['sessions'], pins['count']),
                'session pins changed during partial selector restore')
    else:
        require(d.persisted() == pins, 'pins changed before default restore')
        if original_path.is_file():
            require(load(original_path) == before_guard, 'original preservation snapshot changed')
        else:
            write(original_path, before_guard)
        intent = {'before': current, 'after': desired, 'ownerChangedSelectorsKept': skipped,
                  'activationMigrationSha256': sha(Path(act) / NAME),
                  'originalSnapshotSha256': sha(original_path)}
        write(intent_path, intent)
        bind_operation(op, nativeSelectorIntentSha256=sha(intent_path))
    for p in paths:
        value = desired[str(p)]
        require(value is not None, 'cannot remove selector metadata during rollback')
        if value != current[str(p)]:
            write(p, value)
    d.persisted()
    after_guard = d.daemon_guard().snapshot(d)
    # Every old immutable file is checked, including no-longer-selected manifests.
    allowed = {str(p) for p in paths}
    for name, digest in before_guard['files'].items():
        if name in allowed:
            continue
        if digest is None:
            require(not Path(name).is_file(), 'unexpected protected file appeared')
        elif isinstance(digest, str):
            require(Path(name).is_file() and sha(name) == digest, 'protected runtime file changed: ' + name)
        else:
            require(after_guard['files'].get(name) == digest, 'protected install metadata changed')
    # Changes in the reference inventory must come only from recorded contracts
    # or the captured default. All their artifact hashes are validated too.
    authorized = set()
    contracts = [r[k] for r in report['agents'] if r['state'] == 'moved' for k in ('previous',)]
    contracts += [r['review']['target_contract'] for r in report['agents'] if r['state'] == 'moved']
    manifests = {c['bundle_manifest'] for c in contracts}
    manifests.update(value['bundle_manifest'] for value in (before[str(paths[0])], after[str(paths[0])]))
    for name in manifests:
        authorized.add(name)
        bundle = load(name)
        for role in ('executable', 'cli', 'code_mode_host', 'schema', 'facade'):
            ref = bundle.get(role) or {}
            if ref.get('path'):
                authorized.add(ref['path'])
                require(sha(ref['path']) == ref['sha256'], 'protected package artifact changed')
    for contract in contracts:
        require(sha(contract['bundle_manifest']) == contract['bundle_sha256'], 'recorded Native contract changed')
    # Claude default paths change, while every per-session Claude pin stays exact.
    for value in (before[str(paths[2])], after[str(paths[2])]):
        authorized.update(value[k] for k in ('python', 'adapter_source', 'claude_cli', 'tui_binary') if value.get(k))
    require(set(before_guard['protectedPaths']) ^ set(after_guard['protectedPaths']) <= authorized,
            'unexpected protected reference change')
    require(set(before_guard['files']) ^ set(after_guard['files']) <= authorized,
            'unexpected manifest reference change')
    proof = {'originalSnapshotSha256': sha(Path(op) / 'runtime-selections-original.json'),
             'activationMigrationSha256': sha(Path(act) / NAME), 'allowedSelectorFiles': sorted(allowed),
             'before': intent['before'], 'after': desired, 'ownerChangedSelectorsKept': skipped,
             'sessionRevertReceiptSha256': sha(Path(op) / 'native-r290-revert.json'),
             'immutableFilesValidated': True, 'installedPackagesRetained': True}
    write(Path(op) / 'native-r290-selector-restore.json', proof)
    write(Path(op) / 'runtime-selections-before.json', after_guard)
    new_pins = d.persisted()
    require((new_pins['sessions'], new_pins['count']) == (pins['sessions'], pins['count']),
            'session pins changed while restoring default selectors')
    d.record(op, load(Path(op) / 'operation.json')['phase'], pins=new_pins,
             runtimeSelectionsBeforeSha256=sha(Path(op) / 'runtime-selections-before.json'),
             nativeSelectorRestoreSha256=sha(Path(op) / 'native-r290-selector-restore.json'))
    return new_pins
