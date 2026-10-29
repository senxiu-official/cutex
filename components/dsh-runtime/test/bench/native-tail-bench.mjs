// Synthetic large-log benchmark for the incremental native-log reader.
// Not part of `npm test` (it writes ~200 MB). Run with Node >= 24.9:
//   node test/bench/native-tail-bench.mjs [--events 1000000] [--dir <scratch>] [--sqlite] [--keep]
// Prints one JSON report and exits non-zero if a bound is violated.
import { mkdtemp, rm, stat, appendFile, open } from 'node:fs/promises';
import { createWriteStream } from 'node:fs';
import { once } from 'node:events';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { loadNativeCatalog, reconcileNativeLog, readNativeLog, NativeLogScanCache, NATIVE_TAIL_LIMITS } from '../../src/reconcile.mjs';
import { TimelineStore } from '../../src/store.mjs';
import { HistoryReader } from '../../src/history-reader.mjs';

const args = process.argv.slice(2);
const option = (name, fallback) => { const index = args.indexOf(`--${name}`); return index < 0 ? fallback : args[index + 1]; };
const targetEvents = Number(option('events', 1000000));
const dir = option('dir', null) ?? await mkdtemp(join(tmpdir(), 'dsh-tail-bench-'));
const keep = args.includes('--keep');
const catalog = await loadNativeCatalog();
const sessionId = 'bench-session';
const logPath = join(dir, 'session.v4.jsonl');
const event = (seq, type, data) => ({ seq, time: 1700000000000 + seq, type, data });
const filler = 'lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ';
const turnEvents = (seq, n) => [
  event(seq, 'turn/start', { turn: n }),
  event(seq + 1, 'user/message', { id: `user-${n}`, role: 'user', source: { kind: 'user' }, content: [{ type: 'text', text: `question ${n} ${filler}` }] }),
  event(seq + 2, 'tool/call', { turn: n, step: 1, callId: `call-${n}`, name: 'bash', arguments: JSON.stringify({ command: `echo ${n}` }) }),
  event(seq + 3, 'tool/result', { turn: n, step: 1, message: { id: `result-${n}`, role: 'tool', source: { kind: 'tool', callId: `call-${n}` }, toolCallId: `call-${n}`,
    content: n % 50 === 0 ? [{ type: 'image', attachment: { attachmentId: `img-${n}`, mediaType: 'image/png', bytes: 1234, width: 10, height: 10 } }] : [{ type: 'text', text: `${n}\n${filler}` }] } }),
  event(seq + 4, 'assistant/message', { turn: n, step: 2, message: { id: `assistant-${n}`, role: 'assistant', content: [{ type: 'reasoning', text: 'thinking' }, { type: 'text', text: `answer ${n} ${filler}` }] } }),
  event(seq + 5, 'turn/end', { turn: n, reason: { kind: 'completed' } }),
];
const rows = events => events.map(item => JSON.stringify(catalog.encodeCurrentEvent(item)) + '\n').join('');

let peakRss = 0, peakHeap = 0;
const sample = () => { const usage = process.memoryUsage(); peakRss = Math.max(peakRss, usage.rss); peakHeap = Math.max(peakHeap, usage.heapUsed); };
const mib = bytes => Math.round(bytes / 1024 / 1024 * 10) / 10;

/** Minimal durable-cursor sink: measures the reader, not SQLite projection. */
class CursorSink {
  cursor = null; committed = 0;
  nativeCursor() { return this.cursor; }
  commitNativeBatch(records) {
    for (const record of records) {
      if (JSON.stringify(record.previousCursor) !== JSON.stringify(this.cursor)) throw new Error('stale CAS');
      this.cursor = record.cursor;
    }
    this.committed += records.length;
  }
}

async function generate() {
  const out = createWriteStream(logPath);
  out.write(JSON.stringify(catalog.encodeCurrentHeader({ version: 4, id: sessionId, createdAt: 1, isSeeded: false, delegationDepth: 0 }, 0)) + '\n');
  let seq = 0, turn = 0;
  while (seq < targetEvents) {
    let chunk = '';
    for (let i = 0; i < 500 && seq < targetEvents; i++) { const events = turnEvents(seq, ++turn); seq += events.length; chunk += rows(events); }
    if (!out.write(chunk)) await once(out, 'drain');
  }
  out.end();
  await once(out, 'finish');
  return { events: seq, turns: turn };
}

async function passes(store, cache, limits) {
  const times = [];
  let result, calls = 0, committed = 0;
  do {
    const start = performance.now();
    result = await reconcileNativeLog({ logPath, sessionId, catalog, store, cache, limits });
    times.push(performance.now() - start);
    committed += result.committed;
    calls++;
    sample();
  } while (result.more);
  return { calls, committed, maxCallMs: Math.round(Math.max(...times)), totalMs: Math.round(times.reduce((a, b) => a + b, 0)), status: result.status.state };
}

const report = { node: process.version, limits: NATIVE_TAIL_LIMITS, dir };
const violations = [];
try {
  const generateStart = performance.now();
  const generated = await generate();
  report.log = { ...generated, bytes: (await stat(logPath)).size, mib: mib((await stat(logPath)).size), generateMs: Math.round(performance.now() - generateStart) };
  global.gc?.();
  sample();
  const baseline = process.memoryUsage().rss;
  report.baselineRssMiB = mib(baseline);

  // Old r264 behaviour for reference: the whole-prefix reader refuses this log outright.
  try { await readNativeLog({ logPath, sessionId, catalog }); report.wholePrefixReader = 'accepted'; }
  catch (error) { report.wholePrefixReader = `refused: ${error.code ?? error.message}`; }

  // 1. Cold catch-up of the whole log (reader only), bounded per call.
  const sink = new CursorSink();
  const cache = new NativeLogScanCache();
  peakRss = 0; peakHeap = 0;
  report.catchUp = { ...await passes(sink, cache), peakRssMiB: mib(peakRss), peakHeapMiB: mib(peakHeap) };
  if (report.catchUp.committed !== generated.events) violations.push('catch-up did not commit every event');
  if (report.catchUp.maxCallMs > NATIVE_TAIL_LIMITS.maxScanMs * 3) violations.push('a catch-up call exceeded 3x maxScanMs');
  if (peakRss - baseline > 256 * 1024 * 1024) violations.push('catch-up RSS grew by more than 256 MiB');

  // 2. Steady-state polls on the huge log.
  const timed = async fn => { const start = performance.now(); const value = await fn(); return { ms: Math.round((performance.now() - start) * 1000) / 1000, value }; };
  const unchanged = [];
  for (let i = 0; i < 100; i++) unchanged.push((await timed(() => reconcileNativeLog({ logPath, sessionId, catalog, store: sink, cache }))).ms);
  unchanged.sort((a, b) => a - b);
  report.unchangedPollMs = { p50: unchanged[50], p99: unchanged[98] };
  let readBytes = 0;
  const probe = await open(logPath);
  const handleClass = probe.constructor;
  await probe.close();
  const read = handleClass.prototype.read;
  handleClass.prototype.read = async function (...input) { const result = await read.apply(this, input); readBytes += result.bytesRead; return result; };
  try {
    let seq = generated.events, turn = generated.turns;
    const appendTimes = [];
    for (let i = 0; i < 20; i++) {
      const events = turnEvents(seq, ++turn); seq += events.length;
      await appendFile(logPath, rows(events));
      appendTimes.push((await timed(() => reconcileNativeLog({ logPath, sessionId, catalog, store: sink, cache }))).ms);
    }
    appendTimes.sort((a, b) => a - b);
    report.appendPoll = { p50Ms: appendTimes[10], maxMs: appendTimes.at(-1), bytesReadPerPoll: Math.round(readBytes / 20) };
    if (readBytes / 20 > 4 * NATIVE_TAIL_LIMITS.readChunkBytes) violations.push('append poll read more than 4 chunks');

    // 3. Controller restart: new cache, durable cursor with byte offset.
    readBytes = 0;
    const restart = await timed(() => reconcileNativeLog({ logPath, sessionId, catalog, store: sink, cache: new NativeLogScanCache() }));
    report.restartResume = { ms: restart.ms, bytesRead: readBytes, committed: restart.value.committed, status: restart.value.status.state };
    if (readBytes > 4 * NATIVE_TAIL_LIMITS.readChunkBytes) violations.push('restart read more than 4 chunks');
  } finally { handleClass.prototype.read = read; }

  // 4. r264 cursor (no byte offset): one bounded streaming verification, then tailing.
  const legacy = new CursorSink();
  legacy.cursor = { version: 1, sessionId, nativeSeq: sink.cursor.nativeSeq, hash: sink.cursor.hash };
  peakRss = 0; peakHeap = 0;
  report.legacyVerify = { ...await passes(legacy, new NativeLogScanCache()), peakRssMiB: mib(peakRss) };
  if (legacy.cursor.log?.offset === undefined && report.legacyVerify.committed === 0) {
    // Upgraded on the next appended event; the verification itself must end in `ok`.
  }
  if (report.legacyVerify.status !== 'ok') violations.push('legacy verification did not end ok');

  // 5. Optional: project everything into the real SQLite TimelineStore.
  if (args.includes('--sqlite')) {
    const store = new TimelineStore(join(dir, 'timeline.sqlite'));
    peakRss = 0;
    try { report.sqliteIngest = { ...await passes(store, new NativeLogScanCache()), peakRssMiB: mib(peakRss), dbMiB: mib((await stat(join(dir, 'timeline.sqlite'))).size) }; }
    finally { store.close(); }
    // Offline paged history over the 1M-event projection.
    const writer = new TimelineStore(join(dir, 'timeline.sqlite'));
    writer.bind(sessionId, dir);
    writer.close();
    const reader = new HistoryReader({ database: join(dir, 'timeline.sqlite'), threadId: 'bench', nativeSessionId: sessionId, cwd: dir });
    try {
      const time = fn => { const start = performance.now(); const value = fn(); return [Math.round((performance.now() - start) * 1000) / 1000, value]; };
      const [readMs] = time(() => reader.handle('thread/read', { threadId: 'bench' }));
      const [firstMs, first] = time(() => reader.handle('thread/turns/list', { threadId: 'bench', limit: 50 }));
      const deepCursor = Buffer.from(JSON.stringify({ scope: JSON.parse(Buffer.from(first.nextCursor, 'base64url')).scope, offset: 150000 })).toString('base64url');
      const [deepMs, deep] = time(() => reader.handle('thread/turns/list', { threadId: 'bench', limit: 50, cursor: deepCursor }));
      const [itemsMs] = time(() => reader.handle('thread/items/list', { threadId: 'bench', limit: 100 }));
      sample();
      report.history = { threadReadMs: readMs, firstTurnPageMs: firstMs, deepTurnPageMs: deepMs, deepPageTurns: deep.data.length, itemsPageMs: itemsMs };
      if (Math.max(readMs, firstMs, deepMs, itemsMs) > 1000) violations.push('a history page took more than 1 s');
    } finally { reader.close(); }
  }
} finally {
  if (!keep) await rm(dir, { recursive: true, force: true });
}
report.violations = violations;
console.log(JSON.stringify(report, null, 2));
if (violations.length) process.exitCode = 1;
