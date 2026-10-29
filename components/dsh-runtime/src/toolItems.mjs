/**
 * DSH tool calls as the Codex v2 thread items cutex frontends already render
 * (presentation only; DSH decides, runs and records every call). The same
 * functions serve the native-log projection (history and live) and the ACP
 * fallback path, so both show a call identically:
 *
 * - `bash` / `pwsh` -> `commandExecution` (command, output, exit code, cwd)
 * - `read` / `grep` / `glob` -> `commandExecution` whose `commandActions` say
 *   read / search / listFiles (as the Claude adapter shows Read/Grep/Glob)
 * - `edit` / `write` -> `fileChange` with unified hunks (DSH's result `meta`
 *   carries the applied contextual diff; the call arguments give the
 *   proposed change until the result lands)
 * - `web_search` / `web_fetch` -> `webSearch` (action search / openPage)
 * - `mcp__<server>__<tool>` -> `mcpToolCall` of that server (cutex tools show
 *   as `cutex.<tool>`, exactly as under cute-codex)
 * - `todo_write` -> no timeline item; its `todo/write` fact becomes
 *   `turn/plan/updated` (Codex `update_plan`, Claude `TodoWrite`)
 * - anything else -> generic `dynamicToolCall` in namespace `dsh`
 *
 * Pinned DSH 0.2.1-rc.3 sends only generic ACP tool updates (kind `other`, no
 * diff blocks or locations), so the mapping reads the tool name, arguments,
 * model-facing result and the native result `meta` instead.
 */

export const OUTPUT_BYTES = 65536;
export const PREVIEW_BYTES = 16384;
const DIFF_CELLS = 1_000_000;

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const string = value => typeof value === 'string' ? value : undefined;
const basename = path => path.split(/[\\/]/).filter(Boolean).pop() ?? path;

/** Tool calls that are not timeline rows (their effect is shown elsewhere). */
export const HIDDEN_TOOLS = new Set(['todo_write']);
const SHELL_TOOLS = new Set(['bash', 'pwsh']);
const COMMAND_TOOLS = new Set([...SHELL_TOOLS, 'read', 'grep', 'glob']);
const EDIT_TOOLS = new Set(['edit', 'write']);
const WEB_TOOLS = new Set(['web_search', 'web_fetch']);

/** The model's raw argument JSON (malformed JSON stays the raw string). */
export function parseArguments(raw) {
  if (typeof raw !== 'string') return raw ?? {};
  try { return JSON.parse(raw); } catch { return raw; }
}

/** `mcp__<server>__<tool>` -> [server, tool]. */
export function mcpName(name) {
  const match = /^mcp__(.+?)__(.+)$/.exec(typeof name === 'string' ? name : '');
  return match ? [match[1], match[2]] : null;
}

/** Cut text at `max` UTF-16 units, saying how much was left out. */
export function bounded(text, max) {
  if (typeof text !== 'string' || text.length <= max) return text;
  return `${text.slice(0, max)}\n[${text.length - max} more characters not shown]`;
}

/** ACP tool-call content (`{type:'content', content}` wrappers) as MCP content blocks. */
export function acpResultBlocks(content) {
  if (!Array.isArray(content)) return [];
  return content.flatMap(entry => {
    if (!object(entry)) return [];
    if (entry.type === 'content' && object(entry.content)) return [entry.content];
    if (entry.type === 'text' || entry.type === 'image') return [entry];
    return [];
  });
}

const blocksText = blocks => blocks.filter(block => block.type === 'text' && typeof block.text === 'string').map(block => block.text).join('\n');

/** Codex `update_plan` steps from a DSH todo list. */
export function todoPlan(todos) {
  if (!Array.isArray(todos)) return null;
  const status = value => ({ pending: 'pending', in_progress: 'inProgress', completed: 'completed' })[value] ?? 'pending';
  return todos.filter(todo => object(todo) && typeof todo.content === 'string').map(todo => ({ step: todo.content, status: status(todo.status) }));
}

/** `+`/`-`/` ` lines between two line lists (LCS; oversized inputs fall back to remove-then-add). */
export function lineDiff(before, after) {
  const n = before.length, m = after.length;
  if (n * m > DIFF_CELLS) return [...before.map(line => `-${line}`), ...after.map(line => `+${line}`)];
  const table = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) {
    table[i][j] = before[i] === after[j] ? table[i + 1][j + 1] + 1 : Math.max(table[i + 1][j], table[i][j + 1]);
  }
  const out = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (before[i] === after[j]) { out.push(` ${before[i]}`); i++; j++; }
    else if (table[i + 1][j] >= table[i][j + 1]) out.push(`-${before[i++]}`);
    else out.push(`+${after[j++]}`);
  }
  while (i < n) out.push(`-${before[i++]}`);
  while (j < m) out.push(`+${after[j++]}`);
  return out;
}

const lines = text => typeof text === 'string' && text.length ? text.replace(/\r\n/g, '\n').split('\n') : [];

/**
 * One bare unified hunk (`@@` without line numbers, as apply_patch writes;
 * DSH's diff meta has no line numbers) for a before/after text pair.
 */
export function hunk(before, after) {
  const body = lineDiff(lines(before), lines(after));
  return body.length ? `@@\n${body.join('\n')}\n` : '';
}

/** DSH `edit` / `write` result meta (`{path, operation?, diffs: [{path, oldText, newText}]}`). */
function diffMeta(meta) {
  if (!object(meta) || !Array.isArray(meta.diffs)) return null;
  const diffs = meta.diffs.filter(diff => object(diff) && (diff.oldText === null || typeof diff.oldText === 'string') && typeof diff.newText === 'string');
  if (diffs.length !== meta.diffs.length) return null;
  return { path: string(meta.path) ?? string(diffs[0]?.path), operation: meta.operation, diffs };
}

const UPDATE = Object.freeze({ type: 'update', move_path: null });

function proposedChange(name, args) {
  const path = string(args.file_path) ?? '';
  if (name === 'write') return { path, kind: { type: 'add' }, diff: string(args.content) ?? '' };
  return { path, kind: { ...UPDATE }, diff: hunk(string(args.old_string) ?? '', string(args.new_string) ?? '') };
}

function appliedChange(name, args, meta, proposed) {
  const parsed = diffMeta(meta);
  if (!parsed) return proposed;
  const path = parsed.path ?? proposed.path;
  if (name === 'write' && parsed.operation === 'create') return { path, kind: { type: 'add' }, diff: string(args.content) ?? '' };
  return { path, kind: { ...UPDATE }, diff: parsed.diffs.map(diff => hunk(diff.oldText ?? '', diff.newText)).join('') };
}

function commandSpec(name, args) {
  if (SHELL_TOOLS.has(name)) {
    const command = string(args.command) ?? '';
    return { command, actions: [{ type: 'unknown', command }] };
  }
  if (name === 'read') {
    const path = string(args.file_path) ?? '';
    const command = `Read ${path}`;
    return { command, actions: [{ type: 'read', command, name: basename(path), path }] };
  }
  const path = string(args.path) ?? null;
  if (name === 'grep') {
    const pattern = string(args.pattern) ?? '';
    const command = `Grep ${JSON.stringify(pattern)}${path ? ` in ${path}` : ''}${string(args.include) ? ` (${args.include})` : ''}`;
    return { command, actions: [{ type: 'search', command, query: pattern, path }] };
  }
  const pattern = string(args.pattern) ?? '';
  const command = `Glob ${pattern}${path ? ` in ${path}` : ''}`;
  return { command, actions: [{ type: 'listFiles', command, path }] };
}

/**
 * Exit status of a DSH bash/pwsh result from its model-facing text (DSH reports a
 * non-zero exit as a successful call with a trailing marker).
 */
export function bashOutcome(text) {
  const exit = /\[exit code: (-?\d+)\]\s*$/.exec(text);
  if (exit) return { exitCode: Number(exit[1]), output: text.slice(0, exit.index), failed: Number(exit[1]) !== 0 };
  if (/\[killed by signal: [^\]]*\]\s*$/.test(text) || /\[timed out after \d+ms\]\s*$/.test(text)) return { exitCode: null, output: text, failed: true };
  if (/^started background job \S+$/.test(text.trim()) || /\[still running after \d+ms; moved to background job /.test(text)) return { exitCode: null, output: text, failed: false };
  return { exitCode: 0, output: text === '(no output)' ? '' : text, failed: false };
}

function readOutput(meta, text) {
  if (object(meta) && Array.isArray(meta.lines) && meta.lines.every(line => object(line) && typeof line.text === 'string')) {
    return meta.lines.map(line => line.text).join('\n');
  }
  return text;
}

function webSpec(name, args) {
  if (name === 'web_fetch') {
    const url = string(args.url) ?? '';
    return { query: url, action: { type: 'openPage', url } };
  }
  const queries = Array.isArray(args.queries) ? args.queries.filter(query => typeof query === 'string') : [];
  const query = queries.join(' | ');
  return { query, action: { type: 'search', query, queries } };
}

const sources = meta => object(meta) && Array.isArray(meta.sources)
  ? meta.sources.filter(object).slice(0, 50).map(source => Object.fromEntries(['url', 'title', 'snippet', 'publishedAt']
    .filter(key => typeof source[key] === 'string').map(key => [key, bounded(source[key], 2000)])))
  : null;

function contentItems(blocks) {
  return blocks.flatMap(block => {
    if (block.type === 'text' && typeof block.text === 'string') return [{ type: 'inputText', text: block.text }];
    if (block.type === 'image' && typeof block.data === 'string' && typeof block.mimeType === 'string') return [{ type: 'inputImage', imageUrl: `data:${block.mimeType};base64,${block.data}` }];
    return [];
  });
}

/**
 * The item a started call shows, or null for a hidden tool.
 * @param name - DSH tool name; args - parsed arguments; id - call id.
 */
export function toolStartItem(name, args, id) {
  if (HIDDEN_TOOLS.has(name)) return null;
  const input = object(args) ? args : {};
  if (COMMAND_TOOLS.has(name)) {
    const { command, actions } = commandSpec(name, input);
    return { type: 'commandExecution', id, command, cwd: SHELL_TOOLS.has(name) ? string(input.workdir) ?? null : null,
      processId: null, source: 'agent', status: 'inProgress', commandActions: actions, aggregatedOutput: null,
      exitCode: null, durationMs: null, pluginId: null, scriptPath: null };
  }
  if (EDIT_TOOLS.has(name)) return { type: 'fileChange', id, changes: [proposedChange(name, input)], status: 'inProgress' };
  if (WEB_TOOLS.has(name)) return { type: 'webSearch', id, ...webSpec(name, input), results: null, status: 'inProgress' };
  const mcp = mcpName(name);
  if (mcp) return { type: 'mcpToolCall', id, server: mcp[0], tool: mcp[1], arguments: args ?? {}, status: 'inProgress',
    result: null, error: null, durationMs: null };
  return { type: 'dynamicToolCall', id, namespace: 'dsh', tool: name, arguments: args ?? {}, status: 'inProgress',
    contentItems: null, success: null, durationMs: null };
}

/**
 * The item of a finished call, or null for a hidden tool.
 * @param result - `{ isError, blocks (MCP-style content), meta (DSH result meta), durationMs }`.
 */
export function toolResultItem(name, args, id, { isError = false, blocks = [], meta, durationMs = null } = {}) {
  const item = toolStartItem(name, args, id);
  if (!item) return null;
  const input = object(args) ? args : {};
  const text = blocksText(blocks);
  const duration = Number.isSafeInteger(durationMs) && durationMs >= 0 ? durationMs : null;
  if (item.type === 'commandExecution') {
    if (SHELL_TOOLS.has(name)) {
      const outcome = isError ? { exitCode: null, output: text, failed: true } : bashOutcome(text);
      if (object(meta) && typeof meta.cwd === 'string') item.cwd = meta.cwd;
      return { ...item, status: outcome.failed ? 'failed' : 'completed', aggregatedOutput: bounded(outcome.output, OUTPUT_BYTES), exitCode: outcome.exitCode, durationMs: duration };
    }
    const output = name === 'read' && !isError ? readOutput(meta, text) : text;
    if (name === 'read' && !isError && object(meta) && typeof meta.path === 'string') item.commandActions = [{ ...item.commandActions[0], path: meta.path }];
    return { ...item, status: isError ? 'failed' : 'completed', aggregatedOutput: bounded(output, PREVIEW_BYTES), exitCode: isError ? 1 : 0, durationMs: duration };
  }
  if (item.type === 'fileChange') {
    return { ...item, status: isError ? 'failed' : 'completed', changes: isError ? item.changes : [appliedChange(name, input, meta, item.changes[0])] };
  }
  if (item.type === 'webSearch') {
    return { ...item, status: isError ? 'failed' : 'completed', results: isError ? null : sources(meta),
      ...(isError ? { error: { message: bounded(text, PREVIEW_BYTES) } } : {}) };
  }
  if (item.type === 'mcpToolCall') {
    return { ...item, status: isError ? 'failed' : 'completed', result: { content: blocks, structuredContent: null },
      error: isError ? { message: bounded(text, PREVIEW_BYTES) } : null, durationMs: duration };
  }
  return { ...item, status: isError ? 'failed' : 'completed', success: !isError, contentItems: contentItems(blocks), durationMs: duration };
}

/**
 * A timeline item stored before r265 tool cards (generic `mcpToolCall` of
 * server `dsh`, ACP-wrapped result) in today's shape; null when the tool is
 * hidden now (todo_write). Any other item is returned unchanged.
 */
export function upgradeLegacyToolItem(item) {
  // Today's mcpToolCall items always carry `error`; a legacy one never did.
  if (!object(item) || item.type !== 'mcpToolCall' || item.server !== 'dsh' || typeof item.tool !== 'string' || 'error' in item) return item;
  const args = item.arguments;
  if (item.status === 'inProgress' || !item.status) return toolStartItem(item.tool, args, item.id);
  return toolResultItem(item.tool, args, item.id, { isError: item.status === 'failed', blocks: acpResultBlocks(item.result?.content) });
}
