import { readFileSync, existsSync } from 'node:fs';
import { isAbsolute, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';
import { TimelineStore, timelineTurnId, LATEST_METHODS } from './store.mjs';
import { secretRedactor, sourceFromEnv } from './redaction.mjs';

const fault = (message, code = -32602) => Object.assign(new Error(message), { code });
const check = (condition, message, code) => { if (!condition) throw fault(message, code); };
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);

// Redaction lives in redaction.mjs (literal values, the account's credential
// files and the fingerprint ledger); re-exported for existing importers.
export { secretRedactor, streamRedactor } from './redaction.mjs';

/** Snapshot latest item versions, preserving native order rather than duplicating updates. */
export function projectTimeline(events) {
  const turns = new Map();
  const put = (turn, stored) => {
    const item = stored;
    if (!item) return;
    const index = turn.items.findIndex(saved => saved.id === item.id);
    if (index < 0) turn.items.push(structuredClone(item));
    else turn.items[index] = structuredClone(item);
  };
  for (const { method, params } of events) {
    const id = params?.turn?.id ?? params?.turnId;
    if (!id || method.startsWith('kimi/')) continue;
    let turn = turns.get(id);
    if (!turn) { turn = { id, status: 'inProgress', items: [], error: null }; turns.set(id, turn); }
    if (method === 'turn/started' || method === 'turn/completed') {
      const { items, ...rest } = params.turn;
      Object.assign(turn, rest);
      for (const item of items ?? []) put(turn, item);
    } else if (params.item) put(turn, params.item);
    else if (method === 'item/agentMessage/delta') {
      const item = turn.items.find(item => item.id === params.itemId);
      if (item) item.text = (item.text ?? '') + params.delta;
    } else if (method === 'item/reasoning/textDelta') {
      const item = turn.items.find(item => item.id === params.itemId);
      if (item) { item.content ??= []; const index = params.contentIndex ?? 0; item.content[index] = (item.content[index] ?? '') + params.delta; }
    }
  }
  return [...turns.values()];
}

const ATTACHED = Symbol('attached store');
// Fork / revert / delete of a stopped agent run through session-ops.mjs.
const structureOps = existsSync(fileURLToPath(new URL('./session-ops.mjs', import.meta.url)));
const MAX_RESULT_BYTES = 8 * 1024 * 1024;
const MAX_PAGE_BYTES = 2 * 1024 * 1024;
const MAX_PAGE_ITEMS = 200;

/**
 * Turn index over the timeline: SQLite-backed when the store has the r265
 * timeline index, otherwise (a read-only pre-r265 DB, which must not be
 * migrated) built once by streaming the events table, keeping only seq numbers.
 */
function turnIndex(store) {
  if (store.hasTimelineIndex()) {
    return {
      count: () => store.turnCount(),
      page: (offset, limit, direction) => store.turnIdPage({ offset, limit, direction }),
      has: turnId => store.hasTurn(turnId),
      events: turnId => store.turnEvents(turnId),
      latest: method => store.latest(method),
    };
  }
  const seqs = new Map(), latest = new Map();
  for (const event of store.iterate()) {
    const id = timelineTurnId(event);
    if (id) { if (!seqs.has(id)) seqs.set(id, []); seqs.get(id).push(event.seq); }
    if (LATEST_METHODS.has(event.method)) latest.set(event.method, event.seq);
  }
  const ids = [...seqs.keys()];
  const select = store.db.prepare('SELECT seq, body FROM events WHERE seq=?');
  const load = seq => { const row = select.get(seq); return { seq: row.seq, ...JSON.parse(row.body) }; };
  return {
    count: () => ids.length,
    page: (offset, limit, direction) => (direction === 'desc' ? ids.slice().reverse() : ids).slice(offset, offset + limit),
    has: turnId => seqs.has(turnId),
    events: turnId => (seqs.get(turnId) ?? []).map(load),
    latest: method => latest.has(method) ? load(latest.get(method)) : undefined,
  };
}

export class HistoryReader {
  constructor({ database, threadId, nativeSessionId, cwd, [ATTACHED]: attached }) {
    check(typeof database === 'string' && isAbsolute(database) && typeof cwd === 'string' && isAbsolute(cwd), 'absolute database and cwd required');
    check(typeof threadId === 'string' && threadId.length > 0, 'threadId required');
    check(nativeSessionId == null || (typeof nativeSessionId === 'string' && nativeSessionId.length > 0), 'invalid nativeSessionId');
    this.config = { database, threadId, nativeSessionId, cwd };
    if (attached) {
      this.store = attached.store;
      this.db = attached.store.db;
      this.attached = true;
      this.liveTurnId = attached.liveTurnId;
      this.nativeSessionId = attached.store.binding()?.sessionId;
      return;
    }
    if (!existsSync(database)) return; // A never-started agent has no history; never create its DB.
    this.store = new TimelineStore(database, { readOnly: true });
    this.db = this.store.db;
    try {
      const binding = this.store.binding();
      if (binding) {
        check((nativeSessionId == null || binding.sessionId === nativeSessionId) && resolve(binding.cwd) === resolve(cwd), 'history binding mismatch');
        this.nativeSessionId = binding.sessionId;
      }
    } catch (error) { this.store.close(); throw error; }
  }
  /**
   * Serve the same paged reads from an already open (live, writable) store, e.g.
   * for thread/turns/list and thread/items/list in the live adapter. The caller
   * keeps ownership of the store; close() on the attached reader is a no-op.
   * Turn status is as stored (a live open turn stays inProgress only if
   * `liveTurnId` names it; other open turns read as interrupted).
   */
  static attach(store, { threadId, cwd, liveTurnId } = {}) {
    check(store instanceof TimelineStore, 'TimelineStore required');
    return new HistoryReader({ database: '/', threadId, cwd, nativeSessionId: null, [ATTACHED]: { store, liveTurnId } });
  }
  /** Legacy full replay (unbounded); paged methods never use it. */
  events() { return this.store ? this.store.replay() : []; }
  // A live store's index grows; only a read-only snapshot may cache the fallback index.
  #index() { return this.attached ? turnIndex(this.store) : (this.index ??= this.store ? turnIndex(this.store) : null); }
  /** One turn, projected from only its own events; offline open turns read as interrupted. */
  #turn(index, turnId) {
    const [turn] = projectTimeline(index.events(turnId));
    const live = typeof this.liveTurnId === 'function' ? this.liveTurnId() : this.liveTurnId;
    if (turn && turn.status === 'inProgress' && turn.id !== live) turn.status = 'interrupted';
    return turn;
  }
  /** Thread metadata plus (optionally) all turns, refusing above the result budget. */
  snapshot({ includeTurns = true } = {}) {
    const index = this.#index();
    const occupancy = index?.latest('kimi/contextUsage/updated')?.params;
    const selection = index?.latest('kimi/launchSelection')?.params;
    const turns = [];
    if (includeTurns && index) {
      let bytes = 0;
      const total = index.count();
      for (let offset = 0; offset < total; offset += 100) {
        for (const id of index.page(offset, 100, 'asc')) {
          const turn = this.#turn(index, id);
          if (!turn) continue;
          bytes += Buffer.byteLength(JSON.stringify(turn));
          check(bytes <= MAX_RESULT_BYTES, 'history too large; use paginated methods', -32001);
          turns.push(turn);
        }
      }
    }
    return { id: this.config.threadId, cwd: this.config.cwd, path: this.config.database, status: { type: 'notLoaded' }, historyMode: 'paginated',
      modelProvider: 'kimi', model: selection?.model ?? null, reasoningEffort: selection?.reasoningEffort ?? null,
      capabilities: { readOnly: true, resume: false, interrupt: false, steer: false, fork: structureOps, revert: structureOps, delete: structureOps, tokenUsage: false },
      kimiContext: occupancy ? { used: occupancy.used, size: occupancy.size, observedAt: occupancy.observedAt ?? null, source: 'acp' } : null,
      turns };
  }
  handle(method, params = {}) {
    if (method === 'initialize') return { userAgent: 'cutex-kimi-history-reader/experimental', readOnly: true,
      capabilities: { history: true, resume: false, interrupt: false, steer: false, fork: structureOps, revert: structureOps, delete: structureOps, tokenUsage: false } };
    if (!['thread/read', 'thread/turns/list', 'thread/items/list'].includes(method)) throw fault('method unavailable in read-only Kimi history', -32601);
    check(params.threadId === this.config.threadId, 'unknown thread', -32004);
    if (!this.db && method !== 'thread/read') {
      return { data: [], nextCursor: null, backwardsCursor: null };
    }
    if (method === 'thread/read') {
      const result = { thread: this.snapshot({ includeTurns: params.includeTurns === true }) };
      check(Buffer.byteLength(JSON.stringify(result)) <= MAX_RESULT_BYTES, 'history too large; use paginated methods', -32001);
      return result;
    }
    const direction = params.sortDirection ?? 'desc';
    check(['asc', 'desc'].includes(direction), 'invalid sortDirection');
    const limit = params.limit ?? 100;
    check(Number.isSafeInteger(limit) && limit > 0, 'limit must be a positive integer');
    const view = params.itemsView ?? 'summary';
    check(['summary', 'full', 'notLoaded'].includes(view), 'invalid itemsView');
    const scope = JSON.stringify([method, params.threadId, params.turnId ?? null, direction, view]);
    let position = { offset: 0, item: 0 };
    if (params.cursor != null) {
      try {
        const cursor = JSON.parse(Buffer.from(params.cursor, 'base64url').toString());
        check(cursor.scope === scope && Number.isSafeInteger(cursor.offset) && cursor.offset >= 0
          && (cursor.item === undefined || (Number.isSafeInteger(cursor.item) && cursor.item >= 0)), 'invalid cursor');
        position = { offset: cursor.offset, item: cursor.item ?? 0 };
      } catch { throw fault('invalid cursor'); }
    }
    const index = this.#index();
    const max = Math.min(limit, MAX_PAGE_ITEMS);
    const data = [];
    let bytes = 0;
    const admit = entry => {
      const size = Buffer.byteLength(JSON.stringify(entry));
      check(size <= MAX_RESULT_BYTES, 'history entry too large', -32001);
      if (data.length && bytes + size > MAX_PAGE_BYTES) return false;
      data.push(entry); bytes += size;
      return true;
    };
    const encode = next => Buffer.from(JSON.stringify({ scope, ...next })).toString('base64url');
    if (method === 'thread/turns/list') {
      const ids = index.page(position.offset, max, direction);
      let consumed = 0;
      for (const id of ids) {
        const turn = this.#turn(index, id);
        const items = view === 'notLoaded' ? [] : view === 'full' ? turn.items : [turn.items.find(i => i.type === 'userMessage'), turn.items.findLast(i => i.type === 'agentMessage')].filter(Boolean);
        if (!admit({ ...turn, items, itemsView: view })) break;
        consumed++;
      }
      const next = position.offset + consumed;
      return { data, nextCursor: next < index.count() ? encode({ offset: next }) : null, backwardsCursor: null };
    }
    // thread/items/list: walk turns lazily from the cursor; never flatten the whole thread.
    let ids;
    if (params.turnId !== undefined) {
      check(index.has(params.turnId), 'unknown turn', -32004);
      ids = [params.turnId];
    }
    const total = ids ? 1 : index.count();
    let { offset, item } = position;
    let full = false;
    while (!full && offset < total && data.length < max) {
      const id = ids ? ids[offset] : index.page(offset, 1, direction)[0];
      const items = this.#turn(index, id)?.items ?? [];
      const ordered = direction === 'desc' ? items.slice().reverse() : items;
      while (item < ordered.length && data.length < max) {
        if (!admit({ turnId: id, item: ordered[item] })) { full = true; break; }
        item++;
      }
      if (item >= ordered.length) { offset++; item = 0; }
    }
    return { data, nextCursor: offset < total ? encode({ offset, item }) : null, backwardsCursor: null };
  }
  close() { if (!this.attached) this.store?.close(); }
}

export async function serveHistory(reader, input = process.stdin, output = process.stdout, redact = value => value) {
  let initialized = false, ready = false;
  for await (const line of createInterface({ input, crlfDelay: Infinity })) {
    if (!line.trim()) continue;
    let response, message;
    try { message = JSON.parse(line); }
    catch { output.write(JSON.stringify({ id: null, error: { code: -32700, message: 'invalid JSON' } }) + '\n'); continue; }
    try {
      check(object(message) && typeof message.method === 'string' && (message.params === undefined || object(message.params)), 'invalid request', -32600);
      check(Buffer.byteLength(line) <= 1024 * 1024, 'request too large', -32600);
      if (message.method === 'initialized') { check(initialized, 'initialize required', -32002); ready = true; response = {}; }
      else if (message.method === 'initialize') { check(!initialized, 'already initialized', -32600); response = reader.handle('initialize'); initialized = true; }
      else { check(ready, 'initialize and initialized required', -32002); response = reader.handle(message.method, message.params ?? {}); }
      if (message.id !== undefined) output.write(JSON.stringify({ id: message.id, result: redact(response) }) + '\n');
    } catch (error) {
      if (message?.id !== undefined) output.write(JSON.stringify({ id: message.id, error: { code: error.code ?? -32603, message: error.code ? error.message : 'history unavailable' } }) + '\n');
    }
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  let reader;
  try {
    const file = process.env.KIMI_CUTEX_HISTORY_CONFIG ?? process.env.DSH_CUTEX_HISTORY_CONFIG;
    check(file && isAbsolute(file), 'explicit absolute history config required');
    reader = new HistoryReader(JSON.parse(readFileSync(file, 'utf8')));
    // The values the runtime redacts (cutex passes the current and earlier
    // keys, and the paths of the account's credential files and fingerprint
    // ledger): results are redacted again after projection.
    await serveHistory(reader, process.stdin, process.stdout, secretRedactor(sourceFromEnv()));
  } catch { console.error('Kimi history reader failed; check explicit private history config.'); process.exitCode = 1; }
  finally { reader?.close(); }
}
