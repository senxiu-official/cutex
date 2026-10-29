// One-shot structural operations on a Kimi agent's private state, for cutex
// `cutex/session/history/revert` (agent stopped) and `cutex/session/fork` (source
// may run). Same contract as the DSH adapter's session-ops.mjs, so cutex's
// generic ACP structure code (`dsh_structure.rs`) drives both.
//
// Kimi Code 2.3.3 has ACP `session/fork` (whole session, no turn boundary) and no
// ACP undo. Both operations therefore work on Kimi's own append-only session log
// (`agents/main/wire.jsonl`) with the agent stopped (revert) or read-only (fork):
//
// - revertPrepare: stage the main log cut just before the `turn.prompt` of the
//   first removed turn (Kimi rebuilds its context from this log on resume, so
//   the removed turns are gone for the model too), plus a fresh timeline with
//   the kept turns. cutex swaps both in while the runtime is stopped. Files in
//   the workspace are never restored.
// - forkCopy: copy the session directory into the child agent's own
//   KIMI_CODE_HOME under a new Kimi session id, its main log cut after the last
//   kept turn, plus the child's timeline with the kept turns (new turn ids).
//
// Usage: KIMI_CUTEX_SESSION_OPS_CONFIG=<absolute JSON file> node session-ops.mjs
// (DSH_CUTEX_SESSION_OPS_CONFIG is accepted too.) Prints exactly one JSON line:
// {"ok":true,...} or {"ok":false,"code","message"}.
import { readFileSync, existsSync, statSync, writeFileSync, mkdirSync, cpSync } from 'node:fs';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { TimelineStore, TURN_BINDING, TURN_INPUTS } from './store.mjs';
import { secretRedactor, sourceFromEnv } from './redaction.mjs';
import { sessionDir, wireFiles, MAIN_AGENT } from './wire.mjs';

const fail = (code, message) => Object.assign(new Error(message), { opsCode: code });
const check = (condition, code, message) => { if (!condition) throw fail(code, message); };
const absolute = (value, name) => check(typeof value === 'string' && isAbsolute(value), 'invalid_request', `${name} must be an absolute path`);
const nonEmpty = (value, name) => check(typeof value === 'string' && value.length > 0, 'invalid_request', `${name} required`);

function openBound(database, sessionId, cwd, readOnly) {
  check(existsSync(database), 'history_unavailable', 'the agent has no timeline yet');
  const store = new TimelineStore(database, { readOnly });
  const binding = store.binding();
  if (!binding || binding.sessionId !== sessionId || resolve(binding.cwd) !== resolve(cwd)) {
    store.close();
    throw fail('history_binding_mismatch', 'the timeline is not bound to this Kimi session');
  }
  return store;
}

/** Byte offset of the main log line that starts Kimi turn `kimiTurnId` (its `turn.prompt`), or null. */
function promptOffset(text, kimiTurnId) {
  let offset = 0;
  for (const line of text.split('\n')) {
    if (line.includes('"turn.prompt"')) {
      try { const record = JSON.parse(line); if (record.type === 'turn.prompt' && record.turnId === kimiTurnId) return offset; } catch {}
    }
    offset += Buffer.byteLength(line) + 1;
  }
  return null;
}

/** The Kimi turn number of the first of `turnIds` Kimi recorded (a prompt Kimi refused has none). */
function firstKimiTurn(store, turnIds) {
  for (const id of turnIds) {
    const binding = store.turnFact(id, TURN_BINDING);
    if (Number.isSafeInteger(binding?.kimiTurnId)) return binding.kimiTurnId;
  }
  return null;
}

/** The main log text cut before Kimi turn `cutTurn` (null: nothing to cut). */
function cutLog(logPath, cutTurn) {
  const text = readFileSync(logPath, 'utf8');
  if (cutTurn === null) return text.endsWith('\n') || !text ? text : text.slice(0, text.lastIndexOf('\n') + 1);
  const offset = promptOffset(text, cutTurn);
  check(offset !== null, 'native_log_behind', `Kimi's session log has no turn ${cutTurn}; it ends before the boundary`);
  return Buffer.from(text).subarray(0, offset).toString('utf8');
}

/**
 * A fresh timeline bound to `sessionId` with the events of `turnIds` (in
 * order) and the adapter facts that must survive: the model selection, the
 * turns' bindings and image inputs, and the Kimi log offsets the copy starts at.
 * `rewrite(event)` re-keys copied events (fork).
 */
function writeTimeline(path, { source, sessionId, cwd, turnIds, offsets, usage, rewrite = event => event }) {
  check(!existsSync(path), 'invalid_request', 'target timeline already exists');
  const redact = secretRedactor(sourceFromEnv());
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const store = new TimelineStore(path);
  try {
    store.bind(sessionId, cwd);
    const events = [];
    const selection = source.latest('kimi/launchSelection');
    if (selection) events.push(rewrite({ method: selection.method, params: selection.params }));
    if (usage) { const last = source.latest('kimi/tokenUsage'); if (last) events.push({ method: last.method, params: last.params }); }
    for (const id of turnIds) {
      for (const method of [TURN_BINDING, TURN_INPUTS]) {
        const params = source.turnFact(id, method);
        if (params) events.push(rewrite({ method, params }));
      }
      for (const { seq: _seq, ...event } of source.turnEvents(id)) events.push(rewrite(event));
    }
    store.appendAll(events.map(redact));
    store.setMeta('wireOffsets', JSON.stringify(offsets));
  } finally { store.close(); }
}

/** Every completed turn id in order; `through` must be complete. */
function orderedTurns(store) { return store.turnIds(); }

export function revertPrepare(config) {
  for (const key of ['database', 'sessionsRoot', 'cwd', 'stagingDirectory']) absolute(config[key], key);
  nonEmpty(config.sessionId, 'sessionId');
  nonEmpty(config.beforeTurnId, 'beforeTurnId');
  const { sessionId } = config;
  const store = openBound(config.database, sessionId, config.cwd, false);
  try {
    const turns = orderedTurns(store);
    const index = turns.indexOf(config.beforeTurnId);
    check(index >= 0, 'turn_not_found', `turn ${config.beforeTurnId} is not in this agent's history`);
    const kept = turns.slice(0, index), removed = turns.slice(index);
    if (kept.length) check(store.turnCompleted(kept.at(-1)), 'turn_in_progress', `turn ${kept.at(-1)} has no end; revert before it instead`);
    const dir = sessionDir(dirname(config.sessionsRoot), sessionId);
    check(dir && dir.startsWith(config.sessionsRoot), 'history_unavailable', "Kimi's session log is missing");
    const logPath = join(dir, 'agents', MAIN_AGENT, 'wire.jsonl');
    check(existsSync(logPath), 'history_unavailable', "Kimi's main session log is missing");
    const staged = cutLog(logPath, firstKimiTurn(store, removed));
    mkdirSync(config.stagingDirectory, { recursive: true, mode: 0o700 });
    const stagedLog = join(config.stagingDirectory, basename(logPath));
    writeFileSync(stagedLog, staged, { flag: 'wx', mode: 0o600, flush: true });
    const offsets = Object.fromEntries(wireFiles(dir).map(({ agent, path }) => [agent, agent === MAIN_AGENT ? Buffer.byteLength(staged) : statSync(path).size]));
    const stagedDatabase = join(config.stagingDirectory, 'timeline.sqlite');
    writeTimeline(stagedDatabase, { source: store, sessionId, cwd: config.cwd, turnIds: kept, offsets, usage: true });
    return { logPath, stagedLog, stagedDatabase, throughSeq: null, keptTurnIds: kept, removedTurnIds: removed.slice().reverse(), filesRestored: false };
  } finally { store.close(); }
}

export function forkCopy(config) {
  for (const key of ['database', 'sessionsRoot', 'cwd', 'targetSessionsRoot', 'targetDatabase']) absolute(config[key], key);
  nonEmpty(config.sessionId, 'sessionId');
  nonEmpty(config.newSessionId, 'newSessionId');
  check(config.afterTurnId == null || (typeof config.afterTurnId === 'string' && config.afterTurnId.length > 0), 'invalid_request', 'afterTurnId must be a non-empty string');
  const { sessionId } = config;
  if (!existsSync(config.database)) {
    check(config.afterTurnId == null, 'turn_not_found', `turn ${config.afterTurnId} is not in this agent's history`);
    return { forkedThroughTurnId: null, nativeSessionId: null, historyBytes: 0, events: 0 };
  }
  // The source may be running: a read-only view of what it recorded.
  const store = openBound(config.database, sessionId, config.cwd, true);
  try {
    const turns = orderedTurns(store);
    let through = null;
    if (config.afterTurnId != null) {
      check(turns.includes(config.afterTurnId), 'turn_not_found', `turn ${config.afterTurnId} is not in this agent's history`);
      check(store.turnCompleted(config.afterTurnId), 'turn_in_progress', 'the turn is still running; fork after it completed or pick an earlier turn');
      through = config.afterTurnId;
    } else through = turns.slice().reverse().find(id => store.turnCompleted(id)) ?? null;
    if (through === null) return { forkedThroughTurnId: null, nativeSessionId: null, historyBytes: 0, events: 0 };
    const kept = turns.slice(0, turns.indexOf(through) + 1);
    const dir = sessionDir(dirname(config.sessionsRoot), sessionId);
    check(dir, 'history_unavailable', "Kimi's session directory is missing");
    const newId = config.newSessionId.startsWith('session_') ? config.newSessionId : `session_${config.newSessionId}`;
    const workDir = basename(dirname(dir));
    const target = join(config.targetSessionsRoot, workDir, newId);
    check(!existsSync(target), 'invalid_request', 'target Kimi session already exists');
    mkdirSync(dirname(target), { recursive: true, mode: 0o700 });
    // Everything Kimi keeps for the session (state, subagent logs, tasks), then the main log cut.
    cpSync(dir, target, { recursive: true, errorOnExist: true, filter: source => !basename(source).endsWith('.lock') });
    const main = join(target, 'agents', MAIN_AGENT, 'wire.jsonl');
    const later = turns.slice(turns.indexOf(through) + 1);
    const text = cutLog(join(dir, 'agents', MAIN_AGENT, 'wire.jsonl'), firstKimiTurn(store, later)).split(sessionId).join(newId);
    writeFileSync(main, text, { mode: 0o600 });
    const state = join(target, 'state.json');
    if (existsSync(state)) {
      const meta = JSON.parse(readFileSync(state, 'utf8'));
      writeFileSync(state, JSON.stringify({ ...JSON.parse(JSON.stringify(meta).split(sessionId).join(newId)), id: newId, forkedFrom: sessionId }), { mode: 0o600 });
    }
    const offsets = Object.fromEntries(wireFiles(target).map(({ agent, path }) => [agent, statSync(path).size]));
    // The child's own turn and item ids.
    const ids = new Map(kept.map(id => [id, randomUUID()]));
    const swap = value => {
      if (typeof value === 'string') {
        if (value === sessionId) return newId;
        for (const [from, to] of ids) if (value === from || value.startsWith(`${from}:`)) return to + value.slice(from.length);
        return value;
      }
      if (Array.isArray(value)) return value.map(swap);
      if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, swap(entry)]));
      return value;
    };
    writeTimeline(config.targetDatabase, { source: store, sessionId: newId, cwd: config.cwd, turnIds: kept, offsets, usage: false,
      rewrite: event => ({ method: event.method, params: swap(event.params) }) });
    return { forkedThroughTurnId: through, nativeSessionId: newId, throughSeq: null, events: kept.length, historyBytes: Buffer.byteLength(text) };
  } finally { store.close(); }
}

export function run(config) {
  check(config !== null && typeof config === 'object' && !Array.isArray(config), 'invalid_request', 'config must be an object');
  if (config.op === 'revertPrepare') return revertPrepare(config);
  if (config.op === 'forkCopy') return forkCopy(config);
  throw fail('invalid_request', 'unknown session operation');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  let output;
  try {
    const file = process.env.KIMI_CUTEX_SESSION_OPS_CONFIG ?? process.env.DSH_CUTEX_SESSION_OPS_CONFIG;
    check(file && isAbsolute(file), 'invalid_request', 'explicit absolute session operation config required');
    output = { ok: true, ...run(JSON.parse(readFileSync(file, 'utf8'))) };
  } catch (error) {
    output = { ok: false, code: error.opsCode ?? 'session_operation_failed', message: String(error?.message ?? error).slice(0, 1000) };
    process.exitCode = 1;
  }
  process.stdout.write(JSON.stringify(output) + '\n');
}
