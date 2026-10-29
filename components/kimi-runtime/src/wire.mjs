import { openSync, readSync, closeSync, statSync, readdirSync, existsSync } from 'node:fs';
import { join } from 'node:path';

/**
 * Kimi Code's own session log (`<KIMI_CODE_HOME>/sessions/<workDir>/<session>/
 * agents/<agent>/wire.jsonl`, wire protocol 1.5, append-only JSON lines). ACP
 * does not carry billing token counts, a failed turn's error or Kimi's turn
 * numbers; this log does. The adapter tails it (main agent and subagents)
 * from saved byte offsets, so a restart never counts a record twice.
 */
export const MAIN_AGENT = 'main';

/** The session directory of `sessionId` under `kimiHome`, or null. */
export function sessionDir(kimiHome, sessionId) {
  const root = join(kimiHome, 'sessions');
  if (!sessionId || !existsSync(root)) return null;
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (!entry.isDirectory() || entry.name.startsWith('.')) continue;
    const dir = join(root, entry.name, sessionId);
    if (existsSync(join(dir, 'agents'))) return dir;
  }
  return null;
}

/** `wire.jsonl` of every agent of a session: `[{agent, path}]`, main first. */
export function wireFiles(dir) {
  if (!dir || !existsSync(join(dir, 'agents'))) return [];
  const agents = readdirSync(join(dir, 'agents'), { withFileTypes: true }).filter(entry => entry.isDirectory()).map(entry => entry.name)
    .sort((a, b) => (a === MAIN_AGENT ? -1 : b === MAIN_AGENT ? 1 : a.localeCompare(b)));
  return agents.map(agent => ({ agent, path: join(dir, 'agents', agent, 'wire.jsonl') })).filter(file => existsSync(file.path));
}

/** Complete JSON lines of `path` after byte `offset`: `{records, offset}` (a partial last line waits). */
export function readFrom(path, offset = 0, maxBytes = 8 * 1024 * 1024) {
  let size;
  try { size = statSync(path).size; } catch { return { records: [], offset }; }
  if (size < offset) offset = 0; // replaced (revert / fork copy): read again from the start
  if (size === offset) return { records: [], offset };
  const length = Math.min(size - offset, maxBytes);
  const buffer = Buffer.alloc(length);
  const fd = openSync(path, 'r');
  try { readSync(fd, buffer, 0, length, offset); } finally { closeSync(fd); }
  const last = buffer.lastIndexOf(10);
  if (last < 0) return { records: [], offset };
  const records = [];
  for (const line of buffer.subarray(0, last).toString('utf8').split('\n')) {
    if (!line.trim()) continue;
    try { records.push(JSON.parse(line)); } catch { /* a damaged line is skipped, never fatal */ }
  }
  return { records, offset: offset + last + 1 };
}

/** One `usage.record` as cute-codex token usage (input includes cache reads and writes). */
export function usageOf(record) {
  const usage = record?.usage;
  if (record?.type !== 'usage.record' || !usage || typeof usage !== 'object') return null;
  const count = value => Number.isSafeInteger(value) && value >= 0 ? value : 0;
  const cacheRead = count(usage.inputCacheRead), cacheWrite = count(usage.inputCacheCreation), output = count(usage.output);
  const input = count(usage.inputOther) + cacheRead + cacheWrite;
  const last = { totalTokens: input + output, inputTokens: input, cachedInputTokens: cacheRead, cacheWriteInputTokens: cacheWrite, outputTokens: output, reasoningOutputTokens: 0 };
  return last.totalTokens > 0 ? last : null;
}

export const addUsage = (total = {}, last) => Object.fromEntries(Object.entries(last).map(([key, value]) => [key, (Number.isSafeInteger(total[key]) ? total[key] : 0) + value]));

/**
 * Tails every agent log of one session. `offsets` is `{[agent]: byte}`;
 * `visit(agent, record)` sees each new record once, in file order.
 */
export class WireTail {
  constructor({ kimiHome, sessionId, offsets = {} }) {
    this.kimiHome = kimiHome;
    this.sessionId = sessionId;
    this.offsets = { ...offsets };
  }
  poll(visit) {
    this.dir ??= sessionDir(this.kimiHome, this.sessionId);
    let changed = false;
    for (const { agent, path } of wireFiles(this.dir)) {
      for (;;) {
        const { records, offset } = readFrom(path, this.offsets[agent] ?? 0);
        if (offset === (this.offsets[agent] ?? 0) && !records.length) break;
        this.offsets[agent] = offset;
        changed = true;
        for (const record of records) visit(agent, record);
        if (!records.length) break;
      }
    }
    return changed;
  }
}

/** Main-agent records of a session log (offline: revert / fork). */
export function readAll(path) {
  const all = [];
  let offset = 0;
  for (;;) {
    const result = readFrom(path, offset);
    if (result.offset === offset) break;
    all.push(...result.records);
    offset = result.offset;
  }
  return all;
}
