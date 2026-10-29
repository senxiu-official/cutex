import { DatabaseSync } from 'node:sqlite';

/**
 * The Kimi adapter's timeline: the app-server events it projected from ACP,
 * append-only in SQLite, with the same derived indexes as the DSH adapter's
 * store (the history reader is shared in shape). Kimi's own session files
 * (`wire.jsonl`) stay Kimi's; this store is what cutex shows, live and
 * offline. Adapter facts that are not history use the `kimi/` prefix.
 */
export function timelineTurnId(event) {
  if (!event || typeof event.method !== 'string' || event.method.startsWith('kimi/')) return null;
  const id = event.params?.turn?.id ?? event.params?.turnId;
  return typeof id === 'string' && id ? id : null;
}

/** Latest-value lookups the facade needs without replaying the whole timeline. */
export const LATEST_METHODS = new Set(['kimi/contextUsage/updated', 'kimi/launchSelection', 'kimi/tokenUsage', 'kimi/configOptions/updated']);
/** One cutex turn's Kimi identity (`{turnId, kimiTurnId, promptId}`), from Kimi's wire log. */
export const TURN_BINDING = 'kimi/turnBinding';
/** The owner's original image inputs of one turn (`{turnId, images: [{type:'localImage', path} | {type:'image', sha256}]}`). */
export const TURN_INPUTS = 'kimi/turnInputs';
const TIMELINE_INDEX_VERSION = '1';

export class TimelineStore {
  #statements = new Map();
  constructor(path, { readOnly = false } = {}) {
    if (typeof readOnly !== 'boolean') throw new Error('readOnly must be boolean');
    this.readOnly = readOnly;
    this.db = new DatabaseSync(path, { readOnly });
    if (readOnly) { this.db.exec('PRAGMA query_only=ON'); return; }
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS binding (id INTEGER PRIMARY KEY CHECK(id=1), session_id TEXT NOT NULL, cwd TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS timeline_turns (turn_id TEXT PRIMARY KEY, first_seq INTEGER NOT NULL UNIQUE);
      CREATE TABLE IF NOT EXISTS timeline_event_turns (seq INTEGER PRIMARY KEY, turn_id TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS timeline_event_turn ON timeline_event_turns(turn_id, seq);
      CREATE TABLE IF NOT EXISTS timeline_latest (method TEXT PRIMARY KEY, seq INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS turn_facts (turn_id TEXT NOT NULL, method TEXT NOT NULL, seq INTEGER NOT NULL, PRIMARY KEY(turn_id, method));`);
    this.db.prepare("INSERT OR IGNORE INTO store_meta VALUES ('timelineIndex', ?)").run(TIMELINE_INDEX_VERSION);
  }
  #prepare(sql) {
    let statement = this.#statements.get(sql);
    if (!statement) { statement = this.db.prepare(sql); this.#statements.set(sql, statement); }
    return statement;
  }
  #hasTable(name) { return Boolean(this.db.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?").get(name)); }
  #indexEvent(seq, event) {
    const turnId = timelineTurnId(event);
    if (turnId) {
      this.#prepare('INSERT OR IGNORE INTO timeline_turns VALUES (?, ?)').run(turnId, seq);
      this.#prepare('INSERT INTO timeline_event_turns VALUES (?, ?)').run(seq, turnId);
    }
    if ((event.method === TURN_BINDING || event.method === TURN_INPUTS) && typeof event.params?.turnId === 'string') {
      this.#prepare('INSERT INTO turn_facts VALUES (?, ?, ?) ON CONFLICT(turn_id, method) DO UPDATE SET seq=excluded.seq').run(event.params.turnId, event.method, seq);
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
      if (own) this.db.exec('COMMIT');
      return seq;
    } catch (error) { if (own) this.db.exec('ROLLBACK'); throw error; }
  }
  /** Several events in one durable transaction (one fsync). */
  appendAll(events) {
    this.assertWritable();
    this.db.exec('BEGIN IMMEDIATE');
    try { const seqs = events.map(event => this.append(event)); this.db.exec('COMMIT'); return seqs; }
    catch (error) { this.db.exec('ROLLBACK'); throw error; }
  }
  meta(key) { return this.#hasTable('store_meta') ? this.#prepare('SELECT value FROM store_meta WHERE key=?').get(key)?.value : undefined; }
  setMeta(key, value) { this.assertWritable(); this.#prepare('INSERT INTO store_meta VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value').run(key, value); }
  replay(after = 0) { return this.db.prepare('SELECT seq, body FROM events WHERE seq>? ORDER BY seq').all(after).map(row => ({ seq: row.seq, ...JSON.parse(row.body) })); }
  *iterate(after = 0) {
    for (const row of this.db.prepare('SELECT seq, body FROM events WHERE seq>? ORDER BY seq').iterate(after)) yield { seq: row.seq, ...JSON.parse(row.body) };
  }
  hasTimelineIndex() { return this.#hasTable('timeline_turns') && this.meta('timelineIndex') === TIMELINE_INDEX_VERSION; }
  turnIds() { return this.db.prepare('SELECT turn_id AS id FROM timeline_turns ORDER BY first_seq').all().map(row => row.id); }
  turnCount() { return this.db.prepare('SELECT COUNT(*) AS count FROM timeline_turns').get().count; }
  turnIdPage({ offset = 0, limit = 100, direction = 'asc' } = {}) {
    const order = direction === 'desc' ? 'DESC' : 'ASC';
    return this.db.prepare(`SELECT turn_id AS id FROM timeline_turns ORDER BY first_seq ${order} LIMIT ? OFFSET ?`).all(limit, offset).map(row => row.id);
  }
  hasTurn(turnId) { return Boolean(this.#prepare('SELECT 1 FROM timeline_turns WHERE turn_id=?').get(turnId)); }
  turnEvents(turnId) {
    return this.#prepare('SELECT e.seq, e.body FROM timeline_event_turns t JOIN events e ON e.seq=t.seq WHERE t.turn_id=? ORDER BY t.seq')
      .all(turnId).map(row => ({ seq: row.seq, ...JSON.parse(row.body) }));
  }
  turnCompleted(turnId) { return this.turnEvents(turnId).some(event => event.method === 'turn/completed'); }
  latest(method) {
    if (!LATEST_METHODS.has(method)) throw new Error(`untracked latest method: ${method}`);
    const row = this.#prepare('SELECT e.seq, e.body FROM timeline_latest l JOIN events e ON e.seq=l.seq WHERE l.method=?').get(method);
    return row ? { seq: row.seq, ...JSON.parse(row.body) } : undefined;
  }
  /** The params of a turn's `kimi/turnBinding` or `kimi/turnInputs` fact. */
  turnFact(turnId, method) {
    if (this.readOnly && !this.#hasTable('turn_facts')) return undefined;
    const row = this.#prepare('SELECT e.body FROM turn_facts f JOIN events e ON e.seq=f.seq WHERE f.turn_id=? AND f.method=?').get(turnId, method);
    return row ? JSON.parse(row.body).params : undefined;
  }
  /** Every fact of `method`, in timeline order. */
  allTurnFacts(method) {
    if (this.readOnly && !this.#hasTable('turn_facts')) return [];
    return this.db.prepare('SELECT e.body FROM turn_facts f JOIN events e ON e.seq=f.seq WHERE f.method=? ORDER BY f.seq').all(method).map(row => JSON.parse(row.body).params);
  }
  close() { this.#statements.clear(); this.db.close(); }
}
