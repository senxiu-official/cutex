import { DatabaseSync } from 'node:sqlite';
import { statSync } from 'node:fs';
import { isAbsolute } from 'node:path';
import { isOwnerInput, nativeTurnId, nativeTurnNumber, projectNativeEvent } from './nativeProjection.mjs';

const sameCursor = (a, b) => a === null ? b === null : b !== null
  && a.version === b.version && a.sessionId === b.sessionId && a.nativeSeq === b.nativeSeq && a.hash === b.hash;

/** Timeline turn key of one projected app-server event; mirrors projectTimeline. */
export function timelineTurnId(event) {
  if (!event || typeof event.method !== 'string' || event.method.startsWith('dsh/')) return null;
  const id = event.params?.turn?.id ?? event.params?.turnId;
  return typeof id === 'string' && id ? id : null;
}
/** Re-exported: the projection itself applies the rule (live, history, re-projection). */
export { isOwnerInput };

/** Latest-value lookups the facade needs without replaying the whole timeline. */
const LATEST_METHODS = new Set(['dsh/contextUsage/updated', 'dsh/launchSelection', 'dsh/configOptions/updated']);
const TIMELINE_INDEX_VERSION = '1';
/**
 * Highest events.seq the derived indexes cover, written with every append in
 * the same transaction. A component that predates the index (r264, r265
 * 1c36212c: rollback, a still-pinned older adapter) appends events without
 * touching the index or this mark; the next writable open indexes exactly
 * the events above it, and read-only readers fall back to streaming while
 * the index lags.
 */
const INDEXED_SEQ = 'timelineIndexedSeq';
/**
 * The owner's original image inputs of one turn, recorded by the adapter
 * before `session/prompt` (`{threadId, turnId, images: [{type: 'localImage',
 * path} | {type: 'image', sha256}]}`, in input order). DSH keeps image bytes
 * in its own attachment service; the projection uses this fact to show the
 * owner's upload (`localImage`) instead of a placeholder. No bytes, no
 * DSH-internal paths. Not history (`dsh/`): never a timeline item itself.
 */
export const TURN_INPUTS = 'dsh/turnInputs';

/**
 * The adapter owns replay: ACP resume explicitly does not replay updates.
 *
 * Long-lived sessions: every query used per native commit or per history page is
 * indexed; nothing here loads the complete timeline except the legacy replay().
 * timeline_turns/timeline_event_turns/timeline_latest are derived indexes over
 * the append-only events table, written in the same transaction as each event
 * and backfilled once (streaming) when an older database is opened writable.
 */
/** Native seq of a committed projected event (non-enumerable). */
export const NATIVE_SEQ = Symbol('nativeSeq');

export class TimelineStore {
  #batch;
  #statements = new Map();
  constructor(path, { readOnly = false } = {}) {
    if (typeof readOnly !== 'boolean') throw new Error('readOnly must be boolean');
    this.readOnly = readOnly;
    this.db = new DatabaseSync(path, { readOnly });
    if (readOnly) {
      // History readers never initialize/migrate schemas, change journal mode,
      // or create a missing DB. SQLite's open flag remains read-only even if a
      // caller subsequently disables query_only through the exposed DB handle.
      this.db.exec('PRAGMA query_only=ON');
      return;
    }
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS binding (id INTEGER PRIMARY KEY CHECK(id=1), session_id TEXT NOT NULL, cwd TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS native_cursors (session_id TEXT PRIMARY KEY, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS native_events (
        session_id TEXT NOT NULL, native_seq INTEGER NOT NULL, native_id TEXT NOT NULL UNIQUE,
        event_type TEXT NOT NULL, native_turn INTEGER, call_id TEXT, body TEXT NOT NULL, updates TEXT NOT NULL,
        PRIMARY KEY(session_id, native_seq));
      CREATE INDEX IF NOT EXISTS native_calls ON native_events(session_id, event_type, call_id, native_seq);
      CREATE INDEX IF NOT EXISTS native_turns ON native_events(session_id, native_turn);
      CREATE TABLE IF NOT EXISTS native_items (
        session_id TEXT NOT NULL, native_turn INTEGER NOT NULL, item_id TEXT NOT NULL, body TEXT NOT NULL,
        PRIMARY KEY(session_id, native_turn, item_id));
      CREATE TABLE IF NOT EXISTS native_status (session_id TEXT PRIMARY KEY, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS timeline_turns (turn_id TEXT PRIMARY KEY, first_seq INTEGER NOT NULL UNIQUE);
      CREATE TABLE IF NOT EXISTS timeline_event_turns (seq INTEGER PRIMARY KEY, turn_id TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS timeline_event_turn ON timeline_event_turns(turn_id, seq);
      CREATE TABLE IF NOT EXISTS timeline_latest (method TEXT PRIMARY KEY, seq INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS turn_inputs (turn_id TEXT PRIMARY KEY, seq INTEGER NOT NULL);`);
    this.#backfillTimelineIndex();
  }
  #prepare(sql) {
    let statement = this.#statements.get(sql);
    if (!statement) { statement = this.db.prepare(sql); this.#statements.set(sql, statement); }
    return statement;
  }
  #hasTable(name) {
    return Boolean(this.db.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?").get(name));
  }
  /** The index version and the highest seq it covers vs the newest event. */
  #indexState() {
    const meta = key => this.db.prepare('SELECT value FROM store_meta WHERE key=?').get(key)?.value;
    const mark = Number(meta(INDEXED_SEQ));
    return { version: meta('timelineIndex'), mark: Number.isSafeInteger(mark) && mark >= 0 ? mark : null,
      max: this.db.prepare('SELECT MAX(seq) AS seq FROM events').get().seq ?? 0 };
  }
  /**
   * Streaming backfill: once for databases written before r265 (or by an
   * index without its mark), and incrementally for events an older component
   * appended after the index (above the mark).
   */
  #backfillTimelineIndex() {
    const { version, mark, max } = this.#indexState();
    if (version === TIMELINE_INDEX_VERSION && mark === max) return;
    this.db.exec('BEGIN IMMEDIATE');
    try {
      let after = 0;
      if (version === TIMELINE_INDEX_VERSION && mark !== null && mark < max) after = mark;
      else this.db.exec('DELETE FROM timeline_turns; DELETE FROM timeline_event_turns; DELETE FROM timeline_latest; DELETE FROM turn_inputs;');
      for (const row of this.db.prepare('SELECT seq, body FROM events WHERE seq>? ORDER BY seq').iterate(after)) this.#indexEvent(row.seq, JSON.parse(row.body));
      this.db.prepare("INSERT OR REPLACE INTO store_meta VALUES ('timelineIndex', ?)").run(TIMELINE_INDEX_VERSION);
      this.db.prepare('INSERT OR REPLACE INTO store_meta VALUES (?, ?)').run(INDEXED_SEQ, String(max));
      this.db.exec('COMMIT');
    } catch (error) { this.db.exec('ROLLBACK'); throw error; }
  }
  #indexEvent(seq, event) {
    const turnId = timelineTurnId(event);
    if (turnId) {
      this.#prepare('INSERT OR IGNORE INTO timeline_turns VALUES (?, ?)').run(turnId, seq);
      this.#prepare('INSERT INTO timeline_event_turns VALUES (?, ?)').run(seq, turnId);
    }
    if (event.method === TURN_INPUTS && typeof event.params?.turnId === 'string') {
      this.#prepare('INSERT INTO turn_inputs VALUES (?, ?) ON CONFLICT(turn_id) DO UPDATE SET seq=excluded.seq').run(event.params.turnId, seq);
    }
    if (LATEST_METHODS.has(event.method)) {
      this.#prepare('INSERT INTO timeline_latest VALUES (?, ?) ON CONFLICT(method) DO UPDATE SET seq=excluded.seq').run(event.method, seq);
    }
  }
  assertWritable() { if (this.readOnly) throw new Error('TimelineStore is read-only'); }
  bind(sessionId, cwd) { this.assertWritable(); this.db.prepare('INSERT OR REPLACE INTO binding VALUES (1, ?, ?)').run(sessionId, cwd); }
  binding() { return this.db.prepare('SELECT session_id AS sessionId, cwd FROM binding WHERE id=1').get(); }
  append(event) {
    this.assertWritable();
    const own = !this.db.isTransaction;
    if (own) this.db.exec('BEGIN IMMEDIATE');
    try {
      const seq = Number(this.#prepare('INSERT INTO events(body) VALUES (?)').run(JSON.stringify(event)).lastInsertRowid);
      this.#indexEvent(seq, event);
      this.#prepare('INSERT OR REPLACE INTO store_meta VALUES (?, ?)').run(INDEXED_SEQ, String(seq));
      if (own) this.db.exec('COMMIT');
      return seq;
    } catch (error) { if (own) this.db.exec('ROLLBACK'); throw error; }
  }
  /** Legacy full replay. Long-lived callers should prefer the indexed helpers below. */
  replay(after = 0) { return this.db.prepare('SELECT seq, body FROM events WHERE seq>? ORDER BY seq').all(after).map(row => ({ seq: row.seq, ...JSON.parse(row.body) })); }
  /** Streaming replay with bounded memory. */
  *iterate(after = 0) {
    for (const row of this.db.prepare('SELECT seq, body FROM events WHERE seq>? ORDER BY seq').iterate(after)) yield { seq: row.seq, ...JSON.parse(row.body) };
  }
  /**
   * Whether derived timeline indexes exist and cover every event (false for a
   * read-only pre-r265 DB, and for one an older component appended to since:
   * readers then stream the events instead).
   */
  hasTimelineIndex() {
    if (!(this.#hasTable('timeline_turns') && this.#hasTable('store_meta'))) return false;
    const { version, mark, max } = this.#indexState();
    return version === TIMELINE_INDEX_VERSION && mark === max;
  }
  /** Ordered turn IDs, without loading their events. */
  turnIds() { return this.db.prepare('SELECT turn_id AS id FROM timeline_turns ORDER BY first_seq').all().map(row => row.id); }
  turnCount() { return this.db.prepare('SELECT COUNT(*) AS count FROM timeline_turns').get().count; }
  /** One page of turn IDs in either direction (offset counts from the newest turn for desc). */
  turnIdPage({ offset = 0, limit = 100, direction = 'asc' } = {}) {
    const order = direction === 'desc' ? 'DESC' : 'ASC';
    return this.db.prepare(`SELECT turn_id AS id FROM timeline_turns ORDER BY first_seq ${order} LIMIT ? OFFSET ?`).all(limit, offset).map(row => row.id);
  }
  hasTurn(turnId) { return Boolean(this.#prepare('SELECT 1 FROM timeline_turns WHERE turn_id=?').get(turnId)); }
  /** Events of exactly one turn, in timeline order. */
  turnEvents(turnId) {
    return this.#prepare('SELECT e.seq, e.body FROM timeline_event_turns t JOIN events e ON e.seq=t.seq WHERE t.turn_id=? ORDER BY t.seq')
      .all(turnId).map(row => ({ seq: row.seq, ...JSON.parse(row.body) }));
  }
  /** Whether a terminal event for a turn has been committed; O(events of that turn). */
  turnCompleted(turnId) {
    return this.turnEvents(turnId).some(event => event.method === 'turn/completed');
  }
  /** Latest event for one of the tracked facade methods, or undefined. */
  latest(method) {
    if (!LATEST_METHODS.has(method)) throw new Error(`untracked latest method: ${method}`);
    const row = this.#prepare('SELECT e.seq, e.body FROM timeline_latest l JOIN events e ON e.seq=l.seq WHERE l.method=?').get(method);
    return row ? { seq: row.seq, ...JSON.parse(row.body) } : undefined;
  }
  /** The recorded `dsh/turnInputs` params of a turn, or undefined (also for read-only pre-r265 stores). */
  turnInputs(turnId) {
    if (this.readOnly && !this.#hasTable('turn_inputs')) return undefined;
    const row = this.#prepare('SELECT e.body FROM turn_inputs t JOIN events e ON e.seq=t.seq WHERE t.turn_id=?').get(turnId);
    return row ? JSON.parse(row.body).params : undefined;
  }
  /** Every recorded `dsh/turnInputs` event, in timeline order (revert / fork carry them over). */
  allTurnInputs() {
    if (this.readOnly && !this.#hasTable('turn_inputs')) return [];
    return this.db.prepare('SELECT e.body FROM turn_inputs t JOIN events e ON e.seq=t.seq ORDER BY t.seq').all().map(row => JSON.parse(row.body));
  }
  /**
   * A turn's recorded images for the projection: a `localImage` whose file
   * still exists stays, anything else (data URL digests, deleted uploads) is
   * null, so its block keeps the placeholder.
   */
  #projectionInputs(sessionId, turn) {
    const images = this.turnInputs(nativeTurnId(sessionId, turn))?.images;
    if (!Array.isArray(images)) return undefined;
    return images.map(image => {
      if (image?.type !== 'localImage' || typeof image.path !== 'string' || !isAbsolute(image.path)) return null;
      try { return statSync(image.path).isFile() ? { type: 'localImage', path: image.path } : null; } catch { return null; }
    });
  }
  nativeCursor(sessionId) {
    const row = this.#prepare('SELECT body FROM native_cursors WHERE session_id=?').get(sessionId);
    return row ? JSON.parse(row.body) : null;
  }
  /** Last native-log reader status (ok / pending / damaged / diverged / ...), or null. */
  nativeStatus(sessionId) {
    if (this.readOnly && !this.#hasTable('native_status')) return null;
    const row = this.#prepare('SELECT body FROM native_status WHERE session_id=?').get(sessionId);
    return row ? JSON.parse(row.body) : null;
  }
  recordNativeStatus(sessionId, status) {
    this.assertWritable();
    this.#prepare('INSERT INTO native_status VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET body=excluded.body').run(sessionId, JSON.stringify(status));
  }
  maxNativeTurn(sessionId) {
    return this.#prepare('SELECT MAX(native_turn) AS turn FROM native_events WHERE session_id=?').get(sessionId).turn ?? 0;
  }
  nextNativeTurn(sessionId) { return this.maxNativeTurn(sessionId) + 1; }

  /**
   * Raw fact, semantic updates, item snapshots, timeline and cursor are one FULL
   * synchronous SQLite transaction. The callback is a post-commit notification,
   * not part of durability: if it throws, replay/reconcile still deduplicates.
   * Callers serialize commits; stale cursor CAS is always an error.
   * Inside commitNativeBatch, the enclosing batch owns the transaction and the
   * notification (instance wrappers of commitNative, e.g. redaction, still run).
   */
  commitNative(record) {
    this.assertWritable();
    if (this.#batch) {
      const committed = this.#commitOne(record);
      this.#batch.push(...committed);
      return committed;
    }
    this.db.exec('BEGIN IMMEDIATE');
    let committed;
    try {
      committed = this.#commitOne(record);
      this.db.exec('COMMIT');
    } catch (error) { this.db.exec('ROLLBACK'); throw error; }
    this.onNativeCommitted?.(committed);
    return committed;
  }
  /**
   * Commit contiguous records in ONE durable transaction (one fsync instead of
   * one per event). Records pass through this.commitNative, so instance wrappers
   * (e.g. redaction) still apply. If a wrapper is asynchronous and did not
   * commit its record synchronously, the batch commits the prefix before it and
   * the rest continues one record at a time; a Promise is then returned.
   */
  commitNativeBatch(records) {
    this.assertWritable();
    if (this.#batch) throw new Error('Nested native commit batch');
    if (!Array.isArray(records)) throw new Error('Invalid native commit batch');
    if (!records.length) return [];
    this.db.exec('BEGIN IMMEDIATE');
    this.#batch = [];
    let committed, stop = -1, pending;
    const waits = [];
    try {
      for (let index = 0; index < records.length; index++) {
        const result = this.commitNative(records[index]);
        if (result && typeof result.then === 'function') {
          const saved = this.nativeCursor(records[index].sessionId);
          if (saved && saved.nativeSeq === records[index].nativeSeq && saved.hash === records[index].cursor?.hash) { waits.push(result); continue; }
          stop = index; pending = result; break;
        }
      }
      this.db.exec('COMMIT');
      committed = this.#batch;
    } catch (error) { this.db.exec('ROLLBACK'); throw error; }
    finally { this.#batch = undefined; }
    if (committed.length) this.onNativeCommitted?.(committed);
    if (stop < 0 && !waits.length) return committed;
    return (async () => {
      await Promise.all(waits);
      if (stop >= 0) {
        await pending;
        for (const record of records.slice(stop + 1)) await this.commitNative(record);
      }
      return committed;
    })();
  }
  #commitOne(record) {
    const { sessionId, nativeSeq, nativeId, event, updates, cursor, previousCursor = null } = record;
    if (typeof sessionId !== 'string' || !sessionId || !Number.isSafeInteger(nativeSeq) || nativeSeq < 0
      || event?.seq !== nativeSeq || nativeId !== JSON.stringify([sessionId, nativeSeq])
      || cursor?.version !== 1 || cursor.sessionId !== sessionId || cursor.nativeSeq !== nativeSeq
      || !/^[a-f0-9]{64}$/.test(cursor.hash) || !Array.isArray(updates)) throw new Error('Invalid native commit envelope');
    const committed = [];
    const saved = this.nativeCursor(sessionId);
    if (!sameCursor(saved, previousCursor)) throw new Error('Stale native cursor CAS');
    if (nativeSeq !== (saved?.nativeSeq ?? -1) + 1) throw new Error('Native commit sequence gap');
    const lastStart = this.#prepare("SELECT native_turn FROM native_events WHERE session_id=? AND event_type='turn/start' ORDER BY native_seq DESC LIMIT 1").get(sessionId);
    const turn = nativeTurnNumber(event, lastStart?.native_turn ?? null);
    const callId = event.type === 'tool/call' ? event.data?.callId : event.type === 'tool/result' ? event.data?.message?.toolCallId : null;
    const call = event.type === 'tool/result' && typeof callId === 'string'
      ? this.#prepare("SELECT body FROM native_events WHERE session_id=? AND event_type='tool/call' AND call_id=? ORDER BY native_seq DESC LIMIT 1").get(sessionId, callId) : undefined;
    // Item snapshots are needed only for the terminal projection; loading them
    // for every event would make a long turn quadratic.
    const items = turn === null || event.type !== 'turn/end' ? [] : this.#prepare('SELECT body FROM native_items WHERE session_id=? AND native_turn=? ORDER BY rowid').all(sessionId, turn).map(row => JSON.parse(row.body));
    const inputs = turn !== null && event.type === 'user/message' && isOwnerInput(event) ? this.#projectionInputs(sessionId, turn) : undefined;
    const projected = projectNativeEvent(record, { turn, toolCall: call ? JSON.parse(call.body) : undefined, items, inputs });
    this.#prepare('INSERT INTO native_events VALUES (?, ?, ?, ?, ?, ?, ?, ?)').run(
      sessionId, nativeSeq, nativeId, event.type, turn, typeof callId === 'string' ? callId : null, JSON.stringify(event), JSON.stringify(updates));
    for (const projectedEvent of projected) {
      const item = projectedEvent.params.item;
      if (item) this.#prepare(`INSERT INTO native_items VALUES (?, ?, ?, ?)
        ON CONFLICT(session_id, native_turn, item_id) DO UPDATE SET body=excluded.body`).run(sessionId, turn, item.id, JSON.stringify(item));
      const seq = this.append(projectedEvent);
      // The native seq travels with each committed event (batched commits
      // notify once per batch), invisible to JSON and deep comparisons.
      committed.push(Object.defineProperty({ seq, ...projectedEvent }, NATIVE_SEQ, { value: nativeSeq }));
    }
    this.#prepare('INSERT INTO native_cursors VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET body=excluded.body').run(sessionId, JSON.stringify(cursor));
    return committed;
  }
  /** The newest committed native `tool/call` event of `callId` (redacted as stored), or undefined. */
  nativeToolCall(sessionId, callId) {
    const row = this.#prepare("SELECT body FROM native_events WHERE session_id=? AND event_type='tool/call' AND call_id=? ORDER BY native_seq DESC LIMIT 1").get(sessionId, callId);
    if (!row) return undefined;
    try { const event = JSON.parse(row.body); return typeof event?.data?.name === 'string' ? event : undefined; } catch { return undefined; }
  }
  close() { this.#statements.clear(); this.db.close(); }
}
