import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, appendFile, rm, stat, utimes } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { spawnSync } from 'node:child_process';
import { zstdCompressSync, constants } from 'node:zlib';
import { loadNativeCatalog, readNativeLog, nativeEventUpdates, reconcileNativeLog, NativeLogScanCache } from '../src/reconcile.mjs';

const catalog = await loadNativeCatalog();
const sessionId = 'keyless-recovery';
const header = { version: 4, id: sessionId, createdAt: 1, isSeeded: false, delegationDepth: 0 };
const event = (seq, type, data) => ({ seq, time: seq + 2, type, data });
const assistant = (seq, text = 'committed before controller crash') => event(seq, 'assistant/message', {
  turn: 1, step: 1, message: { id: `message-${seq}`, role: 'assistant', content: [
    { type: 'reasoning', text: 'thinking' }, { type: 'text', text },
  ] },
});
const fixture = [
  event(0, 'turn/start', { turn: 1 }),
  assistant(1),
  event(2, 'tool/call', { turn: 1, step: 1, callId: 'call-1', name: 'example', arguments: '{"x":1}' }),
  event(3, 'tool/result', { turn: 1, step: 1, message: { id: 'result-1', role: 'tool', source: { kind: 'tool', callId: 'call-1' }, toolCallId: 'call-1', content: [{ type: 'text', text: 'done' }] } }),
  event(4, 'turn/end', { turn: 1, reason: 'completed' }),
];
const row = event => JSON.stringify(catalog.encodeCurrentEvent(event)) + '\n';
const headerRow = () => JSON.stringify(catalog.encodeCurrentHeader(header, 0)) + '\n';
const textLog = events => headerRow() + events.map(row).join('');
const frame = text => zstdCompressSync(text, { params: { [constants.ZSTD_c_checksumFlag]: 1 } });

async function setup(t) {
  const dir = await mkdtemp(join(tmpdir(), 'dsh-reconcile-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return { dir, logPath: join(dir, 'session.v4.jsonl'), sessionId, catalog };
}

// Example integration contract only; production store.mjs is deliberately untouched.
class DurableSink {
  constructor(path) {
    this.db = new DatabaseSync(path);
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS cursors (session TEXT PRIMARY KEY, body TEXT);
      CREATE TABLE IF NOT EXISTS native (id TEXT PRIMARY KEY, body TEXT);
      CREATE TABLE IF NOT EXISTS updates (id TEXT, ordinal INTEGER, body TEXT, PRIMARY KEY(id, ordinal));`);
  }
  nativeCursor(id) { const row = this.db.prepare('SELECT body FROM cursors WHERE session=?').get(id); return row ? JSON.parse(row.body) : null; }
  commitNative(record) {
    this.db.exec('BEGIN IMMEDIATE');
    try {
      assert.deepEqual(this.nativeCursor(record.sessionId), record.previousCursor, 'stale native cursor');
      this.db.prepare('INSERT INTO native VALUES (?, ?)').run(record.nativeId, JSON.stringify(record.event));
      record.updates.forEach((update, index) => this.db.prepare('INSERT INTO updates VALUES (?, ?, ?)').run(record.nativeId, index, JSON.stringify(update)));
      if (this.hardCrash === 'before' && record.nativeSeq === 1) process.exit(86);
      if (this.failBefore === record.nativeSeq) throw new Error('crash before transaction commit');
      this.db.prepare('INSERT OR REPLACE INTO cursors VALUES (?, ?)').run(record.sessionId, JSON.stringify(record.cursor));
      this.db.exec('COMMIT');
    } catch (error) { this.db.exec('ROLLBACK'); throw error; }
    if (this.hardCrash === 'after' && record.nativeSeq === 1) process.exit(86);
    if (this.failAfter === record.nativeSeq) throw new Error('crash after transaction commit before acknowledgement');
  }
  updates() { return this.db.prepare('SELECT body FROM updates ORDER BY rowid').all().map(row => JSON.parse(row.body)); }
  close() { this.db.close(); }
}

for (const point of ['before', 'after']) {
  test(`controller process death ${point} SQLite commit recovers without duplicate output`, async t => {
    const options = await setup(t);
    await writeFile(options.logPath, textLog(fixture));
    const database = join(options.dir, 'projection.sqlite');
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
      import assert from 'node:assert/strict';
      import { DatabaseSync } from 'node:sqlite';
      import { reconcileNativeLog } from ${JSON.stringify(new URL('../src/reconcile.mjs', import.meta.url).href)};
      ${DurableSink.toString()}
      const sink = new DurableSink(process.argv[1]);
      sink.hardCrash = process.argv[3];
      await reconcileNativeLog({ store: sink, logPath: process.argv[2], sessionId: ${JSON.stringify(sessionId)} });
    `, database, options.logPath, point], { encoding: 'utf8', timeout: 15000 });
    assert.equal(child.status, 86, child.stderr);
    const sink = new DurableSink(database);
    try {
      assert.equal(sink.nativeCursor(sessionId).nativeSeq, point === 'before' ? 0 : 1);
      await reconcileNativeLog({ ...options, store: sink });
      assert.equal(sink.updates().length, 4);
      assert.equal((await reconcileNativeLog({ ...options, store: sink })).committed, 0);
    } finally { sink.close(); }
  });
}

test('pinned released codec reads actual v4 event envelopes and preserves native seq', async t => {
  const options = await setup(t);
  await writeFile(options.logPath, textLog(fixture));
  const snapshot = await readNativeLog(options);
  assert.deepEqual(snapshot.events.map(record => record.event), fixture);
  assert.equal(snapshot.events[0].nativeSeq, 0);
  assert.equal(snapshot.events[1].nativeId, JSON.stringify([sessionId, 1]));
  assert.equal(snapshot.tornBytes, 0);
});

for (const point of ['failBefore', 'failAfter']) {
  test(`restart replays DSH-committed gap exactly once (${point})`, async t => {
    const options = await setup(t);
    await writeFile(options.logPath, textLog(fixture));
    const database = join(options.dir, 'projection.sqlite');
    let sink = new DurableSink(database);
    sink[point] = 1;
    await assert.rejects(reconcileNativeLog({ ...options, store: sink }), /crash/);
    sink.close();
    // A fresh controller reopens only durable SQLite state; no in-memory IDs.
    sink = new DurableSink(database);
    try {
      const result = await reconcileNativeLog({ ...options, store: sink });
      assert.equal(result.committed, point === 'failBefore' ? 4 : 3);
      assert.deepEqual(sink.updates().map(update => update.sessionUpdate), [
        'agent_thought_chunk', 'agent_message_chunk', 'tool_call', 'tool_call_update',
      ]);
      assert.equal(sink.updates()[1].content.text, 'committed before controller crash');
      assert.equal((await reconcileNativeLog({ ...options, store: sink })).committed, 0);
      assert.equal(sink.nativeCursor(sessionId).nativeSeq, 4);
      await appendFile(options.logPath, row(assistant(5, 'next turn output')));
      assert.equal((await reconcileNativeLog({ ...options, store: sink })).committed, 1);
      assert.equal(sink.updates().length, 6);
    } finally { sink.close(); }
  });
}

test('incomplete JSONL record is retried when the newline arrives', async t => {
  const options = await setup(t);
  const final = row(assistant(1));
  await writeFile(options.logPath, textLog(fixture.slice(0, 1)) + final.slice(0, -1));
  assert.equal((await readNativeLog(options)).events.length, 1);
  await appendFile(options.logPath, '\n');
  assert.equal((await readNativeLog(options)).events.length, 2);
});

test('concatenated checksummed zstd batches: torn final frame waits; corrupt checksum rejects', async t => {
  const options = await setup(t);
  options.logPath += '.zstd';
  const prefix = Buffer.concat([frame(headerRow()), frame(fixture.slice(0, 2).map(row).join(''))]);
  const tail = frame(fixture.slice(2).map(row).join(''));
  await writeFile(options.logPath, Buffer.concat([prefix, tail.subarray(0, -2)]));
  const partial = await readNativeLog(options);
  assert.equal(partial.events.length, 2);
  assert.ok(partial.tornBytes > 0);
  await appendFile(options.logPath, tail.subarray(-2));
  assert.equal((await readNativeLog(options)).events.length, 5);
  const corrupt = Buffer.from(tail);
  corrupt[corrupt.length - 1] ^= 1;
  await writeFile(options.logPath, Buffer.concat([prefix, corrupt]));
  await assert.rejects(readNativeLog(options));
});

test('rejects identity, format changes, committed garbage and native sequence gaps', async t => {
  const options = await setup(t);
  await writeFile(options.logPath, textLog(fixture));
  await assert.rejects(readNativeLog({ ...options, sessionId: 'another' }), /identity/);
  await writeFile(options.logPath, textLog(fixture).replace('"version":4', '"version":5'));
  await assert.rejects(readNativeLog(options), /format v4/);
  await writeFile(options.logPath, textLog(fixture) + 'broken\n');
  await assert.rejects(readNativeLog(options));
  await writeFile(options.logPath, textLog([fixture[0], fixture[2]]));
  await assert.rejects(readNativeLog(options), /seq gap|Noncontiguous/);
});

test('rewritten or truncated committed prefix is reported, never re-projected, and never throws', async t => {
  const options = await setup(t);
  const sink = new DurableSink(join(options.dir, 'projection.sqlite'));
  try {
    await writeFile(options.logPath, textLog(fixture));
    await reconcileNativeLog({ ...options, store: sink });
    await writeFile(options.logPath, textLog([fixture[0], assistant(1, 'rewritten'), ...fixture.slice(2)]));
    const diverged = await reconcileNativeLog({ ...options, store: sink });
    assert.equal(diverged.status.state, 'diverged');
    assert.equal(diverged.committed, 0);
    await writeFile(options.logPath, textLog(fixture.slice(0, 2)));
    const behind = await reconcileNativeLog({ ...options, store: sink });
    assert.equal(behind.status.state, 'behind');
    assert.equal(sink.updates().length, 4);
    assert.equal(sink.nativeCursor(sessionId).nativeSeq, 4);
    // The original bytes come back (e.g. restored backup): projection continues.
    await writeFile(options.logPath, textLog([...fixture, assistant(5, 'after restore')]));
    const restored = await reconcileNativeLog({ ...options, store: sink });
    assert.equal(restored.status.state, 'ok');
    assert.equal(restored.committed, 1);
  } finally { sink.close(); }
});

test('ACP mapping preserves block order, opaque malformed arguments, tool failure and never fails on attachments', async () => {
  assert.equal((await nativeEventUpdates({ ...fixture[2], data: { ...fixture[2].data, arguments: 'invalid JSON' } }))[0].rawInput, 'invalid JSON');
  assert.equal((await nativeEventUpdates({ ...fixture[3], data: { ...fixture[3].data, message: { ...fixture[3].data.message, isError: true } } }))[0].status, 'failed');
  assert.deepEqual(await nativeEventUpdates(event(0, 'assistant/attempt', { stream: ['transient'] })), []);
  assert.deepEqual(await nativeEventUpdates(event(0, 'turn/start', { turn: 1 })), []);
  const message = assistant(1);
  message.data.usage = { inputTokens: 9, outputTokens: 3 };
  assert.equal((await nativeEventUpdates(message)).length, 2, 'billing facts cannot reconstruct occupancy');
  const attachment = { attachmentId: 'image-ref', mediaType: 'image/png', bytes: 2048, width: 4, height: 3 };
  message.data.message.content.push({ type: 'image', attachment });
  const placeholder = (await nativeEventUpdates(message)).at(-1).content;
  assert.equal(placeholder.type, 'text');
  assert.equal(placeholder.text, '[Image: image/png, 4x3, 2.0 KB]');
  assert.deepEqual(placeholder._meta.dshAttachment, { kind: 'image', ...attachment });
  const image = { type: 'image', data: 'YQ==', mimeType: 'image/png' };
  assert.deepEqual((await nativeEventUpdates(message, { resolveImage: async () => image })).at(-1).content, image);
  assert.equal((await nativeEventUpdates(message, { resolveImage: async () => { throw new Error('gone'); } })).at(-1).content.type, 'text', 'resolver failure falls back to a placeholder');
  assert.equal((await nativeEventUpdates(message, { resolveImage: async () => ({ type: 'image' }) })).at(-1).content.type, 'text', 'invalid resolver output falls back');
  const odd = await nativeEventUpdates(event(3, 'tool/result', { turn: 1, step: 1, message: { id: 'r', role: 'tool', toolCallId: 'call-1',
    content: ['not-a-block', { type: 'file', attachment: { name: 'a.pdf', bytes: 10 } }, { type: 'future-kind' }, { type: 'text', text: 'ok' }] } }));
  assert.deepEqual(odd[0].content.map(block => block.content.text), ['[File a.pdf: 10 B]', '[Unsupported content: future-kind]', 'ok']);
  assert.deepEqual(await nativeEventUpdates(event(3, 'tool/result', { message: null })), []);
  assert.deepEqual(await nativeEventUpdates(event(3, 'assistant/message', {})), []);
});

test('per-call budgets bound work, not log size; the next call continues where the last stopped', async t => {
  const options = await setup(t);
  const many = [event(0, 'turn/start', { turn: 1 }), ...Array.from({ length: 49 }, (_, index) => assistant(index + 1, `m${index}`))];
  await writeFile(options.logPath, textLog(many));
  const sink = new DurableSink(join(options.dir, 'bounded.sqlite'));
  const cache = new NativeLogScanCache();
  try {
    const limits = { maxEventsPerScan: 10, commitBatchEvents: 3 };
    const first = await reconcileNativeLog({ ...options, store: sink, cache, limits });
    assert.equal(first.committed, 10);
    assert.equal(first.more, true);
    assert.equal(first.status.state, 'catchingUp');
    let calls = 1, total = first.committed, result = first;
    while (result.more) { result = await reconcileNativeLog({ ...options, store: sink, cache, limits }); total += result.committed; calls++; }
    assert.equal(total, many.length);
    assert.ok(calls >= 5 && calls <= 6, String(calls));
    assert.equal(result.status.state, 'ok');
    assert.equal(sink.nativeCursor(sessionId).nativeSeq, many.length - 1);
    // A fresh controller without cache resumes from the durable byte offset as well.
    const resumed = await reconcileNativeLog({ ...options, store: sink, limits: { fullVerifyBelowBytes: 0 } });
    assert.equal(resumed.committed, 0);
    assert.equal(resumed.status.state, 'ok');
    for (const invalid of [{ maxEventsPerScan: Infinity }, { maxEventsPerScan: 0 }, { maxRecordBytes: -1 }, { typo: 4 }, { maxFileBytes: 4 }]) {
      await assert.rejects(reconcileNativeLog({ ...options, store: sink, limits: invalid }), /Invalid native log limit/);
    }
  } finally { sink.close(); }
});

test('Zstandard bomb and cumulative small frames obey total decoded-byte budget', async t => {
  const options = await setup(t);
  options.logPath += '.zstd';
  const bomb = frame(row(assistant(1, 'A'.repeat(128 * 1024))));
  assert.ok(bomb.length < 1024, 'fixture actually compresses far below its expanded size');
  await writeFile(options.logPath, Buffer.concat([frame(headerRow()), frame(row(fixture[0])), bomb]));
  await assert.rejects(readNativeLog({ ...options, limits: { maxDecompressedBytes: 1024 } }),
    error => error.code === 'DSH_NATIVE_LOG_LIMIT' && error.limit === 'maxDecompressedBytes');
  await writeFile(options.logPath, Buffer.concat([frame(headerRow()), ...fixture.map(item => frame(row(item)))]));
  await assert.rejects(readNativeLog({ ...options, limits: { maxDecompressedBytes: Buffer.byteLength(textLog(fixture)) - 1 } }),
    error => error.code === 'DSH_NATIVE_LOG_LIMIT' && error.limit === 'maxDecompressedBytes');
  assert.equal((await readNativeLog({ ...options, limits: { maxDecompressedBytes: Buffer.byteLength(textLog(fixture)) } })).events.length, fixture.length);
});

function countReads(t) {
  // Counts positioned reads through FileHandle.read on handles opened by reconcile.
  let reads = 0;
  return { get reads() { return reads; }, async wrap(fn) {
    const probe = await (await import('node:fs/promises')).open(process.execPath);
    const FileHandle = probe.constructor;
    await probe.close();
    const original = FileHandle.prototype.read;
    FileHandle.prototype.read = function (...args) { reads++; return original.apply(this, args); };
    try { return await fn(); } finally { FileHandle.prototype.read = original; }
  } };
}

test('unchanged stat skips reading, appends read only the tail, new controller cache revalidates', async t => {
  const options = await setup(t);
  await writeFile(options.logPath, textLog(fixture));
  const sink = new DurableSink(join(options.dir, 'cached.sqlite'));
  const cache = new NativeLogScanCache();
  const counter = countReads(t);
  const scan = extra => reconcileNativeLog({ ...options, store: sink, cache, ...extra });
  try {
    assert.equal((await scan()).unchanged, false);
    await counter.wrap(async () => { for (let i = 0; i < 20; i++) assert.equal((await scan()).unchanged, true); });
    assert.equal(counter.reads, 0, 'polling an unchanged log performs no reads');
    await appendFile(options.logPath, row(assistant(5, 'appended')));
    const appended = await counter.wrap(() => scan());
    assert.equal(appended.committed, 1);
    assert.ok(counter.reads <= 2, `append read only the new tail (${counter.reads} reads)`);
    assert.equal((await scan({ limits: { maxEventsPerScan: 7 } })).unchanged, false, 'budget change invalidates the cache entry');
    assert.equal((await scan({ cache: new NativeLogScanCache() })).unchanged, false);
    const reopened = new DurableSink(join(options.dir, 'cached.sqlite'));
    try {
      const fresh = await scan({ store: reopened });
      assert.equal(fresh.unchanged, false, 'a new store/controller is fresh even if a caller reuses its old cache');
      assert.equal(fresh.committed, 0);
    } finally { reopened.close(); }
  } finally { sink.close(); }
});

test('same-size prefix rewrite with restored mtime is detected live and after restart; truncation is reported', async t => {
  const options = await setup(t);
  const original = textLog(fixture);
  await writeFile(options.logPath, original);
  const database = join(options.dir, 'rewrite.sqlite');
  let sink = new DurableSink(database);
  const cache = new NativeLogScanCache();
  await reconcileNativeLog({ ...options, store: sink, cache });
  const cursor = sink.nativeCursor(sessionId);
  const metadata = await stat(options.logPath);
  const rewritten = original.replace('committed', 'tampered!');
  assert.equal(Buffer.byteLength(rewritten), Buffer.byteLength(original));
  await writeFile(options.logPath, rewritten);
  await utimes(options.logPath, metadata.atime, metadata.mtime);
  assert.equal((await reconcileNativeLog({ ...options, store: sink, cache })).status.state, 'diverged');
  sink.close();
  sink = new DurableSink(database);
  try {
    assert.equal((await reconcileNativeLog({ ...options, store: sink, cache: new NativeLogScanCache() })).status.state, 'diverged');
    await writeFile(options.logPath, textLog(fixture.slice(0, 2)));
    assert.equal((await reconcileNativeLog({ ...options, store: sink, cache })).status.state, 'behind');
    assert.deepEqual(sink.nativeCursor(sessionId), cursor);
    assert.equal(sink.updates().length, 4);
  } finally { sink.close(); }
});


test('failed commit is never cached and unchanged log replays its commit gap exactly once', async t => {
  const options = await setup(t);
  await writeFile(options.logPath, textLog(fixture));
  const sink = new DurableSink(join(options.dir, 'gap-cache.sqlite'));
  const cache = new NativeLogScanCache();
  try {
    sink.failAfter = 1;
    await assert.rejects(reconcileNativeLog({ ...options, store: sink, cache }), /crash/);
    sink.failAfter = undefined;
    const recovered = await reconcileNativeLog({ ...options, store: sink, cache });
    assert.equal(recovered.unchanged, false);
    assert.equal(recovered.committed, 3);
    assert.equal(sink.updates().length, 4);
    assert.equal((await reconcileNativeLog({ ...options, store: sink, cache })).unchanged, true);
  } finally { sink.close(); }
});

test('cached torn compressed tail stays pending; checksum failure stops at the last good frame, then recovers', async t => {
  const options = await setup(t);
  options.logPath += '.zstd';
  const prefix = Buffer.concat([frame(headerRow()), frame(row(fixture[0]))]);
  const final = frame(fixture.slice(1).map(row).join(''));
  await writeFile(options.logPath, Buffer.concat([prefix, final.subarray(0, -2)]));
  const sink = new DurableSink(join(options.dir, 'tail-cache.sqlite'));
  const cache = new NativeLogScanCache();
  try {
    const partial = await reconcileNativeLog({ ...options, store: sink, cache });
    assert.equal(partial.committed, 1);
    assert.ok(partial.tornBytes > 0);
    assert.equal((await reconcileNativeLog({ ...options, store: sink, cache })).unchanged, true);
    const corrupt = Buffer.from(final);
    corrupt[corrupt.length - 1] ^= 1;
    await writeFile(options.logPath, Buffer.concat([prefix, corrupt]));
    const damaged = await reconcileNativeLog({ ...options, store: sink, cache });
    assert.equal(damaged.status.state, 'damaged');
    assert.equal(damaged.status.offset, prefix.length);
    assert.match(damaged.status.reason, /zstd/);
    assert.equal(sink.nativeCursor(sessionId).nativeSeq, 0);
    // DSH truncates the damaged tail on its next open and appends again.
    await writeFile(options.logPath, Buffer.concat([prefix, final]));
    const repaired = await reconcileNativeLog({ ...options, store: sink, cache });
    assert.equal(repaired.committed, 4);
    assert.equal(repaired.status.state, 'ok');
    assert.equal(sink.updates().length, 4);
  } finally { sink.close(); }
});

