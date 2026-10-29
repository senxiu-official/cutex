// Streamed text and reasoning never carry a known secret in pieces: a key
// split over deltas (every split position) is held back until it is whole and
// then redacted; held-back tails go out at completion, abort and error.
// Synthetic keys only; no process, socket or network.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { DshRuntime } from '../src/runtime.mjs';
import { secretRedactor, streamRedactor } from '../src/history-reader.mjs';
import { TimelineStore } from '../src/store.mjs';

const KEY = 'sk-SYNTHETIC-stream-0123456789abcdef';
const OTHER = 'tok-synthetic-second-secret-42';
const wireRedact = secretRedactor([KEY, OTHER]);

/** A runtime with only what stream() uses; events pass a server-equivalent per-message redactor. */
function liveRuntime() {
  const runtime = Object.create(DshRuntime.prototype);
  const events = [];
  Object.assign(runtime, { sessionId: 'native', turnId: 'native:turn:1', live: new Map(), aliases: new Map(), held: [], items: new Map(),
    streamFilter: streamRedactor([KEY, OTHER]), emit: event => events.push(wireRedact(event)), publishNative() {} });
  return { runtime, events };
}

function run(parts, { kind = 'text', end = { kind: 'committed', eventType: 'assistant/message', seq: 7 }, abandon = false } = {}) {
  const { runtime, events } = liveRuntime();
  const frame = extra => runtime.stream({ sessionId: 'native', attemptId: 'a1', ...extra });
  frame({ type: 'start', turn: 1 });
  for (const text of parts) frame({ type: 'delta', kind, text });
  if (abandon) runtime.releaseHeld(true);
  else frame({ type: 'end', outcome: end });
  const method = kind === 'text' ? 'item/agentMessage/delta' : 'item/reasoning/textDelta';
  const deltas = events.filter(event => event.method === method).map(event => event.params.delta);
  return { deltas, joined: deltas.join(''), events };
}

// Any 6 consecutive characters of the key.
const fragment = text => [...Array(KEY.length - 5).keys()].some(i => text.includes(KEY.slice(i, i + 6)));

const splits = text => {
  const result = [];
  for (let i = 1; i < text.length; i++) result.push([text.slice(0, i), text.slice(i)]);
  return result;
};

test('a key split at every position is never emitted in pieces (text and reasoning)', () => {
  const full = `before ${KEY} after`;
  for (const kind of ['text', 'reasoning']) {
    for (const parts of splits(full)) {
      const { deltas, joined } = run(parts, { kind });
      assert.equal(joined, `before [REDACTED] after`, JSON.stringify(parts));
      for (const delta of deltas) assert.ok(!fragment(delta), `no key fragment in a delta (split at ${parts[0].length})`);
    }
  }
});

test('three-way splits, a key at the very start or end, and two secrets', () => {
  for (let i = 1; i < KEY.length - 1; i++) {
    for (let j = i + 1; j < KEY.length; j++) {
      const parts = [KEY.slice(0, i), KEY.slice(i, j), KEY.slice(j)];
      assert.equal(run(parts).joined, '[REDACTED]', JSON.stringify(parts));
    }
  }
  const both = `${OTHER}${KEY} ${OTHER}`;
  for (const parts of splits(both)) assert.equal(run(parts).joined, '[REDACTED][REDACTED] [REDACTED]', JSON.stringify(parts));
  // One character per delta.
  assert.equal(run([...`x${KEY}y`]).joined, 'x[REDACTED]y');
});

test('held-back tails go out at an abandoned or failed attempt, and text that only looks like a key prefix is not lost', () => {
  for (const end of [{ kind: 'failed' }, { kind: 'abandoned' }]) {
    for (const parts of splits(`tail ${KEY}`)) {
      const { joined, events } = run(parts, { end });
      assert.equal(joined, 'tail [REDACTED]');
      const completed = events.filter(event => event.method === 'item/completed');
      assert.equal(completed.length, 1);
      assert.equal(completed[0].params.item.text, 'tail [REDACTED]');
    }
  }
  // Abort: the attempt never ended (runtime gave up waiting); the tail is flushed before it is dropped.
  for (const parts of splits(`abort ${KEY}`)) assert.equal(run(parts, { abandon: true }).joined, 'abort [REDACTED]');
  // A stream that ends inside something shaped like the key's start keeps every character.
  const prefix = KEY.slice(0, 10);
  assert.equal(run(['see ', prefix]).joined, `see ${prefix}`);
  assert.equal(run(['see ', prefix], { end: { kind: 'failed' } }).joined, `see ${prefix}`);
  // Ordinary text is not delayed.
  assert.deepEqual(run(['hello ', 'world']).deltas, ['hello ', 'world']);
});

test('stream filter: unit behaviour', () => {
  const filter = streamRedactor([KEY])();
  assert.equal(filter.push('abc sk-SYN'), 'abc ');
  assert.equal(filter.push('THETIC'), '');
  assert.equal(filter.push(KEY.slice('sk-SYNTHETIC'.length) + ' done'), '[REDACTED] done');
  assert.equal(filter.flush(), '');
  const none = streamRedactor([])();
  assert.equal(none.push('sk-'), 'sk-');
});

test('ACP path: recorded deltas, the completed item and offline history never carry the split key', t => {
  const dir = mkdtempSync(join(tmpdir(), 'dsh-stream-acp-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const database = join(dir, 'timeline.sqlite');
  for (const [kind, type] of [['agent_message_chunk', 'agentMessage'], ['agent_thought_chunk', 'reasoning']]) {
    for (const parts of splits(`x ${KEY} y`).filter((_, i) => i % 5 === 0)) {
      const store = new TimelineStore(database);
      const runtime = Object.create(DshRuntime.prototype);
      const emitted = [];
      Object.assign(runtime, { sessionId: 'native', turnId: 'turn-1', items: new Map(), acpFilters: new Map(), store, deliveries: { observe() {} },
        redact: secretRedactor([KEY]), streamFilter: streamRedactor([KEY]), emit: event => emitted.push(event) });
      store.bind('native', dir);
      runtime.record('turn/started', { threadId: 'native', turn: { id: 'turn-1', status: 'inProgress', items: [] } });
      for (const text of parts) runtime.update({ sessionId: 'native', update: { sessionUpdate: kind, messageId: 'm1', content: { type: 'text', text } } });
      runtime.flushAcpDeltas('turn-1');
      for (const item of runtime.items.values()) runtime.record('item/completed', { threadId: 'native', turnId: 'turn-1', item });
      const stored = store.replay();
      store.close();
      const deltas = emitted.filter(event => event.method.endsWith('elta')).map(event => event.params.delta).join('');
      assert.equal(deltas, 'x [REDACTED] y', `${type} ${JSON.stringify(parts)}`);
      const raw = JSON.stringify(stored);
      assert.ok(!raw.includes(KEY), 'stored timeline holds no key');
      const item = emitted.find(event => event.method === 'item/completed').params.item;
      assert.equal(type === 'agentMessage' ? item.text : item.content.join(''), 'x [REDACTED] y');
      rmSync(database, { force: true }); rmSync(`${database}-wal`, { force: true }); rmSync(`${database}-shm`, { force: true });
    }
  }
});

test('offline history reader redacts a key that older stored deltas split (DSH_CUTEX_REDACT)', t => {
  const cwd = mkdtempSync(join(tmpdir(), 'dsh-stream-reader-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const database = join(cwd, 'timeline.sqlite');
  const store = new TimelineStore(database);
  store.bind('native', cwd);
  const params = { threadId: 'native', turnId: 't1' };
  store.append({ method: 'turn/started', params: { threadId: 'native', turn: { id: 't1', status: 'inProgress', items: [] } } });
  store.append({ method: 'item/started', params: { ...params, item: { id: 'm', type: 'agentMessage', text: '' } } });
  // Written before stream-safe redaction: each piece alone passed the per-event redactor.
  store.append({ method: 'item/agentMessage/delta', params: { ...params, itemId: 'm', delta: `old ${KEY.slice(0, 15)}` } });
  store.append({ method: 'item/agentMessage/delta', params: { ...params, itemId: 'm', delta: `${KEY.slice(15)} end` } });
  store.close();
  const file = join(cwd, 'reader.json');
  writeFileSync(file, JSON.stringify({ database, threadId: 'facade', nativeSessionId: 'native', cwd }));
  const read = env => {
    const child = spawnSync(process.execPath, [fileURLToPath(new URL('../src/history-reader.mjs', import.meta.url))], {
      env: { DSH_CUTEX_HISTORY_CONFIG: file, HOME: cwd, ...env }, encoding: 'utf8', timeout: 10000,
      input: [JSON.stringify({ id: 1, method: 'initialize' }), JSON.stringify({ method: 'initialized' }),
        JSON.stringify({ id: 2, method: 'thread/read', params: { threadId: 'facade', includeTurns: true } }),
        JSON.stringify({ id: 3, method: 'thread/items/list', params: { threadId: 'facade', itemsView: 'full' } })].join('\n') + '\n',
    });
    assert.equal(child.status, 0, child.stderr);
    return child.stdout;
  };
  assert.ok(read({}).includes(KEY), 'fixture: the projection joins the stored pieces');
  const redacted = read({ DSH_CUTEX_REDACT: JSON.stringify(['sk-SYNTHETIC-earlier-version', KEY]) });
  assert.ok(!redacted.includes(KEY));
  assert.match(redacted, /old \[REDACTED\] end/);
});
