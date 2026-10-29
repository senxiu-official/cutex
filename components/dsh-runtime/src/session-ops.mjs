// One-shot structural operations on a STOPPED (revert) or any (fork) DSH agent's
// private state: cutex `cutex/session/history/revert` and `cutex/session/fork`.
//
// DSH 0.2.1-rc.3 has no ACP fork/rewind/delete (docs/development/r265-dsh-history-notes.md
// §3), so both operations are built on the native JSONL log:
//
// - revertPrepare: stage a log that is the agent's own log cut at the end of the
//   last kept turn (header and rows byte for byte, so the native session ID and
//   every kept turn ID stay the same) plus a fresh timeline DB bound to the same
//   native session, with the cut log already projected. The caller (Rust, `dsh_structure.rs`) swaps them in with
//   renames while the runtime is stopped and restarts it. Conversation only:
//   DSH keeps no workspace file checkpoints, so files are never restored.
// - forkCopy: copy a closed prefix of the source log into the child agent's own
//   sessions root under a new native ID (`writeNativeSessionCopy`) and pre-bind
//   the child's timeline DB. Reads only; the source may keep running.
//
// Usage: DSH_CUTEX_SESSION_OPS_CONFIG=<absolute JSON file> node session-ops.mjs
// Prints exactly one JSON line: {"ok":true,...} or {"ok":false,"code","message"}.
import { readFileSync, existsSync, statSync } from 'node:fs';
import { open, mkdir, writeFile } from 'node:fs/promises';
import { basename, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { TimelineStore, TURN_INPUTS, timelineTurnId } from './store.mjs';
import { nativeTurnId } from './nativeProjection.mjs';
import { decodeNativeRow, loadNativeCatalog, locateNativeLog, reconcileNativeLog, writeNativeSessionCopy } from './reconcile.mjs';
import { secretRedactor } from './history-reader.mjs';

const fail = (code, message) => Object.assign(new Error(message), { opsCode: code });
const check = (condition, code, message) => { if (!condition) throw fail(code, message); };
const absolute = (value, name) => check(typeof value === 'string' && isAbsolute(value), 'invalid_request', `${name} must be an absolute path`);
const nonEmpty = (value, name) => check(typeof value === 'string' && value.length > 0, 'invalid_request', `${name} required`);
/** Rows that start a turn in DSH's log: the prompt is spliced into the inbox, then turn/start. */
const TURN_OPENERS = new Set(['agent/inbox/spliced', 'turn/start']);

/** Timeline turn IDs in order, from the index or (pre-r265 read-only DB) by streaming. */
function orderedTurnIds(store) {
  if (store.hasTimelineIndex()) return store.turnIds();
  const seen = new Set();
  for (const event of store.iterate()) { const id = timelineTurnId(event); if (id) seen.add(id); }
  return [...seen];
}

/** Native turn number of a native-mode timeline turn ID (`<sessionId>:turn:<N>`). */
function nativeTurnOf(sessionId, turnId) {
  const prefix = `${sessionId}:turn:`;
  check(turnId.startsWith(prefix), 'unsupported_history', `turn ${turnId} is not a native DSH turn of this session`);
  const turn = Number(turnId.slice(prefix.length));
  check(Number.isSafeInteger(turn) && turn >= 1 && String(turn) === turnId.slice(prefix.length), 'unsupported_history', `turn ${turnId} has no native turn number`);
  return turn;
}

function turnEndSeq(store, sessionId, turn) {
  return store.db.prepare("SELECT native_seq AS seq FROM native_events WHERE session_id=? AND event_type='turn/end' AND native_turn=? ORDER BY native_seq DESC LIMIT 1")
    .get(sessionId, turn)?.seq ?? null;
}

function openBound(database, sessionId, cwd, readOnly) {
  check(existsSync(database), 'history_unavailable', 'the agent has no timeline yet');
  const store = new TimelineStore(database, { readOnly });
  const binding = store.binding();
  if (!binding || binding.sessionId !== sessionId || resolve(binding.cwd) !== resolve(cwd)) {
    store.close();
    throw fail('history_binding_mismatch', 'the timeline is not bound to this native DSH session');
  }
  return store;
}

/** The source's `dsh/turnInputs` of `turnIds`, re-keyed to `targetSessionId` (same native turn numbers). */
function keptTurnInputs(store, sessionId, targetSessionId, turnIds) {
  const kept = new Set(turnIds);
  return store.allTurnInputs().map(event => event.params)
    .filter(params => kept.has(params?.turnId))
    .map(params => ({ ...params, threadId: targetSessionId, turnId: nativeTurnId(targetSessionId, nativeTurnOf(sessionId, params.turnId)) }));
}

/**
 * A fresh timeline bound to `sessionId`, carrying the launch selection
 * (model/effort) of the source, with `logPath` already projected so offline
 * history is complete before the runtime starts. Raw bodies pass the same
 * secret redaction as the runtime's projection (values from
 * DSH_CUTEX_REDACT, a JSON string array the caller passes in the environment).
 * `turnInputs`: the `dsh/turnInputs` params of the kept turns, already keyed
 * to `sessionId` (a turn's user message then shows the owner's upload again).
 */
async function writeBoundTimeline(path, { sessionId, cwd, selection, turnInputs = [], logPath, catalog }) {
  check(!existsSync(path), 'invalid_request', 'target timeline already exists');
  let secrets = [];
  try { secrets = JSON.parse(process.env.DSH_CUTEX_REDACT ?? '[]'); } catch {}
  const redact = secretRedactor(Array.isArray(secrets) ? secrets : []);
  const store = new TimelineStore(path);
  try {
    store.bind(sessionId, cwd);
    if (selection) store.append({ method: selection.method, params: selection.params });
    // The owner's original images of the kept turns (before their user messages are projected).
    for (const params of turnInputs) store.append({ method: TURN_INPUTS, params });
    const commit = store.commitNative.bind(store);
    store.commitNative = record => commit({ ...record, event: redact(record.event), updates: redact(record.updates) });
    let result;
    do { result = await reconcileNativeLog({ logPath, sessionId, catalog, store }); } while (result.more);
    check(result.status?.state === 'ok', 'native_log_damaged', `the copied native log does not project cleanly (${result.status?.state}: ${result.status?.reason ?? ''})`);
  } finally { store.close(); }
}

/** Stream text log rows (header first) with their decoded event; stops when `visit` returns false. */
async function scanTextLog(logPath, visit) {
  const handle = await open(logPath, 'r');
  try {
    let index = -1;
    for await (const line of handle.readLines({ encoding: 'utf8' })) {
      if (index === -1) { index++; if (visit({ header: JSON.parse(line), line }) === false) return; continue; }
      if (!line.length) throw fail('native_log_damaged', `blank row after seq ${index - 1}`);
      const event = decodeNativeRow(JSON.parse(line), index);
      if (visit({ event, line }) === false) return;
      index++;
    }
  } finally { await handle.close(); }
}

export async function revertPrepare(config) {
  for (const key of ['database', 'sessionsRoot', 'cwd', 'stagingDirectory', 'installRoot']) absolute(config[key], key);
  nonEmpty(config.sessionId, 'sessionId');
  nonEmpty(config.beforeTurnId, 'beforeTurnId');
  const { sessionId } = config;
  const store = openBound(config.database, sessionId, config.cwd, false);
  let turns, keptEnd, keptTurn, selection, turnInputs;
  try {
    turns = orderedTurnIds(store);
    const index = turns.indexOf(config.beforeTurnId);
    check(index >= 0, 'turn_not_found', `turn ${config.beforeTurnId} is not in this agent's history`);
    keptTurn = index > 0 ? nativeTurnOf(sessionId, turns[index - 1]) : null;
    for (const id of turns.slice(index)) nativeTurnOf(sessionId, id);
    if (keptTurn !== null) {
      keptEnd = turnEndSeq(store, sessionId, keptTurn);
      check(keptEnd !== null, 'turn_in_progress', `turn ${turns[index - 1]} has no native end; revert before it instead`);
    }
    selection = store.latest('dsh/launchSelection');
    turns = { kept: turns.slice(0, index), removed: turns.slice(index) };
    turnInputs = keptTurnInputs(store, sessionId, sessionId, turns.kept);
  } finally { store.close(); }
  const logPath = await locateNativeLog(config.sessionsRoot, sessionId);
  check(logPath, 'history_unavailable', 'the native DSH log is missing');
  check(!logPath.endsWith('.zstd'), 'unsupported_history', 'compressed native DSH logs cannot be reverted');
  const lines = [];
  let header, last = null, cut = keptEnd ?? null, ended = false;
  await scanTextLog(logPath, ({ header: h, event, line }) => {
    if (h) { header = h; lines.push(line); return true; }
    if (keptEnd === undefined && TURN_OPENERS.has(event.type)) { ended = true; return false; }
    lines.push(line); last = event;
    if (keptEnd !== undefined && event.seq === keptEnd) { ended = true; return false; }
    return true;
  });
  check(header?.type === 'session' && header.version === 4 && header.id === sessionId, 'unsupported_history', 'the native log is not a pinned v4 log of this session');
  check(header.parentSession === undefined && header.origin === undefined, 'unsupported_history', 'only top-level native sessions can be reverted');
  check(ended, 'native_log_behind', 'the native log ends before the revert boundary');
  if (keptEnd !== undefined) check(last?.type === 'turn/end' && last.data?.turn === keptTurn, 'native_log_damaged', 'the revert boundary is not the end of the last kept turn');
  else cut = last?.seq ?? -1;
  await mkdir(config.stagingDirectory, { recursive: true, mode: 0o700 });
  const stagedLog = join(config.stagingDirectory, basename(logPath));
  await writeFile(stagedLog, lines.join('\n') + '\n', { flag: 'wx', mode: 0o600, flush: true });
  const stagedDatabase = join(config.stagingDirectory, 'timeline.sqlite');
  const catalog = await loadNativeCatalog(join(config.installRoot, 'package.json'));
  await writeBoundTimeline(stagedDatabase, { sessionId, cwd: config.cwd, selection, turnInputs, logPath: stagedLog, catalog });
  return { logPath, stagedLog, stagedDatabase, throughSeq: cut, keptTurnIds: turns.kept,
    // Newest first, like Codex thread/rollback receipts.
    removedTurnIds: turns.removed.slice().reverse(), filesRestored: false };
}

export async function forkCopy(config) {
  for (const key of ['database', 'sessionsRoot', 'cwd', 'targetSessionsRoot', 'targetDatabase', 'installRoot']) absolute(config[key], key);
  nonEmpty(config.sessionId, 'sessionId');
  nonEmpty(config.newSessionId, 'newSessionId');
  check(config.afterTurnId == null || (typeof config.afterTurnId === 'string' && config.afterTurnId.length > 0), 'invalid_request', 'afterTurnId must be a non-empty string');
  const { sessionId } = config;
  let through = null, throughSeq = null, selection, turnInputs = [];
  if (existsSync(config.database)) {
    // The source runtime may be running: a read-only view of committed facts only.
    const store = openBound(config.database, sessionId, config.cwd, true);
    try {
      const turns = orderedTurnIds(store);
      if (config.afterTurnId != null) {
        check(turns.includes(config.afterTurnId), 'turn_not_found', `turn ${config.afterTurnId} is not in this agent's history`);
        throughSeq = turnEndSeq(store, sessionId, nativeTurnOf(sessionId, config.afterTurnId));
        check(throughSeq !== null, 'turn_in_progress', 'the turn is still running; fork after it completed or pick an earlier turn');
        through = config.afterTurnId;
      } else {
        for (const id of turns.slice().reverse()) {
          const seq = turnEndSeq(store, sessionId, nativeTurnOf(sessionId, id));
          if (seq !== null) { through = id; throughSeq = seq; break; }
        }
      }
      selection = store.latest('dsh/launchSelection');
      if (through !== null) turnInputs = keptTurnInputs(store, sessionId, config.newSessionId, turns.slice(0, turns.indexOf(through) + 1));
    } finally { store.close(); }
  } else check(config.afterTurnId == null, 'turn_not_found', `turn ${config.afterTurnId} is not in this agent's history`);
  // No completed turn: the child starts a fresh native session (nothing to copy).
  if (through === null) return { forkedThroughTurnId: null, nativeSessionId: null, historyBytes: 0, events: 0 };
  const catalog = await loadNativeCatalog(join(config.installRoot, 'package.json'));
  const copy = await writeNativeSessionCopy({ sessionsRoot: config.sessionsRoot, sourceSessionId: sessionId, newSessionId: config.newSessionId,
    throughSeq, catalog, targetSessionsRoot: config.targetSessionsRoot });
  await writeBoundTimeline(config.targetDatabase, { sessionId: config.newSessionId, cwd: config.cwd, selection, turnInputs, logPath: copy.logPath, catalog });
  return { forkedThroughTurnId: through, nativeSessionId: config.newSessionId, throughSeq, events: copy.events,
    historyBytes: statSync(copy.logPath).size };
}

export async function run(config) {
  check(config !== null && typeof config === 'object' && !Array.isArray(config), 'invalid_request', 'config must be an object');
  if (config.op === 'revertPrepare') return revertPrepare(config);
  if (config.op === 'forkCopy') return forkCopy(config);
  throw fail('invalid_request', 'unknown session operation');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  let output;
  try {
    const file = process.env.DSH_CUTEX_SESSION_OPS_CONFIG;
    check(file && isAbsolute(file), 'invalid_request', 'explicit absolute session operation config required');
    output = { ok: true, ...(await run(JSON.parse(readFileSync(file, 'utf8')))) };
  } catch (error) {
    output = { ok: false, code: error.opsCode ?? 'session_operation_failed', message: String(error?.message ?? error).slice(0, 1000) };
    process.exitCode = 1;
  }
  process.stdout.write(JSON.stringify(output) + '\n');
}
