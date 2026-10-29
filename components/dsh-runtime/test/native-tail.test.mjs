import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, appendFile, rm, truncate, rename, copyFile, open } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { zstdCompressSync, constants } from 'node:zlib';
import { TimelineStore } from '../src/store.mjs';
import { HistoryReader } from '../src/history-reader.mjs';
import { loadNativeCatalog, readNativeLog, reconcileNativeLog, decodeNativeRow, NativeLogScanCache, deleteNativeSession, locateNativeLog, writeNativeSessionCopy } from '../src/reconcile.mjs';
import { mkdir, symlink, readdir, readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { spawn } from 'node:child_process';

const catalog = await loadNativeCatalog();
const sessionId = 'native-tail-test';
const header = { version: 4, id: sessionId, createdAt: 1, isSeeded: false, delegationDepth: 0 };
const event = (seq, type, data, extra = {}) => ({ seq, time: seq + 100, type, data, ...extra });
const text = value => ({ type: 'text', text: value });
const image = { type: 'image', attachment: { attachmentId: 'att-1', mediaType: 'image/png', bytes: 5120, width: 32, height: 16, name: 'shot.png' } };
const turn = (start, n) => [
  event(start, 'turn/start', { turn: n }),
  event(start + 1, 'user/message', { id: `user-${n}`, role: 'user', source: { kind: 'user' }, content: [text(`question ${n}`)] }),
  event(start + 2, 'assistant/message', { turn: n, step: 1, message: { id: `assistant-${n}`, role: 'assistant', content: [text(`answer ${n}`)] } }),
  event(start + 3, 'turn/end', { turn: n, reason: { kind: 'completed' } }),
];
const row = item => JSON.stringify(catalog.encodeCurrentEvent(item)) + '\n';
const headerRow = () => JSON.stringify(catalog.encodeCurrentHeader(header, 0)) + '\n';
const textLog = events => headerRow() + events.map(row).join('');
const frame = value => zstdCompressSync(value, { params: { [constants.ZSTD_c_checksumFlag]: 1 } });

async function setup(t, events, { zstd = false } = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'dsh-native-tail-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const logPath = join(dir, zstd ? 'session.v4.jsonl.zstd' : 'session.v4.jsonl');
  if (events) await writeFile(logPath, zstd ? Buffer.concat([frame(headerRow()), frame(events.map(row).join(''))]) : textLog(events));
  return { dir, logPath, sessionId, catalog, database: join(dir, 'timeline.sqlite') };
}
const drain = async options => {
  let result, committed = 0;
  do { result = await reconcileNativeLog(options); committed += result.committed; } while (result.more);
  return { ...result, committed };
};

test('tail decoder is equivalent to the pinned catalog restore, including repair rows with sourceEventSeqs', async t => {
  const events = [
    event(0, 'turn/start', { turn: 1 }),
    event(1, 'tool/call', { turn: 1, step: 1, callId: 'call-1', name: 'bash', arguments: '{}' }),
    event(2, 'tool/call', { turn: 1, step: 1, callId: 'call-2', name: 'bash', arguments: '{}' }),
    event(3, 'tool/result', { turn: 1, step: 1, message: { id: 'r1', role: 'tool', toolCallId: 'call-1', isError: true, source: { kind: 'tool', callId: 'call-1' }, content: [text('unknown outcome')] },
      error: { name: 'ToolOutcomeUnknownError', code: 'TOOL_OUTCOME_UNKNOWN' } }, { surfaceOp: 'append', sourceEventSeqs: [1] }),
    event(4, 'tool/result', { turn: 1, step: 1, message: { id: 'r2', role: 'tool', toolCallId: 'call-2', isError: true, source: { kind: 'tool', callId: 'call-2' }, content: [text('unknown outcome'), image] },
      error: { name: 'ToolOutcomeUnknownError', code: 'TOOL_OUTCOME_UNKNOWN' } }, { surfaceOp: 'append', sourceEventSeqs: [1, 2] }),
    event(5, 'turn/end', { turn: 1, reason: { kind: 'completed' } }),
  ];
  for (const zstd of [false, true]) {
    const options = await setup(t, events, { zstd });
    const snapshot = await readNativeLog(options);
    const decoded = snapshot.events.map(record => record.event);
    assert.equal(decoded.length, events.length);
    // Row-local decoding reproduces the catalog's logical events exactly.
    const rows = (zstd ? textLog(events) : await (await import('node:fs/promises')).readFile(options.logPath, 'utf8')).trim().split('\n').slice(1).map(line => JSON.parse(line));
    assert.ok(rows.some(value => Array.isArray(value.sourceEventSeqs)), 'fixture really carries encoded source ranges');
    rows.forEach((value, seq) => assert.deepEqual(decodeNativeRow(value, seq), decoded[seq]));
    // ...so the tail reader's cursor hash chain equals the r264 whole-prefix chain.
    const store = new TimelineStore(options.database);
    try {
      const result = await drain({ ...options, store });
      assert.equal(result.committed, events.length);
      assert.equal(store.nativeCursor(sessionId).hash, snapshot.events.at(-1).cursor.hash);
      const tool = store.replay().find(item => item.method === 'item/completed' && item.params.item.id === 'call-2').params.item;
      assert.equal(tool.status, 'failed');
      assert.equal(tool.type, 'commandExecution');
      assert.equal(tool.aggregatedOutput, 'unknown outcome\n[Image shot.png: image/png, 32x16, 5.0 KB]');
    } finally { store.close(); }
  }
  assert.throws(() => decodeNativeRow({ ...events[0], extra: 1 }, 0), /unexpected field/);
  assert.throws(() => decodeNativeRow(events[1], 0), /seq gap/);
  assert.throws(() => decodeNativeRow({ ...events[4], sourceEventSeqs: [[3, 9]] }, 4), /exceeds/);
});

test('images in tool results, user messages and assistant messages project as placeholders and never fail the read', async t => {
  const events = [
    event(0, 'turn/start', { turn: 1 }),
    event(1, 'user/message', { id: 'user-1', role: 'user', source: { kind: 'user' }, content: [text('look at this'), image] }),
    event(2, 'tool/call', { turn: 1, step: 1, callId: 'call-1', name: 'read_file', arguments: '{"path":"shot.png"}' }),
    event(3, 'tool/result', { turn: 1, step: 1, message: { id: 'result-1', role: 'tool', source: { kind: 'tool', callId: 'call-1' }, toolCallId: 'call-1', content: [image, text('read 5120 bytes')] } }),
    event(4, 'assistant/message', { turn: 1, step: 2, message: { id: 'assistant-1', role: 'assistant', content: [text('It is a screenshot.'), { ...image, offloaded: true }] } }),
    event(5, 'turn/end', { turn: 1, reason: { kind: 'completed' } }),
  ];
  const options = await setup(t, events);
  const store = new TimelineStore(options.database);
  try {
    const result = await reconcileNativeLog({ ...options, store });
    assert.equal(result.status.state, 'ok');
    assert.equal(result.committed, events.length);
    const completed = store.replay().at(-1).params.turn;
    const user = completed.items.find(item => item.type === 'userMessage');
    assert.deepEqual(user.content.map(block => block.text), ['look at this', '[Image shot.png: image/png, 32x16, 5.0 KB]']);
    assert.deepEqual(user.content[1].dshAttachment, { kind: 'image', ...image.attachment });
    const tool = completed.items.find(item => item.type === 'dynamicToolCall');
    assert.equal(tool.status, 'completed');
    assert.deepEqual(tool.contentItems.map(block => block.text), ['[Image shot.png: image/png, 32x16, 5.0 KB]', 'read 5120 bytes']);
    assert.ok(!JSON.stringify(tool).includes('attachmentId":"att-1","data'), 'no image bytes are invented');
    const agent = completed.items.find(item => item.type === 'agentMessage');
    assert.equal(agent.text, 'It is a screenshot.\n[Image shot.png: image/png, 32x16, 5.0 KB, offloaded]');
  } finally { store.close(); }
});

test('damaged complete row: commit up to the last good record, persist status, resume after DSH truncates and appends', async t => {
  const events = [...turn(0, 1), ...turn(4, 2)];
  const options = await setup(t, events);
  const good = textLog(events);
  await appendFile(options.logPath, '{"type":"assistant/message","seq":8,"tim\n');
  const store = new TimelineStore(options.database);
  const cache = new NativeLogScanCache();
  try {
    const damaged = await reconcileNativeLog({ ...options, store, cache });
    assert.equal(damaged.status.state, 'damaged');
    assert.equal(damaged.status.offset, Buffer.byteLength(good));
    assert.equal(damaged.status.nativeSeq, 7);
    assert.match(damaged.status.reason, /seq 8/);
    assert.equal(damaged.committed, 8, 'every record before the damage is projected');
    assert.equal(store.nativeStatus(sessionId).state, 'damaged');
    assert.equal((await reconcileNativeLog({ ...options, store, cache })).unchanged, true, 'a damaged unchanged tail costs one stat per poll');
    // Offline readers see the same status beside the history.
    store.bind(sessionId, options.dir);
    const reader = new HistoryReader({ database: options.database, threadId: 'facade', nativeSessionId: sessionId, cwd: options.dir });
    try { assert.equal(reader.handle('thread/read', { threadId: 'facade' }).thread.dshNativeLog.state, 'damaged'); }
    finally { reader.close(); }
    // DSH's recoverable scanner truncates to committedBytes, then appends.
    await truncate(options.logPath, Buffer.byteLength(good));
    await appendFile(options.logPath, turn(8, 3).map(row).join(''));
    const recovered = await reconcileNativeLog({ ...options, store, cache });
    assert.equal(recovered.status.state, 'ok');
    assert.equal(recovered.committed, 4);
    assert.equal(store.nativeStatus(sessionId).state, 'ok');
    // A fresh controller (restart) with a damaged-then-fixed log agrees.
    assert.equal((await reconcileNativeLog({ ...options, store })).committed, 0);
  } finally { store.close(); }
});

test('sequence gap, foreign identity, unsupported format, empty file and oversize records are statuses', async t => {
  const options = await setup(t, turn(0, 1));
  const store = new TimelineStore(options.database);
  // Every store is closed before the fixture is removed (Windows: EBUSY otherwise).
  const extra = [];
  const scratch = name => { const opened = new TimelineStore(join(other.dir, name)); extra.push(opened); return opened; };
  let other;
  try {
    await appendFile(options.logPath, row(turn(5, 2)[0]));
    const gap = await reconcileNativeLog({ ...options, store });
    assert.equal(gap.status.state, 'damaged');
    assert.match(gap.status.reason, /seq gap/);
    assert.equal(gap.committed, 4);
    other = await setup(t, turn(0, 1));
    assert.equal((await reconcileNativeLog({ ...other, sessionId: 'someone-else', store })).status.state, 'invalid');
    await writeFile(other.logPath, textLog(turn(0, 1)).replace('"version":4', '"version":5'));
    assert.equal((await reconcileNativeLog({ ...other, store: scratch('v5.sqlite') })).status.state, 'unsupported');
    await writeFile(other.logPath, '');
    assert.equal((await reconcileNativeLog({ ...other, store: scratch('empty.sqlite') })).status.state, 'pending');
    await writeFile(other.logPath, textLog([event(0, 'turn/start', { turn: 1 }), event(1, 'user/message', { id: 'u', role: 'user', source: { kind: 'user' }, content: [text('x'.repeat(4096))] })]));
    const blocked = await reconcileNativeLog({ ...other, store: scratch('big.sqlite'), limits: { maxRecordBytes: 1024, readChunkBytes: 256 } });
    assert.equal(blocked.status.state, 'blocked');
    assert.equal(blocked.committed, 1);
  } finally { store.close(); for (const opened of extra) opened.close(); }
});

test('rotation to a new inode with identical content re-verifies and continues without duplicates', async t => {
  const options = await setup(t, turn(0, 1));
  const store = new TimelineStore(options.database);
  const cache = new NativeLogScanCache();
  try {
    await reconcileNativeLog({ ...options, store, cache });
    const before = store.replay().length;
    await copyFile(options.logPath, options.logPath + '.copy');
    await appendFile(options.logPath + '.copy', turn(4, 2).map(row).join(''));
    await rename(options.logPath + '.copy', options.logPath);
    const rotated = await reconcileNativeLog({ ...options, store, cache, limits: { fullVerifyBelowBytes: 0 } });
    assert.equal(rotated.status.state, 'ok');
    assert.equal(rotated.committed, 4);
    assert.equal(store.replay().length, before + 4, "four projections, no duplicates");
    assert.equal(store.nativeCursor(sessionId).log.ino !== undefined, true);
  } finally { store.close(); }
});

test('r264 cursors without a byte offset are upgraded by one streaming verification', async t => {
  const options = await setup(t, [...turn(0, 1), ...turn(4, 2)]);
  const store = new TimelineStore(options.database);
  try {
    // Simulate r264: commit the first turn with whole-prefix cursors (no `log`).
    const { events } = await readNativeLog(options);
    let previousCursor = null;
    for (const record of events.slice(0, 4)) { store.commitNative({ ...record, previousCursor, updates: [] }); previousCursor = record.cursor; }
    assert.equal(store.nativeCursor(sessionId).log, undefined);
    const result = await drain({ ...options, store, limits: { fullVerifyBelowBytes: 0, maxEventsPerScan: 3 } });
    assert.equal(result.committed, 4);
    assert.equal(result.status.state, 'ok');
    const cursor = store.nativeCursor(sessionId);
    assert.equal(cursor.hash, events.at(-1).cursor.hash);
    assert.equal(cursor.log.offset, Buffer.byteLength(textLog([...turn(0, 1), ...turn(4, 2)])));
  } finally { store.close(); }
});

test('zstd resume points inside a multi-event frame survive budgets and controller restarts', async t => {
  const events = [...turn(0, 1), ...turn(4, 2)];
  const options = await setup(t, events, { zstd: true });
  const store = new TimelineStore(options.database);
  try {
    for (let committed = 0; committed < events.length;) {
      // No cache: every call is a fresh controller resuming from cursor.log.
      const result = await reconcileNativeLog({ ...options, store, limits: { maxEventsPerScan: 3, fullVerifyBelowBytes: 0 } });
      assert.ok(result.committed > 0, 'progress on every call');
      committed += result.committed;
      const { log } = store.nativeCursor(sessionId);
      assert.equal(log.compression, 'zstd');
      if (committed < events.length) assert.ok(log.skip > 0, 'mid-frame cursor records rows already consumed');
    }
    assert.equal(store.nativeCursor(sessionId).nativeSeq, events.length - 1);
    assert.equal(store.nativeCursor(sessionId).log.skip, 0);
    await appendFile(options.logPath, frame(turn(8, 3).map(row).join('')));
    assert.equal((await reconcileNativeLog({ ...options, store, limits: { fullVerifyBelowBytes: 0 } })).committed, 4);
    assert.equal(store.replay().filter(item => item.method === 'turn/completed').length, 3);
  } finally { store.close(); }
});

test('large-log fresh start trusts identity + header + anchor and reads only the tail', async t => {
  const events = Array.from({ length: 200 }, (_, n) => turn(n * 4, n + 1)).flat();
  const options = await setup(t, events);
  const store = new TimelineStore(options.database);
  try {
    await drain({ ...options, store });
    await appendFile(options.logPath, turn(800, 201).map(row).join(''));
    let bytes = 0;
    const probe = await open(options.logPath);
    const handleClass = probe.constructor;
    await probe.close();
    const original = handleClass.prototype.read;
    handleClass.prototype.read = async function (...args) { const result = await original.apply(this, args); bytes += result.bytesRead; return result; };
    let result;
    try { result = await reconcileNativeLog({ ...options, store, limits: { fullVerifyBelowBytes: 0, readChunkBytes: 4096 } }); }
    finally { handleClass.prototype.read = original; }
    assert.equal(result.committed, 4);
    assert.ok(bytes < 3 * 4096 + 2048, `fresh controller read ${bytes} bytes, not the ${events.length}-event prefix`);
  } finally { store.close(); }
});

test('timeline index: paged history reads only requested turns; old DBs are backfilled once', async t => {
  const options = await setup(t, Array.from({ length: 30 }, (_, n) => turn(n * 4, n + 1)).flat());
  let store = new TimelineStore(options.database);
  store.bind(sessionId, options.dir);
  await drain({ ...options, store });
  assert.equal(store.turnCount(), 30);
  assert.equal(store.turnCompleted(`${sessionId}:turn:30`), true);
  assert.equal(store.turnEvents(`${sessionId}:turn:2`).length, 4);
  // Drop the derived index (as in an r264 database) and reopen writable: backfill restores it.
  store.db.exec("DELETE FROM timeline_turns; DELETE FROM timeline_event_turns; DELETE FROM store_meta;");
  store.close();
  store = new TimelineStore(options.database);
  assert.equal(store.turnCount(), 30);
  store.append({ method: 'dsh/contextUsage/updated', params: { used: 7, size: 100 } });
  assert.equal(store.latest('dsh/contextUsage/updated').params.used, 7);
  store.close();
  const reader = new HistoryReader({ database: options.database, threadId: 'facade', nativeSessionId: sessionId, cwd: options.dir });
  try {
    const page = reader.handle('thread/turns/list', { threadId: 'facade', limit: 5 });
    assert.deepEqual(page.data.map(item => item.id), [30, 29, 28, 27, 26].map(n => `${sessionId}:turn:${n}`));
    const items = [];
    let cursor = null;
    do {
      const result = reader.handle('thread/items/list', { threadId: 'facade', sortDirection: 'asc', limit: 7, ...(cursor ? { cursor } : {}) });
      items.push(...result.data);
      cursor = result.nextCursor;
    } while (cursor);
    assert.equal(items.length, 60);
    assert.deepEqual(items.slice(0, 3).map(entry => entry.item.id), ['user-1', 'agent_message_chunk:assistant-1', 'user-2']);
    const snapshot = reader.handle('thread/read', { threadId: 'facade', includeTurns: true }).thread;
    assert.equal(snapshot.turns.length, 30);
    assert.equal(snapshot.dshContext.used, 7);
    assert.equal(snapshot.dshNativeLog.state, 'ok');
  } finally { reader.close(); }
});

test('native delete removes the session, its subagent children and the timeline DB; refuses links and live writers', async t => {
  const { dir } = await setup(t);
  const root = join(dir, 'sessions');
  const write = async (project, id, extra = {}) => {
    const directory = join(root, project, id);
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, 'session.v4.jsonl'), JSON.stringify(catalog.encodeCurrentHeader({ ...header, id, ...extra }, 0)) + '\n');
    await writeFile(join(directory, 'session.lock'), '');
    return directory;
  };
  const main = await write('--project--', sessionId);
  const child = await write('--project--', 'child-1', { parentSession: sessionId, origin: 'subagent', delegationDepth: 1 });
  const grandchild = await write('--other--', 'child-2', { parentSession: 'child-1', origin: 'subagent', delegationDepth: 2 });
  const unrelated = await write('--project--', 'unrelated');
  const database = join(dir, 'timeline.sqlite');
  new TimelineStore(database).close();
  assert.equal(await locateNativeLog(root, sessionId), join(main, 'session.v4.jsonl'));

  if (process.platform === 'linux' && existsSync('/usr/bin/flock')) {
    const holder = spawn('/usr/bin/flock', ['-n', '-o', join(child, 'session.lock'), 'sleep', '30'], { stdio: 'ignore', detached: true });
    const stop = () => { try { process.kill(-holder.pid, 'SIGKILL'); } catch { /* already gone */ } };
    t.after(stop);
    const deadline = Date.now() + 5000;
    const inode = String((await (await import('node:fs/promises')).stat(join(child, 'session.lock'))).ino);
    while (Date.now() < deadline && !(await readFile('/proc/locks', 'utf8')).split('\n').some(line => line.includes('FLOCK') && line.includes(`:${inode} `))) await new Promise(resolve => setTimeout(resolve, 20));
    await assert.rejects(deleteNativeSession({ sessionsRoot: root, sessionId, database }), error => error.code === 'DSH_NATIVE_SESSION_ACTIVE');
    assert.ok(existsSync(main) && existsSync(child) && existsSync(database), 'a refused delete touches nothing');
    const exited = new Promise(resolve => holder.once('exit', resolve));
    stop();
    await exited;
  }
  const result = await deleteNativeSession({ sessionsRoot: root, sessionId, database });
  assert.deepEqual(result.removed.slice(0, 3).sort(), [child, grandchild, main].sort());
  assert.ok(result.removed.includes(database));
  assert.ok(existsSync(unrelated));
  assert.equal(existsSync(database), false);
  assert.equal(await locateNativeLog(root, sessionId), null);
  assert.equal(existsSync(`${root}.cutex-deleted`), false);
  assert.deepEqual((await deleteNativeSession({ sessionsRoot: root, sessionId, database })).removed, [], 'idempotent');

  const outside = join(dir, 'outside');
  await mkdir(outside);
  await symlink(outside, join(root, '--project--', 'linked'), process.platform === 'win32' ? 'junction' : 'dir');
  assert.deepEqual((await deleteNativeSession({ sessionsRoot: root, sessionId: 'linked' })).removed, [], 'linked directories are never followed');
  assert.ok(existsSync(outside));
  await assert.rejects(deleteNativeSession({ sessionsRoot: 'relative', sessionId }), /absolute/);
  assert.deepEqual(await readdir(join(root, '--project--')), ['linked', 'unrelated']);
});

test('batch commits honour synchronous and asynchronous instance wrappers of commitNative', async t => {
  const options = await setup(t, [...turn(0, 1), ...turn(4, 2)]);
  const store = new TimelineStore(options.database);
  try {
    const original = store.commitNative.bind(store);
    const seen = [];
    let release;
    const gate = new Promise(resolve => { release = resolve; });
    // Sync wrapper for most events; an async one that commits only later for one event.
    store.commitNative = record => {
      seen.push(record.nativeSeq);
      if (record.nativeSeq === 6) return gate.then(() => original(record));
      return original({ ...record, event: record.event.type === 'user/message' ? { ...record.event, data: { ...record.event.data, content: [text('[redacted]')] } } : record.event });
    };
    const scan = reconcileNativeLog({ ...options, store });
    await new Promise(resolve => setTimeout(resolve, 50));
    assert.equal(store.nativeCursor(sessionId).nativeSeq, 5, "prefix before the async wrapper is durable");
    release();
    const result = await scan;
    assert.equal(result.committed, 8);
    assert.equal(store.nativeCursor(sessionId).nativeSeq, 7);
    assert.deepEqual(seen, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert.ok(store.replay().filter(item => item.params?.item?.type === 'userMessage').every(item => item.params.item.content[0].text === '[redacted]'));
  } finally { store.close(); }
});

test('native delete can move sessions and the timeline into a recovery directory instead', async t => {
  const { dir } = await setup(t);
  const root = join(dir, 'sessions');
  const directory = join(root, '--p--', sessionId);
  await mkdir(directory, { recursive: true });
  await writeFile(join(directory, 'session.v4.jsonl'), headerRow());
  const database = join(dir, 'timeline.sqlite');
  new TimelineStore(database).close();
  const recovery = join(dir, 'recovery');
  const result = await deleteNativeSession({ sessionsRoot: root, sessionId, database, recoveryDirectory: recovery });
  assert.equal(result.recoveryDirectory, recovery);
  assert.equal(existsSync(directory), false);
  assert.equal(await readFile(join(recovery, 'dsh-sessions', '--p--', sessionId, 'session.v4.jsonl'), 'utf8'), headerRow());
  assert.ok(existsSync(join(recovery, 'timeline.sqlite')));
  assert.equal(existsSync(database), false);
});

test('HistoryReader.attach pages a live writable store without taking ownership', async t => {
  const options = await setup(t, [...turn(0, 1), event(4, 'turn/start', { turn: 2 })]);
  const store = new TimelineStore(options.database);
  try {
    store.bind(sessionId, options.dir);
    await reconcileNativeLog({ ...options, store });
    const reader = HistoryReader.attach(store, { threadId: 'facade', cwd: options.dir, liveTurnId: () => `${sessionId}:turn:2` });
    const page = reader.handle('thread/turns/list', { threadId: 'facade', itemsView: 'notLoaded' });
    assert.deepEqual(page.data.map(entry => [entry.id, entry.status]), [[`${sessionId}:turn:2`, 'inProgress'], [`${sessionId}:turn:1`, 'completed']]);
    reader.close();
    assert.equal(store.turnCount(), 2, 'store still open after attached reader closes');
  } finally { store.close(); }
});

test('writeNativeSessionCopy copies a closed prefix byte-for-byte under a new unseeded header (text and zstd)', async t => {
  for (const zstd of [false, true]) {
    const { dir } = await setup(t);
    const root = join(dir, 'sessions');
    const directory = join(root, '--work--', sessionId);
    await mkdir(directory, { recursive: true });
    const events = [...turn(0, 1), ...turn(4, 2)];
    const body = textLog(events);
    await writeFile(join(directory, zstd ? 'session.v4.jsonl.zstd' : 'session.v4.jsonl'), zstd ? Buffer.concat([frame(headerRow()), frame(events.map(row).join(''))]) : body);
    await assert.rejects(writeNativeSessionCopy({ sessionsRoot: root, sourceSessionId: sessionId, newSessionId: 'copy', throughSeq: 2, catalog }), /turn\/end/);
    await assert.rejects(writeNativeSessionCopy({ sessionsRoot: root, sourceSessionId: sessionId, newSessionId: 'copy', throughSeq: 99, catalog }), /ends before/);
    const other = join(dir, 'other-root');
    const copy = await writeNativeSessionCopy({ sessionsRoot: root, sourceSessionId: sessionId, newSessionId: 'copy', throughSeq: 3, catalog, targetSessionsRoot: other });
    assert.equal(copy.events, 4);
    assert.equal(await locateNativeLog(other, 'copy'), copy.logPath);
    assert.ok(copy.logPath.includes('--work--'));
    await assert.rejects(writeNativeSessionCopy({ sessionsRoot: root, sourceSessionId: sessionId, newSessionId: 'copy', throughSeq: 3, catalog, targetSessionsRoot: other }), /already exists/);
    const snapshot = await readNativeLog({ logPath: copy.logPath, sessionId: 'copy', catalog });
    assert.deepEqual(snapshot.events.map(record => record.event), events.slice(0, 4));
    assert.equal(snapshot.header.isSeeded, false);
    assert.equal(snapshot.header.parentSession, undefined);
    const store = new TimelineStore(join(dir, `copy-${zstd}.sqlite`));
    try { assert.equal((await reconcileNativeLog({ logPath: copy.logPath, sessionId: 'copy', catalog, store })).committed, 4); }
    finally { store.close(); }
    const empty = await writeNativeSessionCopy({ sessionsRoot: root, sourceSessionId: sessionId, newSessionId: 'empty', throughSeq: -1, catalog });
    assert.equal(empty.events, 0);
  }
  const { dir } = await setup(t);
  const root = join(dir, 'sessions');
  await mkdir(join(root, '--w--', 'child'), { recursive: true });
  await writeFile(join(root, '--w--', 'child', 'session.v4.jsonl'), JSON.stringify(catalog.encodeCurrentHeader({ ...header, id: 'child', parentSession: 'p', isSeeded: false, origin: 'subagent', delegationDepth: 1 }, 0)) + '\n');
  await assert.rejects(writeNativeSessionCopy({ sessionsRoot: root, sourceSessionId: 'child', newSessionId: 'x', throughSeq: -1, catalog }), /top-level/);
});
