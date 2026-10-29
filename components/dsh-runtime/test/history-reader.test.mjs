import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, existsSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { TimelineStore } from '../src/store.mjs';
import { HistoryReader } from '../src/history-reader.mjs';

function setup(t) {
  const cwd = mkdtempSync(join(tmpdir(), 'dsh-history-reader-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const config = { cwd, database: join(cwd, 'timeline.sqlite'), threadId: 'facade-id', nativeSessionId: 'native-id' };
  const store = new TimelineStore(config.database);
  store.bind(config.nativeSessionId, cwd);
  for (const n of [1, 2]) {
    const threadId = config.nativeSessionId, turnId = `native-turn-${n}`;
    store.append({ method: 'turn/started', params: { threadId, turn: { id: turnId, status: 'inProgress', items: [] } } });
    store.append({ method: 'item/completed', params: { threadId, turnId, item: { id: `user-${n}`, type: 'userMessage', content: [{ type: 'text', text: `question ${n}` }] } } });
    store.append({ method: 'item/started', params: { threadId, turnId, item: { id: `tool-${n}`, type: 'mcpToolCall', status: 'inProgress' } } });
    store.append({ method: 'item/completed', params: { threadId, turnId, item: { id: `tool-${n}`, type: 'mcpToolCall', status: 'completed' } } });
    store.append({ method: 'item/completed', params: { threadId, turnId, item: { id: `agent-${n}`, type: 'agentMessage', text: `answer ${n}` } } });
    if (n === 1) store.append({ method: 'turn/completed', params: { threadId, turn: { id: turnId, status: 'completed', items: [] } } });
  }
  store.append({ method: 'dsh/contextUsage/updated', params: { used: 42, size: 1024, observedAt: 'test-time' } });
  store.close();
  return config;
}

test('offline facade history snapshots latest items without mutations or capability inflation', t => {
  const config = setup(t);
  const before = readFileSync(config.database), files = readdirSync(config.cwd);
  const reader = new HistoryReader(config);
  try {
    assert.throws(() => reader.db.exec('CREATE TABLE forbidden(x)'), /readonly|read.only/i);
    const snapshot = reader.handle('thread/read', { threadId: config.threadId, includeTurns: true }).thread;
    assert.equal(snapshot.id, config.threadId);
    assert.equal(snapshot.path, config.database);
    assert.equal(snapshot.turns[0].id, 'native-turn-1');
    assert.equal(snapshot.turns[1].status, 'interrupted');
    assert.equal(snapshot.turns[0].items.filter(i => i.id === 'tool-1').length, 1);
    assert.equal(snapshot.turns[0].items.find(i => i.id === 'tool-1').status, 'completed');
    assert.equal(snapshot.dshContext.used, 42);
    assert.equal(snapshot.billing, 'unavailable');
    assert.equal(snapshot.tokenUsage, undefined);
    assert.equal(snapshot.capabilities.revert, true); // r265: cutex reverts a stopped agent through session-ops.mjs
    assert.throws(() => reader.handle('turn/start', { threadId: config.threadId }), /read-only/);
    assert.throws(() => reader.handle('thread/read', { threadId: config.nativeSessionId }), /unknown thread/);
  } finally { reader.close(); }
  assert.deepEqual(readFileSync(config.database), before);
  // SQLite may create empty WAL/shared-lock sidecars even for a read-only WAL
  // connection. The database and schema themselves must remain byte-identical.
  assert.deepEqual(readdirSync(config.cwd).filter(name => !name.endsWith('-wal') && !name.endsWith('-shm')), files);
  if (existsSync(config.database + '-wal')) assert.equal(readFileSync(config.database + '-wal').length, 0);
  assert.throws(() => new HistoryReader({ ...config, nativeSessionId: 'wrong-native' }), /binding mismatch/);
});

test('bounded turn/item pages carry stable native IDs and reject cursors from another scope', t => {
  const config = setup(t), reader = new HistoryReader(config);
  try {
    const params = { threadId: config.threadId, sortDirection: 'desc', limit: 1, itemsView: 'summary' };
    const first = reader.handle('thread/turns/list', params);
    assert.equal(first.data[0].id, 'native-turn-2');
    assert.equal(first.data[0].items.length, 2);
    assert.ok(first.data[0].items.every(i => ['userMessage', 'agentMessage'].includes(i.type)));
    const last = reader.handle('thread/turns/list', { ...params, cursor: first.nextCursor });
    assert.equal(last.data[0].id, 'native-turn-1');
    assert.equal(last.nextCursor, null);
    assert.throws(() => reader.handle('thread/items/list', { ...params, cursor: first.nextCursor }), /invalid cursor/);
    const items = reader.handle('thread/items/list', { threadId: config.threadId, turnId: 'native-turn-1', sortDirection: 'asc' });
    assert.deepEqual(items.data.map(entry => entry.item.id), ['user-1', 'tool-1', 'agent-1']);
    assert.ok(items.data.every(entry => entry.turnId === 'native-turn-1'));
    assert.throws(() => reader.handle('thread/items/list', { threadId: config.threadId, turnId: 'missing' }), /unknown turn/);
  } finally { reader.close(); }
});

test('missing offline history never creates a database', t => {
  const config = setup(t), database = join(config.cwd, 'not-created.sqlite');
  const reader = new HistoryReader({ ...config, database });
  assert.deepEqual(reader.handle('thread/turns/list', { threadId: config.threadId }).data, []);
  const empty = reader.handle('thread/read', { threadId: config.threadId, includeTurns: true }).thread;
  assert.equal(empty.id, config.threadId);
  assert.deepEqual(empty.turns, []);
  reader.close();
  assert.equal(existsSync(database), false);
});

test('preallocated facade accepts absent native ID and validates it only after binding exists', t => {
  const config = setup(t);
  const missing = new HistoryReader({ ...config, database: join(config.cwd, 'never-started.sqlite'), nativeSessionId: null });
  assert.equal(missing.handle('thread/read', { threadId: config.threadId }).thread.id, config.threadId);
  missing.close();
  const bound = new HistoryReader({ ...config, nativeSessionId: null });
  assert.equal(bound.handle('thread/read', { threadId: config.threadId }).thread.id, config.threadId);
  assert.equal(bound.nativeSessionId, config.nativeSessionId);
  bound.close();
  assert.throws(() => new HistoryReader({ ...config, nativeSessionId: 'wrong' }), /binding mismatch/);
});

test('stdio reader implements OwnedStdioEndpoint handshake and denies execution', t => {
  const config = setup(t), file = join(config.cwd, 'reader-config.json');
  writeFileSync(file, JSON.stringify(config));
  const request = (id, method, params = {}) => JSON.stringify({ id, method, params });
  const child = spawnSync(process.execPath, [fileURLToPath(new URL('../src/history-reader.mjs', import.meta.url))], {
    env: { DSH_CUTEX_HISTORY_CONFIG: file, HOME: config.cwd, USERPROFILE: config.cwd }, encoding: 'utf8', timeout: 10000,
    input: [request(1, 'initialize'), JSON.stringify({ method: 'initialized' }), request(2, 'thread/read', { threadId: config.threadId, includeTurns: true }),
      request(3, 'turn/start', { threadId: config.threadId }), '{bad-json', request(4, 'thread/turns/list', { threadId: config.threadId })].join('\n') + '\n',
  });
  assert.equal(child.status, 0, child.stderr);
  const responses = child.stdout.trim().split('\n').map(JSON.parse);
  assert.equal(responses[0].result.readOnly, true);
  assert.equal(responses[1].result.thread.id, config.threadId);
  assert.equal(responses[2].error.code, -32601);
  assert.equal(responses[3].error.code, -32700);
  assert.equal(responses[4].result.data.length, 2);
});

test('stdio initialization of an unstarted facade returns an empty thread without creating SQLite', t => {
  const cfg = setup(t), database = join(cfg.cwd, 'new-agent.sqlite'), file = join(cfg.cwd, 'new-agent.json');
  writeFileSync(file, JSON.stringify({ database, threadId: 'preallocated-facade', cwd: cfg.cwd, nativeSessionId: null }));
  const child = spawnSync(process.execPath, [fileURLToPath(new URL('../src/history-reader.mjs', import.meta.url))], {
    env: { DSH_CUTEX_HISTORY_CONFIG: file, HOME: cfg.cwd, USERPROFILE: cfg.cwd }, encoding: 'utf8', timeout: 10000,
    input: [JSON.stringify({ id: 1, method: 'initialize' }), JSON.stringify({ method: 'initialized' }),
      JSON.stringify({ id: 2, method: 'thread/read', params: { threadId: 'preallocated-facade', includeTurns: true } })].join('\n') + '\n',
  });
  assert.equal(child.status, 0, child.stderr);
  const responses = child.stdout.trim().split('\n').map(JSON.parse);
  assert.equal(responses[1].result.thread.id, 'preallocated-facade');
  assert.deepEqual(responses[1].result.thread.turns, []);
  assert.equal(existsSync(database), false);
});
