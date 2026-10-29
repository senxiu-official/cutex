import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, stat, rm, symlink } from 'node:fs/promises';
import { DatabaseSync } from 'node:sqlite';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { TimelineStore } from '../src/store.mjs';
import { loadNativeCatalog, readNativeLog, reconcileNativeLog, locateNativeLog } from '../src/reconcile.mjs';
import { projectNativeEvent } from '../src/nativeProjection.mjs';

const catalog = await loadNativeCatalog();
const sessionId = 'native-store-test';
const turnId = `${sessionId}:turn:1`;
const event = (seq, type, data) => ({ seq, time: seq + 10, type, data });
const fixture = [
  event(0, 'turn/start', { turn: 1 }),
  event(1, 'user/message', { id: 'user-1', role: 'user', source: { kind: 'user' }, content: [{ type: 'text', text: 'hello' }] }),
  event(2, 'assistant/message', { turn: 1, step: 1, message: { id: 'assistant-1', role: 'assistant', content: [
    { type: 'reasoning', text: 'think ' }, { type: 'text', text: 'hello ' },
    { type: 'reasoning', text: 'more' }, { type: 'text', text: 'world' },
  ] } }),
  event(3, 'tool/call', { turn: 1, step: 1, callId: 'call-1', name: 'native-tool', arguments: '{"value":42}' }),
  event(4, 'tool/result', { turn: 1, step: 1, message: { id: 'result-1', role: 'tool', source: { kind: 'tool', callId: 'call-1' },
    toolCallId: 'call-1', content: [{ type: 'text', text: 'tool output' }] } }),
  event(5, 'turn/end', { turn: 1, reason: { kind: 'completed' } }),
];
const logText = (events = fixture, id = sessionId) => [
  catalog.encodeCurrentHeader({ version: 4, id, createdAt: 1, isSeeded: false, delegationDepth: 0 }, 0),
  ...events.map(event => catalog.encodeCurrentEvent(event)),
].map(row => JSON.stringify(row) + '\n').join('');

async function setup(t, events = fixture) {
  const dir = await mkdtemp(join(tmpdir(), 'native-store-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const logPath = join(dir, 'session.v4.jsonl');
  await writeFile(logPath, logText(events));
  return { dir, database: join(dir, 'history.sqlite'), logPath, sessionId, catalog };
}

function counts(store) {
  return Object.fromEntries(['native_events', 'native_items', 'native_cursors', 'events'].map(table =>
    [table, store.db.prepare(`SELECT COUNT(*) AS count FROM ${table}`).get().count]));
}

test('production store atomically retains raw facts, ACP updates and complete projected history', async t => {
  const options = await setup(t);
  const store = new TimelineStore(options.database);
  try {
    assert.equal(store.nextNativeTurn(sessionId), 1);
    const notifications = [];
    store.onNativeCommitted = events => {
      // A second connection can see projections and cursor when notified.
      const observer = new TimelineStore(options.database);
      try {
        assert.deepEqual(observer.nativeCursor(sessionId), store.nativeCursor(sessionId));
        assert.deepEqual(observer.replay(), store.replay());
      } finally { observer.close(); }
      notifications.push(...events);
    };
    const result = await reconcileNativeLog({ ...options, store });
    assert.equal(result.committed, fixture.length);
    assert.deepEqual(notifications, store.replay());
    assert.deepEqual(counts(store), { native_events: 6, native_items: 4, native_cursors: 1, events: 7 });
    const raw = store.db.prepare('SELECT body, updates FROM native_events WHERE native_seq=2').get();
    assert.deepEqual(JSON.parse(raw.body), fixture[2]);
    assert.equal(JSON.parse(raw.updates).length, 4);
    const timeline = store.replay();
    assert.equal(timeline[0].params.turn.id, turnId);
    assert.equal(timeline[1].params.turnId, turnId, 'user/message inherits its committed turn');
    const completed = timeline.at(-1).params.turn;
    assert.equal(completed.status, 'completed');
    assert.equal(completed.items.find(item => item.type === 'agentMessage').text, 'hello world');
    assert.deepEqual(completed.items.find(item => item.type === 'reasoning').content, ['think more']);
    // An unmapped DSH tool is a generic dynamic tool call (toolItems.mjs), duration from the native times.
    const tool = completed.items.find(item => item.type === 'dynamicToolCall');
    assert.deepEqual(tool, { id: 'call-1', type: 'dynamicToolCall', namespace: 'dsh', tool: 'native-tool', arguments: { value: 42 },
      status: 'completed', contentItems: [{ type: 'inputText', text: 'tool output' }], success: true, durationMs: 1 });
    assert.equal(store.maxNativeTurn(sessionId), 1);
    assert.equal(store.nextNativeTurn(sessionId), 2);
    assert.equal(store.nextNativeTurn('another-session'), 1);
    assert.equal((await reconcileNativeLog({ ...options, store })).committed, 0);
    assert.equal(notifications.length, timeline.length, 'dedup sends no notifications');
  } finally { store.close(); }
});

for (const point of ['before', 'after']) {
  test(`production store survives process.exit ${point} COMMIT with no partial state or duplicate projection`, async t => {
    const options = await setup(t);
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
      import { TimelineStore } from ${JSON.stringify(new URL('../src/store.mjs', import.meta.url).href)};
      import { reconcileNativeLog } from ${JSON.stringify(new URL('../src/reconcile.mjs', import.meta.url).href)};
      const store = new TimelineStore(process.argv[1]);
      const exec = store.db.exec.bind(store.db);
      // Fault injection lives entirely in the test. Production store has no hooks.
      store.db.exec = sql => {
        const target = sql === 'COMMIT' && store.nativeCursor(${JSON.stringify(sessionId)})?.nativeSeq === 2;
        if (target && process.argv[3] === 'before') process.exit(87);
        const result = exec(sql);
        if (target && process.argv[3] === 'after') process.exit(87);
        return result;
      };
      // One native event per transaction, so the crash lands between events.
      await reconcileNativeLog({ store, logPath: process.argv[2], sessionId: ${JSON.stringify(sessionId)}, limits: { commitBatchEvents: 1 } });
    `, options.database, options.logPath, point], { encoding: 'utf8', timeout: 15000 });
    assert.equal(child.status, 87, child.stderr);
    const store = new TimelineStore(options.database);
    try {
      assert.equal(store.nativeCursor(sessionId).nativeSeq, point === 'before' ? 1 : 2);
      assert.deepEqual(counts(store), point === 'before'
        ? { native_events: 2, native_items: 1, native_cursors: 1, events: 2 }
        : { native_events: 3, native_items: 3, native_cursors: 1, events: 4 });
      await reconcileNativeLog({ ...options, store });
      assert.deepEqual(counts(store), { native_events: 6, native_items: 4, native_cursors: 1, events: 7 });
      assert.equal(store.replay().at(-1).params.turn.items.length, 4);
      assert.equal((await reconcileNativeLog({ ...options, store })).committed, 0);
    } finally { store.close(); }
  });
}

test('cursor CAS across connections and sequence gap fail without partial writes; orphan tool result still projects', async t => {
  const options = await setup(t);
  const store = new TimelineStore(options.database);
  const second = new TimelineStore(options.database);
  try {
    const { events } = await readNativeLog(options);
    store.commitNative({ ...events[0], previousCursor: null, updates: [] });
    assert.throws(() => second.commitNative({ ...events[0], previousCursor: null, updates: [] }), /CAS/);
    const before = counts(store);
    assert.throws(() => store.commitNative({ ...events[2], previousCursor: events[0].cursor, updates: [] }), /sequence gap/);
    assert.throws(() => store.commitNativeBatch([{ ...events[1], previousCursor: events[0].cursor, updates: [] },
      { ...events[3], previousCursor: events[1].cursor, updates: [] }]), /sequence gap/, 'a failing batch rolls back entirely');
    assert.deepEqual(counts(store), before);
    assert.deepEqual(store.nativeCursor(sessionId), events[0].cursor);
    // A result whose call is not visible (damaged/seeded prefix) must not brick later history.
    const orphan = { ...events[1], event: { ...fixture[4], seq: 1 }, previousCursor: events[0].cursor,
      updates: [{ sessionUpdate: 'tool_call_update', toolCallId: 'call-1', status: 'completed', content: [] }] };
    const [projected] = store.commitNative(orphan);
    assert.deepEqual(projected.params.item, { id: 'call-1', type: 'dynamicToolCall', namespace: 'dsh', tool: 'unknown', arguments: {}, status: 'completed', contentItems: [], success: true, durationMs: null });
  } finally { store.close(); second.close(); }
});

test('post-commit observer failure keeps durable state and retry deduplicates', async t => {
  const options = await setup(t);
  const store = new TimelineStore(options.database);
  try {
    store.onNativeCommitted = () => { throw new Error('observer unavailable'); };
    await assert.rejects(reconcileNativeLog({ ...options, store, limits: { commitBatchEvents: 1 } }), /observer/);
    assert.equal(store.nativeCursor(sessionId).nativeSeq, 0);
    store.onNativeCommitted = undefined;
    await reconcileNativeLog({ ...options, store });
    assert.equal(store.replay().filter(event => event.method === 'turn/started').length, 1);
    const baseline = store.replay();
    const seq = store.append({ method: 'legacy/event', params: { value: 'unchanged' } });
    assert.deepEqual(store.replay(seq - 1), [{ seq, method: 'legacy/event', params: { value: 'unchanged' } }]);
    assert.deepEqual(store.replay().slice(0, -1), baseline);
  } finally { store.close(); }
});

test('native reason variants preserve failure facts and interrupted state without billing', () => {
  for (const [kind, status] of [['completed', 'completed'], ['interrupted', 'interrupted'], ['aborted', 'interrupted'],
    ['forked', 'interrupted'], ['blocked', 'failed'], ['max-tokens', 'failed'], ['future-reason', 'failed'], ['error', 'failed']]) {
    const reason = { kind, ...(kind === 'error' ? { error: { message: 'model failed', code: 'UNKNOWN' } } : {}) };
    const [projection] = projectNativeEvent({ sessionId, event: event(0, 'turn/end', { turn: 1, reason }), updates: [] }, { turn: 1 });
    assert.equal(projection.params.turn.status, status);
    assert.deepEqual(projection.params.turn.items, []);
    assert.deepEqual(projection.params.turn.dshEndReason, reason);
    assert.equal('tokenUsage' in projection.params, false);
  }
});

test('the projection itself hides every non-user source kind of a user message (store guard not needed)', () => {
  const project = source => projectNativeEvent({ sessionId, event: event(1, 'user/message', { id: 'm', role: 'user',
    ...(source === undefined ? {} : { source }), content: [{ type: 'text', text: 'x' }] }), updates: [] }, { turn: 1 });
  for (const kind of ['runtime-context', 'model-selection', 'future-injected-kind']) assert.deepEqual(project({ kind }), [], kind);
  assert.equal(project({ kind: 'user' })[0].params.item.type, 'userMessage');
  assert.equal(project(undefined)[0].params.item.type, 'userMessage', 'no source: shown (never lose input)');
  assert.equal(project({})[0].params.item.type, 'userMessage', 'source without kind: shown');
});

test('native turn prediction uses persisted maximum rather than event count or current prompt', async t => {
  const options = await setup(t, [...fixture, event(6, 'turn/start', { turn: 9 })]);
  let store = new TimelineStore(options.database);
  await reconcileNativeLog({ ...options, store });
  store.close();
  store = new TimelineStore(options.database);
  try { assert.equal(store.nextNativeTurn(sessionId), 10); }
  finally { store.close(); }
});

test('tool result after store reopen recovers full metadata from prior native call', async t => {
  const options = await setup(t, fixture.slice(0, 4));
  let store = new TimelineStore(options.database);
  await reconcileNativeLog({ ...options, store });
  store.close();
  const failed = structuredClone(fixture);
  failed[4].data.message.isError = true;
  await writeFile(options.logPath, logText(failed));
  store = new TimelineStore(options.database);
  try {
    await reconcileNativeLog({ ...options, store });
    const result = store.replay().find(row => row.method === 'item/completed' && row.params.item.id === 'call-1').params.item;
    assert.equal(result.tool, 'native-tool');
    assert.deepEqual(result.arguments, { value: 42 });
    assert.equal(result.status, 'failed');
    assert.deepEqual(store.replay().at(-1).params.turn.items.find(item => item.id === 'call-1'), result);
  } finally { store.close(); }
});

test('explicit-root discovery waits for materialization, encodes IDs and selects canonical highest generation', async t => {
  const { dir } = await setup(t);
  const root = join(dir, 'native-root');
  assert.equal(await locateNativeLog(root, sessionId), null);
  await assert.rejects(locateNativeLog('relative', sessionId), /absolute/);
  const directory = join(root, '--project--', '..~002Fsecret~005C~007E');
  await mkdir(directory, { recursive: true });
  assert.equal(await locateNativeLog(root, '../secret\\~'), null);
  await writeFile(join(directory, 'session.v4.jsonl.tmp'), 'ignore');
  await writeFile(join(directory, 'session.v04.jsonl'), 'ignore');
  assert.equal(await locateNativeLog(root, '../secret\\~'), null);
  await writeFile(join(directory, 'session.v3.jsonl'), 'old');
  await writeFile(join(directory, 'session.v4.jsonl'), logText([], '../secret\\~'));
  assert.equal(await locateNativeLog(root, '../secret\\~'), join(directory, 'session.v4.jsonl'));
  await writeFile(join(directory, 'session.v5.jsonl'), 'future generation');
  assert.equal(await locateNativeLog(root, '../secret\\~'), join(directory, 'session.v5.jsonl'), 'never silently falls back to old format');
  await writeFile(join(directory, 'session.v4.jsonl.zstd'), 'conflicting encoding');
  await assert.rejects(locateNativeLog(root, '../secret\\~'), /compression/);
});

test('discovery refuses duplicate identities, flat artifacts, and session directory junctions', async t => {
  const { dir } = await setup(t);
  const root = join(dir, 'root');
  for (const project of ['one', 'two']) {
    const directory = join(root, project, sessionId);
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, 'session.v4.jsonl'), logText());
  }
  await assert.rejects(locateNativeLog(root, sessionId), /Duplicate/);
  await rm(join(root, 'two'), { recursive: true });
  await writeFile(join(root, 'one', `${sessionId}.jsonl`), 'obsolete');
  await assert.rejects(locateNativeLog(root, sessionId), /Legacy/);
  await rm(join(root, 'one', `${sessionId}.jsonl`));
  const outside = join(dir, 'outside');
  await mkdir(outside);
  await rm(join(root, 'one', sessionId), { recursive: true });
  await symlink(outside, join(root, 'one', sessionId), process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(locateNativeLog(root, sessionId), /symlink/);
});

test('read-only history reader sees committed native history and cannot write by API or SQLite', async t => {
  const options = await setup(t);
  const writer = new TimelineStore(options.database);
  let reader;
  try {
    writer.bind(sessionId, options.dir);
    reader = new TimelineStore(options.database, { readOnly: true });
    assert.equal(reader.db.prepare('PRAGMA query_only').get().query_only, 1);
    assert.deepEqual(reader.replay(), []);
    await reconcileNativeLog({ ...options, store: writer });
    assert.deepEqual(reader.replay(), writer.replay(), 'separate reader sees subsequent committed WAL data');
    assert.deepEqual(reader.binding(), writer.binding());
    assert.deepEqual(reader.nativeCursor(sessionId), writer.nativeCursor(sessionId));
    assert.equal(reader.nextNativeTurn(sessionId), 2);
    const before = counts(writer);
    assert.throws(() => reader.bind('other', options.dir), /read-only/);
    assert.throws(() => reader.append({ method: 'bad', params: {} }), /read-only/);
    assert.throws(() => reader.commitNative({}), /read-only/);
    assert.throws(() => reader.db.exec('DELETE FROM events'), /readonly|read.only/i);
    reader.db.exec('PRAGMA query_only=OFF');
    assert.throws(() => reader.db.exec('DELETE FROM events'), /readonly|read.only/i, 'open flag still forbids writes');
    assert.deepEqual(counts(writer), before);
  } finally { reader?.close(); writer.close(); }
});

test('read-only opening never creates missing DB, migrates old schema, or switches DELETE journal to WAL', async t => {
  const options = await setup(t);
  const missing = join(options.dir, 'missing.sqlite');
  assert.throws(() => new TimelineStore(missing, { readOnly: true }));
  await assert.rejects(stat(missing), error => error.code === 'ENOENT');
  const oldPath = join(options.dir, 'old-schema.sqlite');
  const old = new DatabaseSync(oldPath);
  old.exec("CREATE TABLE binding (id INTEGER PRIMARY KEY, session_id TEXT, cwd TEXT); CREATE TABLE events (seq INTEGER PRIMARY KEY, body TEXT); INSERT INTO events VALUES (1, '{\"method\":\"old/event\",\"params\":{}}');");
  old.close();
  const before = await readFile(oldPath);
  const reader = new TimelineStore(oldPath, { readOnly: true });
  try {
    assert.equal(reader.db.prepare('PRAGMA journal_mode').get().journal_mode, 'delete');
    assert.deepEqual(reader.db.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").all().map(row => row.name), ['binding', 'events']);
    assert.equal(reader.replay()[0].method, 'old/event');
    assert.throws(() => reader.nativeCursor(sessionId), /no such table/, 'old schema is not silently migrated');
  } finally { reader.close(); }
  assert.deepEqual(await readFile(oldPath), before);
  await assert.rejects(stat(oldPath + '-wal'), error => error.code === 'ENOENT');
});

test('DSH-injected user messages (runtime context, model change) stay raw and are not shown as owner messages', async t => {
  const injected = [
    event(0, 'turn/start', { turn: 1 }),
    event(1, 'user/message', { id: 'ctx-1', role: 'user', source: { kind: 'runtime-context' }, content: [{ type: 'text', text: 'Current runtime context. This snapshot supersedes...' }] }),
    event(2, 'user/message', { id: 'model-1', role: 'user', source: { kind: 'model-selection' }, content: [{ type: 'text', text: '[model changed: assistant turns above this point were generated by deepseek-flash]' }] }),
    event(3, 'user/message', { id: 'user-1', role: 'user', source: { kind: 'user' }, content: [{ type: 'text', text: 'hello' }] }),
    event(4, 'assistant/message', { turn: 1, step: 1, message: { id: 'assistant-1', role: 'assistant', content: [{ type: 'text', text: 'hi' }] } }),
    event(5, 'turn/end', { turn: 1, reason: { kind: 'completed' } }),
  ];
  const options = await setup(t, injected);
  const store = new TimelineStore(options.database);
  try {
    const result = await reconcileNativeLog({ ...options, store });
    assert.equal(result.committed, injected.length);
    assert.equal(counts(store).native_events, injected.length, 'the injected facts stay raw');
    const timeline = store.replay();
    const users = timeline.filter(e => e.params?.item?.type === 'userMessage').map(e => e.params.item.id);
    assert.deepEqual(users, ['user-1']);
    const completed = timeline.at(-1).params.turn;
    assert.deepEqual(completed.items.filter(item => item.type === 'userMessage').map(item => item.id), ['user-1']);
    assert.equal(JSON.stringify(timeline).includes('Current runtime context'), false);
    assert.equal(JSON.stringify(timeline).includes('[model changed'), false);
  } finally { store.close(); }
});

test('C-4: an owner upload in a user message is its localImage (as cute-codex echoes it); anything else stays a placeholder', async t => {
  const image = name => ({ type: 'image', attachment: { attachmentId: `att-${name}`, mediaType: 'image/png', bytes: 5120, width: 32, height: 16, name } });
  const withImages = [
    event(0, 'turn/start', { turn: 1 }),
    event(1, 'user/message', { id: 'user-1', role: 'user', source: { kind: 'user' }, content: [{ type: 'text', text: 'look' }, image('a.png'), image('b.png'), image('c.png')] }),
    event(2, 'turn/end', { turn: 1, reason: { kind: 'completed' } }),
    event(3, 'turn/start', { turn: 2 }),
    event(4, 'user/message', { id: 'user-2', role: 'user', source: { kind: 'user' }, content: [{ type: 'text', text: 'again' }, image('d.png')] }),
    event(5, 'turn/end', { turn: 2, reason: { kind: 'completed' } }),
    event(6, 'turn/start', { turn: 3 }),
    event(7, 'user/message', { id: 'user-3', role: 'user', source: { kind: 'user' }, content: [image('e.png')] }),
    event(8, 'turn/end', { turn: 3, reason: { kind: 'completed' } }),
  ];
  const options = await setup(t, withImages);
  const upload = join(options.dir, 'upload.png'), deleted = join(options.dir, 'gone.png');
  await writeFile(upload, 'png');
  const store = new TimelineStore(options.database);
  try {
    // Recorded by the adapter before session/prompt (server.mjs turn/start).
    store.append({ method: 'dsh/turnInputs', params: { threadId: sessionId, turnId, images: [
      { type: 'localImage', path: upload }, { type: 'image', sha256: 'a'.repeat(64) }, { type: 'localImage', path: deleted }] } });
    // Turn 2: two recorded inputs for one image block: no guessing, placeholders.
    store.append({ method: 'dsh/turnInputs', params: { threadId: sessionId, turnId: `${sessionId}:turn:2`, images: [
      { type: 'localImage', path: upload }, { type: 'localImage', path: upload }] } });
    await reconcileNativeLog({ ...options, store });
    const users = Object.fromEntries(store.replay().filter(e => e.params?.item?.type === 'userMessage').map(e => [e.params.item.id, e.params.item.content]));
    assert.deepEqual(users['user-1'], [
      { type: 'text', text: 'look' },
      { type: 'localImage', path: upload, dshAttachment: { kind: 'image', attachmentId: 'att-a.png', mediaType: 'image/png', bytes: 5120, width: 32, height: 16, name: 'a.png' } },
      { type: 'text', text: '[Image b.png: image/png, 32x16, 5.0 KB]', dshAttachment: { kind: 'image', attachmentId: 'att-b.png', mediaType: 'image/png', bytes: 5120, width: 32, height: 16, name: 'b.png' } },
      { type: 'text', text: '[Image c.png: image/png, 32x16, 5.0 KB]', dshAttachment: { kind: 'image', attachmentId: 'att-c.png', mediaType: 'image/png', bytes: 5120, width: 32, height: 16, name: 'c.png' } },
    ]);
    assert.deepEqual(users['user-2'].map(part => part.type), ['text', 'text'], 'count mismatch keeps placeholders');
    assert.deepEqual(users['user-3'].map(part => part.type), ['text'], 'no recorded inputs (data URL turn, older timeline) keeps the placeholder');
    // The only path ever emitted is the owner's own input; the turnInputs fact is not a timeline item.
    const paths = JSON.stringify(store.replay().filter(e => !e.method.startsWith('dsh/'))).match(/"path":"[^"]*"/g);
    assert.deepEqual([...new Set(paths)], [`"path":${JSON.stringify(upload)}`], 'item/completed and the turn snapshot');
    assert.equal(store.turnIds().length, 3);
  } finally { store.close(); }
});

test('C-4: the projection maps recorded originals only for owner messages with matching image blocks', () => {
  const block = { type: 'image', attachment: { mediaType: 'image/jpeg' } };
  const record = content => ({ sessionId, event: event(1, 'user/message', { id: 'u', source: { kind: 'user' }, content }), updates: [] });
  const project = (content, inputs) => projectNativeEvent(record(content), { turn: 1, inputs })[0].params.item.content;
  assert.deepEqual(project([block], [{ type: 'localImage', path: '/att/s/x.jpg' }]), [{ type: 'localImage', path: '/att/s/x.jpg', dshAttachment: { kind: 'image', mediaType: 'image/jpeg' } }]);
  assert.equal(project([block], undefined)[0].type, 'text');
  assert.equal(project([block], [null])[0].type, 'text');
  assert.equal(project([block], [{ type: 'image', sha256: 'f'.repeat(64) }])[0].type, 'text');
  assert.equal(project([block, block], [{ type: 'localImage', path: '/a.jpg' }])[0].type, 'text');
  // Files are never mapped to images.
  assert.equal(project([{ type: 'file', attachment: { name: 'f.txt' } }], [{ type: 'localImage', path: '/a.jpg' }])[0].type, 'text');
});
