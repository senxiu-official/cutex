import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, rename, appendFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { TimelineStore } from '../src/store.mjs';
import { loadNativeCatalog, reconcileNativeLog, locateNativeLog } from '../src/reconcile.mjs';
import { revertPrepare, forkCopy } from '../src/session-ops.mjs';

const catalog = await loadNativeCatalog();
const installRoot = fileURLToPath(new URL('../../../.dsh-install', import.meta.url));
const sessionId = 'ops-session';
const event = (seq, type, data) => ({ seq, time: seq + 100, type, data });
const text = value => ({ type: 'text', text: value });
// Shaped like a real DSH 0.2.1-rc.3 log: session settings rows, then per turn the
// prompt spliced into the inbox before turn/start.
const settings = [event(0, 'permission/preset', { preset: 'workspace-write' }), event(1, 'sandbox/mode', { mode: 'workspace-write' }), event(2, 'approval/policy', { policy: 'ask' })];
const turn = (start, n, { end = true } = {}) => [
  event(start, 'agent/inbox/spliced', { target: 'next-turn', start: 0, inserted: [] }),
  event(start + 1, 'turn/start', { turn: n }),
  event(start + 2, 'user/message', { id: `user-${n}`, role: 'user', source: { kind: 'user' }, content: [text(`question ${n}`)] }),
  event(start + 3, 'assistant/message', { turn: n, step: 1, message: { id: `assistant-${n}`, role: 'assistant', content: [text(`answer ${n}`)] } }),
  ...(end ? [event(start + 4, 'turn/end', { turn: n, reason: { kind: 'completed' } })] : []),
];
const row = item => JSON.stringify(catalog.encodeCurrentEvent(item)) + '\n';
const headerRow = id => JSON.stringify(catalog.encodeCurrentHeader({ version: 4, id, createdAt: 1, cwd: '/work', isSeeded: false, delegationDepth: 0 }, 0)) + '\n';
const turnId = n => `${sessionId}:turn:${n}`;
const selection = { model: 'deepseek-v4-flash', reasoningEffort: 'high', modelValue: 'deepseek-v4-flash', effortValue: 'high' };

async function agent(t, events) {
  const dir = await mkdtemp(join(tmpdir(), 'dsh-session-ops-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const cwd = join(dir, 'work');
  await mkdir(cwd);
  const sessionsRoot = join(dir, 'home', 'sessions');
  const directory = join(sessionsRoot, '--work--', sessionId);
  await mkdir(directory, { recursive: true });
  const logPath = join(directory, 'session.v4.jsonl');
  await writeFile(logPath, headerRow(sessionId) + events.map(row).join(''));
  const database = join(dir, 'timeline.sqlite');
  const store = new TimelineStore(database);
  try {
    store.bind(sessionId, cwd);
    store.append({ method: 'dsh/launchSelection', params: selection });
    let result;
    do { result = await reconcileNativeLog({ logPath, sessionId, catalog, store }); } while (result.more);
    store.append({ method: 'dsh/contextUsage/updated', params: { used: 1, size: 2 } });
  } finally { store.close(); }
  return { dir, cwd, sessionsRoot, logPath, database, installRoot };
}

const turnsOf = store => store.turnIds();

test('revertPrepare stages the own log cut after the last kept turn and a fresh bound timeline', async t => {
  const events = [...settings, ...turn(3, 1), ...turn(8, 2), ...turn(13, 3)];
  const a = await agent(t, events);
  await assert.rejects(revertPrepare({ ...a, sessionId, beforeTurnId: 'nope', stagingDirectory: join(a.dir, 'stage-x') }), error => error.opsCode === 'turn_not_found');
  const staging = join(a.dir, 'stage');
  const result = await revertPrepare({ ...a, sessionId, beforeTurnId: turnId(2), stagingDirectory: staging });
  assert.deepEqual(result.removedTurnIds, [turnId(3), turnId(2)]);
  assert.deepEqual(result.keptTurnIds, [turnId(1)]);
  assert.equal(result.throughSeq, 7);
  assert.equal(result.filesRestored, false);
  const original = (await readFile(a.logPath, 'utf8')).split('\n');
  assert.equal(await readFile(result.stagedLog, 'utf8'), original.slice(0, 1 + 8).join('\n') + '\n', 'header and kept rows byte for byte');
  // Swap in as cutex does, then the ordinary reader re-projects the kept turn under its old ID.
  await rename(a.database, join(a.dir, 'old.sqlite'));
  await rename(result.stagedDatabase, a.database);
  await rename(result.stagedLog, a.logPath);
  const store = new TimelineStore(a.database);
  try {
    assert.deepEqual({ ...store.binding() }, { sessionId, cwd: a.cwd });
    assert.deepEqual(store.latest('dsh/launchSelection').params, selection);
    assert.equal(store.latest('dsh/contextUsage/updated'), undefined, 'stale occupancy is not carried over');
    assert.deepEqual(turnsOf(store), [turnId(1)], 'offline history is complete before the runtime starts');
    let status;
    do { status = await reconcileNativeLog({ logPath: a.logPath, sessionId, catalog, store }); } while (status.more);
    assert.equal(status.status.state, 'ok');
    assert.equal(status.committed, 0, 'the renamed log keeps its projected cursor');
    assert.deepEqual(turnsOf(store), [turnId(1)]);
    assert.equal(store.nextNativeTurn(sessionId), 2, 'the next prompt continues as turn 2');
    // DSH appends after the cut; the reader follows.
    await appendFile(a.logPath, turn(8, 2).map(row).join(''));
    do { status = await reconcileNativeLog({ logPath: a.logPath, sessionId, catalog, store }); } while (status.more);
    assert.deepEqual(turnsOf(store), [turnId(1), turnId(2)]);
  } finally { store.close(); }
});

test('revertPrepare before the first turn keeps only the session settings rows', async t => {
  const a = await agent(t, [...settings, ...turn(3, 1), ...turn(8, 2)]);
  const result = await revertPrepare({ ...a, sessionId, beforeTurnId: turnId(1), stagingDirectory: join(a.dir, 'stage') });
  assert.deepEqual(result.removedTurnIds, [turnId(2), turnId(1)]);
  assert.equal(result.throughSeq, 2);
  assert.equal((await readFile(result.stagedLog, 'utf8')).trim().split('\n').length, 4);
});

test('revertPrepare refuses an unfinished kept turn, a foreign binding and compressed logs', async t => {
  const a = await agent(t, [...settings, ...turn(3, 1, { end: false })]);
  await assert.rejects(revertPrepare({ ...a, sessionId: 'other', beforeTurnId: turnId(1), stagingDirectory: join(a.dir, 's1') }), error => error.opsCode === 'history_binding_mismatch');
  const b = await agent(t, [...settings, ...turn(3, 1, { end: false }), ...turn(7, 2)]);
  await assert.rejects(revertPrepare({ ...b, sessionId, beforeTurnId: turnId(2), stagingDirectory: join(b.dir, 's2') }), error => error.opsCode === 'turn_in_progress');
  const c = await agent(t, [...settings, ...turn(3, 1)]);
  await rename(c.logPath, c.logPath + '.zstd');
  await assert.rejects(revertPrepare({ ...c, sessionId, beforeTurnId: turnId(1), stagingDirectory: join(c.dir, 's3') }), error => error.opsCode === 'unsupported_history');
});

test('forkCopy copies the newest completed turn (or the chosen one) into the child root under a new ID', async t => {
  const a = await agent(t, [...settings, ...turn(3, 1), ...turn(8, 2), ...turn(13, 3, { end: false })]);
  const child = join(a.dir, 'child');
  const config = { ...a, sessionId, newSessionId: 'child-native', targetSessionsRoot: join(child, 'home', 'sessions'), targetDatabase: join(child, 'timeline.sqlite') };
  await assert.rejects(forkCopy({ ...config, afterTurnId: turnId(3) }), error => error.opsCode === 'turn_in_progress');
  await assert.rejects(forkCopy({ ...config, afterTurnId: 'nope' }), error => error.opsCode === 'turn_not_found');
  const result = await forkCopy(config);
  assert.equal(result.forkedThroughTurnId, turnId(2));
  assert.equal(result.throughSeq, 12);
  assert.equal(result.nativeSessionId, 'child-native');
  const logPath = await locateNativeLog(config.targetSessionsRoot, 'child-native');
  assert.equal(dirname(dirname(logPath)), join(config.targetSessionsRoot, '--work--'));
  const store = new TimelineStore(config.targetDatabase);
  try {
    assert.deepEqual({ ...store.binding() }, { sessionId: 'child-native', cwd: a.cwd });
    assert.deepEqual(store.latest('dsh/launchSelection').params, selection);
    assert.deepEqual(store.turnIds(), ['child-native:turn:1', 'child-native:turn:2'], 'projected before the runtime starts');
    let status;
    do { status = await reconcileNativeLog({ logPath, sessionId: 'child-native', catalog, store }); } while (status.more);
    assert.equal(status.committed, 0, 'the runtime finds the projection caught up');
    assert.deepEqual(store.turnIds(), ['child-native:turn:1', 'child-native:turn:2']);
  } finally { store.close(); }
  const second = await forkCopy({ ...config, afterTurnId: turnId(1), newSessionId: 'child-2', targetDatabase: join(child, 'timeline-2.sqlite') });
  assert.equal(second.throughSeq, 7);
});

test('forkCopy of an agent without a completed turn copies nothing', async t => {
  const a = await agent(t, [...settings, ...turn(3, 1, { end: false })]);
  const child = join(a.dir, 'child');
  const result = await forkCopy({ ...a, sessionId, newSessionId: 'c', targetSessionsRoot: join(child, 'sessions'), targetDatabase: join(child, 'timeline.sqlite') });
  assert.deepEqual(result, { forkedThroughTurnId: null, nativeSessionId: null, historyBytes: 0, events: 0 });
  assert.equal(existsSync(join(child, 'timeline.sqlite')), false);
  const never = await forkCopy({ ...a, database: join(a.dir, 'missing.sqlite'), sessionId, newSessionId: 'c', targetSessionsRoot: join(child, 'sessions'), targetDatabase: join(child, 'timeline.sqlite') });
  assert.equal(never.forkedThroughTurnId, null);
});

test('the one-shot entry point prints one JSON line and fails closed', async t => {
  const a = await agent(t, [...settings, ...turn(3, 1), ...turn(8, 2)]);
  const run = config => {
    const file = join(a.dir, `ops-${Math.random()}.json`);
    return writeFile(file, JSON.stringify(config)).then(() => spawnSync(process.execPath, [fileURLToPath(new URL('../src/session-ops.mjs', import.meta.url))], {
      env: { DSH_CUTEX_SESSION_OPS_CONFIG: file }, encoding: 'utf8' }));
  };
  const ok = await run({ op: 'revertPrepare', ...a, sessionId, beforeTurnId: turnId(2), stagingDirectory: join(a.dir, 'stage') });
  assert.equal(ok.status, 0, ok.stderr);
  const lines = ok.stdout.trim().split('\n');
  assert.equal(lines.length, 1);
  assert.deepEqual(JSON.parse(lines[0]).removedTurnIds, [turnId(2)]);
  const refused = await run({ op: 'revertPrepare', ...a, sessionId, beforeTurnId: 'nope', stagingDirectory: join(a.dir, 'stage2') });
  assert.equal(refused.status, 1);
  assert.deepEqual({ ...JSON.parse(refused.stdout), message: undefined }, { ok: false, code: 'turn_not_found', message: undefined });
  const unknown = await run({ op: 'delete' });
  assert.equal(JSON.parse(unknown.stdout).code, 'invalid_request');
  const missing = spawnSync(process.execPath, [fileURLToPath(new URL('../src/session-ops.mjs', import.meta.url))], { env: {}, encoding: 'utf8' });
  assert.equal(missing.status, 1);
  assert.equal(JSON.parse(missing.stdout).ok, false);
});

test('pre-projected timelines redact the secrets the caller passes, like the runtime projection', async t => {
  const secret = 'sk-test-redaction-0123456789';
  const leaky = [...settings, ...turn(3, 1), event(8, 'agent/inbox/spliced', { target: 'next-turn', start: 0, inserted: [] }), event(9, 'turn/start', { turn: 2 }),
    event(10, 'user/message', { id: 'user-2', role: 'user', source: { kind: 'user' }, content: [text(`my key is ${secret}`)] }),
    event(11, 'assistant/message', { turn: 2, step: 1, message: { id: 'assistant-2', role: 'assistant', content: [text(`echo ${secret}`)] } }),
    event(12, 'turn/end', { turn: 2, reason: { kind: 'completed' } }), ...turn(13, 3)];
  const a = await agent(t, leaky);
  const saved = process.env.DSH_CUTEX_REDACT;
  process.env.DSH_CUTEX_REDACT = JSON.stringify([secret]);
  t.after(() => { if (saved === undefined) delete process.env.DSH_CUTEX_REDACT; else process.env.DSH_CUTEX_REDACT = saved; });
  const result = await revertPrepare({ ...a, sessionId, beforeTurnId: turnId(3), stagingDirectory: join(a.dir, 'stage') });
  const child = join(a.dir, 'child');
  await forkCopy({ ...a, sessionId, newSessionId: 'fork', targetSessionsRoot: join(child, 'sessions'), targetDatabase: join(child, 'timeline.sqlite') });
  for (const database of [result.stagedDatabase, join(child, 'timeline.sqlite')]) {
    const store = new TimelineStore(database, { readOnly: true });
    try {
      const dump = JSON.stringify(store.replay()) + JSON.stringify(store.db.prepare('SELECT body, updates FROM native_events').all());
      assert.ok(dump.includes('[REDACTED]'), 'the turn with the secret was projected');
      assert.equal(dump.includes(secret), false, `secret projected into ${database}`);
    } finally { store.close(); }
  }
});

test('C-4: revert and fork carry the kept turns\' original image inputs, so their uploads stay thumbnails', async t => {
  const image = { type: 'image', attachment: { attachmentId: 'att', mediaType: 'image/png', bytes: 3, width: 1, height: 1 } };
  const withImage = (start, n) => turn(start, n).map(e => e.type === 'user/message' ? { ...e, data: { ...e.data, content: [text(`look ${n}`), image] } } : e);
  const a = await agent(t, [...settings, ...withImage(3, 1), ...withImage(8, 2)]);
  const upload = join(a.dir, 'upload.png');
  await writeFile(upload, 'png');
  const source = new TimelineStore(a.database);
  try {
    for (const n of [1, 2]) source.append({ method: 'dsh/turnInputs', params: { threadId: sessionId, turnId: turnId(n), images: [{ type: 'localImage', path: upload }] } });
  } finally { source.close(); }
  const userImages = store => store.replay().filter(e => e.method === 'item/completed' && e.params.item.type === 'userMessage')
    .map(e => [e.params.turnId, e.params.item.content[1].type === 'localImage' ? e.params.item.content[1].path : null]);
  const staging = join(a.dir, 'stage');
  const reverted = await revertPrepare({ ...a, sessionId, beforeTurnId: turnId(2), stagingDirectory: staging });
  let store = new TimelineStore(reverted.stagedDatabase);
  try {
    assert.deepEqual(userImages(store), [[turnId(1), upload]]);
    assert.deepEqual(store.allTurnInputs().map(e => e.params.turnId), [turnId(1)], 'removed turns\' inputs are not carried');
  } finally { store.close(); }
  const child = join(a.dir, 'child');
  await forkCopy({ ...a, sessionId, newSessionId: 'fork-native', afterTurnId: turnId(1), targetSessionsRoot: join(child, 'sessions'), targetDatabase: join(child, 'timeline.sqlite') });
  store = new TimelineStore(join(child, 'timeline.sqlite'));
  try {
    assert.deepEqual(userImages(store), [['fork-native:turn:1', upload]]);
    assert.deepEqual(store.turnInputs('fork-native:turn:1'), { threadId: 'fork-native', turnId: 'fork-native:turn:1', images: [{ type: 'localImage', path: upload }] });
  } finally { store.close(); }
});
