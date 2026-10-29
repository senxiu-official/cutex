/**
 * Kimi Code tool calls as the Codex v2 thread items cutex frontends already
 * render (presentation only; Kimi decides, runs and records every call). The
 * input is Kimi's ACP stream (`tool_call` / `tool_call_update`):
 *
 * - `Bash` -> `commandExecution` (command, output, exit code, cwd)
 * - `Read` / `Grep` / `Glob` -> `commandExecution` whose `commandActions` say
 *   read / search / listFiles (as the Claude and DSH adapters show them)
 * - `Edit` / `Write` -> `fileChange` (Kimi's ACP diff block when present,
 *   else the call arguments)
 * - `WebSearch` / `FetchURL` -> `webSearch` (action search / openPage)
 * - `mcp__<server>__<tool>` -> `mcpToolCall` (cutex tools show as `cutex.<tool>`)
 * - `TodoList` -> no timeline item (its ACP `plan` update becomes `turn/plan/updated`)
 * - anything else -> `dynamicToolCall` in namespace `kimi`
 *
 * ACP gives the tool's description as the title once the call starts, and
 * its name only while the arguments stream; `toolName` recovers the name.
 */
export const OUTPUT_BYTES = 65536;
export const PREVIEW_BYTES = 16384;
const DIFF_CELLS = 1_000_000;

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const string = value => typeof value === 'string' ? value : undefined;
const basename = path => path.split(/[\\/]/).filter(Boolean).pop() ?? path;

export const KNOWN_TOOLS = new Set(['Bash', 'Read', 'Grep', 'Glob', 'Edit', 'Write', 'WebSearch', 'FetchURL', 'WebFetch', 'TodoList',
  'Agent', 'AgentSwarm', 'AskUserQuestion', 'NotifyUser', 'Skill', 'ReadMediaFile', 'TaskList', 'TaskOutput', 'TaskStop', 'WaitFor',
  'CronCreate', 'CronList', 'CronDelete', 'EnterPlanMode', 'ExitPlanMode', 'CreateGoal', 'Think', 'Terminal']);
export const HIDDEN_TOOLS = new Set(['TodoList']);
const COMMAND_TOOLS = new Set(['Bash', 'Terminal', 'Read', 'Grep', 'Glob']);
const EDIT_TOOLS = new Set(['Edit', 'Write']);
const WEB_TOOLS = new Set(['WebSearch', 'FetchURL', 'WebFetch']);

/** `mcp__<server>__<tool>` -> [server, tool]. */
export function mcpName(name) {
  const match = /^mcp__(.+?)__(.+)$/.exec(typeof name === 'string' ? name : '');
  return match ? [match[1], match[2]] : null;
}

/** A title that is a tool's own name (not its description). */
export const isToolName = title => typeof title === 'string' && (KNOWN_TOOLS.has(title) || mcpName(title) !== null);

/** The tool's name from what ACP showed: a name-like title, else its kind and arguments. */
export function toolName({ name, title, kind, rawInput }) {
  if (isToolName(name)) return name;
  if (isToolName(title)) return title;
  const args = object(rawInput) ? rawInput : {};
  if (kind === 'execute') return 'Bash';
  if (kind === 'edit') return typeof args.content === 'string' && args.old_string === undefined ? 'Write' : 'Edit';
  if (kind === 'fetch') return typeof args.url === 'string' ? 'FetchURL' : 'WebSearch';
  if (kind === 'read') {
    if (typeof args.pattern !== 'string') return 'Read';
    return ['output_mode', 'glob', 'type', 'multiline'].some(key => key in args) || !/[*?[\]{}]/.test(args.pattern) ? 'Grep' : 'Glob';
  }
  if (Array.isArray(args.todos) || Array.isArray(args.items)) return 'TodoList';
  return typeof title === 'string' && title ? title : 'tool';
}

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
const acpDiffs = content => Array.isArray(content) ? content.filter(entry => object(entry) && entry.type === 'diff' && typeof entry.newText === 'string') : [];
const blocksText = blocks => blocks.filter(block => block.type === 'text' && typeof block.text === 'string').map(block => block.text).join('\n');

/** Codex `update_plan` steps from an ACP `plan` update. */
export function planSteps(entries) {
  if (!Array.isArray(entries)) return null;
  const status = value => ({ pending: 'pending', in_progress: 'inProgress', completed: 'completed' })[value] ?? 'pending';
  return entries.filter(entry => object(entry) && typeof entry.content === 'string').map(entry => ({ step: entry.content, status: status(entry.status) }));
}

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
export function hunk(before, after) {
  const body = lineDiff(lines(before), lines(after));
  return body.length ? `@@\n${body.join('\n')}\n` : '';
}

const UPDATE = Object.freeze({ type: 'update', move_path: null });
function proposedChange(name, args, cwd) {
  const raw = string(args.path) ?? string(args.file_path) ?? '';
  const path = raw && cwd && !/^([\\/]|[A-Za-z]:[\\/])/.test(raw) ? `${cwd.replace(/[\\/]+$/, '')}/${raw}` : raw;
  if (name === 'Write') return { path, kind: { type: 'add' }, diff: string(args.content) ?? '' };
  return { path, kind: { ...UPDATE }, diff: hunk(string(args.old_string) ?? '', string(args.new_string) ?? '') };
}
function diffChanges(diffs, fallback) {
  return diffs.map(diff => diff.oldText == null
    ? { path: string(diff.path) ?? fallback.path, kind: { type: 'add' }, diff: diff.newText }
    : { path: string(diff.path) ?? fallback.path, kind: { ...UPDATE }, diff: hunk(diff.oldText, diff.newText) });
}

function commandSpec(name, args) {
  if (name === 'Bash' || name === 'Terminal') {
    const command = string(args.command) ?? '';
    return { command, actions: [{ type: 'unknown', command }] };
  }
  if (name === 'Read') {
    const path = string(args.path) ?? string(args.file_path) ?? '';
    const command = `Read ${path}`;
    return { command, actions: [{ type: 'read', command, name: basename(path), path }] };
  }
  const path = string(args.path) ?? null;
  if (name === 'Grep') {
    const pattern = string(args.pattern) ?? '';
    const command = `Grep ${JSON.stringify(pattern)}${path ? ` in ${path}` : ''}${string(args.glob) ? ` (${args.glob})` : ''}`;
    return { command, actions: [{ type: 'search', command, query: pattern, path }] };
  }
  const pattern = string(args.pattern) ?? '';
  const command = `Glob ${pattern}${path ? ` in ${path}` : ''}`;
  return { command, actions: [{ type: 'listFiles', command, path }] };
}

/** A Bash result's exit status (Kimi reports failures as `Command failed with exit code: N.`). */
export function bashOutcome(text, isError) {
  const exit = /(?:exit code:?|exited with code)\s*(-?\d+)/i.exec(text);
  if (exit) return { exitCode: Number(exit[1]), failed: isError || Number(exit[1]) !== 0 };
  return { exitCode: isError ? null : 0, failed: isError };
}

function webSpec(name, args) {
  if (name === 'FetchURL' || name === 'WebFetch') {
    const url = string(args.url) ?? '';
    return { query: url, action: { type: 'openPage', url } };
  }
  const query = string(args.query) ?? '';
  return { query, action: { type: 'search', query, queries: query ? [query] : [] } };
}

function contentItems(blocks) {
  return blocks.flatMap(block => {
    if (block.type === 'text' && typeof block.text === 'string') return [{ type: 'inputText', text: block.text }];
    if (block.type === 'image' && typeof block.data === 'string' && typeof block.mimeType === 'string') return [{ type: 'inputImage', imageUrl: `data:${block.mimeType};base64,${block.data}` }];
    return [];
  });
}

/** The item a started call shows, or null for a hidden tool. */
export function toolStartItem(name, args, id, { cwd, content } = {}) {
  if (HIDDEN_TOOLS.has(name)) return null;
  const input = object(args) ? args : {};
  if (COMMAND_TOOLS.has(name)) {
    const { command, actions } = commandSpec(name, input);
    return { type: 'commandExecution', id, command, cwd: name === 'Bash' ? string(input.cwd) ?? cwd ?? null : null,
      processId: null, source: 'agent', status: 'inProgress', commandActions: actions, aggregatedOutput: null,
      exitCode: null, durationMs: null, pluginId: null, scriptPath: null };
  }
  if (EDIT_TOOLS.has(name)) {
    const proposed = proposedChange(name, input, cwd);
    const diffs = acpDiffs(content);
    return { type: 'fileChange', id, changes: diffs.length ? diffChanges(diffs, proposed) : [proposed], status: 'inProgress' };
  }
  if (WEB_TOOLS.has(name)) return { type: 'webSearch', id, ...webSpec(name, input), results: null, status: 'inProgress' };
  const mcp = mcpName(name);
  if (mcp) return { type: 'mcpToolCall', id, server: mcp[0], tool: mcp[1], arguments: args ?? {}, status: 'inProgress',
    result: null, error: null, durationMs: null };
  return { type: 'dynamicToolCall', id, namespace: 'kimi', tool: name, arguments: args ?? {}, status: 'inProgress',
    contentItems: null, success: null, durationMs: null };
}

/**
 * The item of a finished call, or null for a hidden tool.
 * @param result `{ isError, content (ACP tool content), rawOutput, startContent (the started call's ACP content) }`.
 */
export function toolResultItem(name, args, id, { isError = false, content = [], rawOutput, startContent, cwd } = {}) {
  const item = toolStartItem(name, args, id, { cwd, content: startContent });
  if (!item) return null;
  const blocks = acpResultBlocks(content);
  const text = typeof rawOutput === 'string' ? rawOutput : blocksText(blocks);
  if (item.type === 'commandExecution') {
    if (name === 'Bash' || name === 'Terminal') {
      const outcome = bashOutcome(text, isError);
      return { ...item, status: outcome.failed ? 'failed' : 'completed', aggregatedOutput: bounded(text, OUTPUT_BYTES), exitCode: outcome.exitCode };
    }
    return { ...item, status: isError ? 'failed' : 'completed', aggregatedOutput: bounded(text, PREVIEW_BYTES), exitCode: isError ? 1 : 0 };
  }
  if (item.type === 'fileChange') {
    const diffs = acpDiffs(content);
    return { ...item, status: isError ? 'failed' : 'completed', ...(diffs.length && !isError ? { changes: diffChanges(diffs, item.changes[0]) } : {}) };
  }
  if (item.type === 'webSearch') {
    return { ...item, status: isError ? 'failed' : 'completed', ...(isError ? { error: { message: bounded(text, PREVIEW_BYTES) } } : {}) };
  }
  if (item.type === 'mcpToolCall') {
    return { ...item, status: isError ? 'failed' : 'completed', result: { content: blocks, structuredContent: null },
      error: isError ? { message: bounded(text, PREVIEW_BYTES) } : null };
  }
  return { ...item, status: isError ? 'failed' : 'completed', success: !isError, contentItems: contentItems(blocks) };
}
