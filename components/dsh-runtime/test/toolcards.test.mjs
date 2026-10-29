// DSH tool calls as the rich Codex v2 items cutex frontends render (toolItems.mjs):
// pure mapping, native-log projection (history), legacy history upgrade and the ACP path.
// Native payloads below are recorded from pinned DSH 0.2.1-rc.3 (paths shortened).
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { TimelineStore } from '../src/store.mjs';
import { loadNativeCatalog, reconcileNativeLog } from '../src/reconcile.mjs';
import { HistoryReader, projectTimeline } from '../src/history-reader.mjs';
import { DshRuntime } from '../src/runtime.mjs';
import { bashOutcome, hunk, lineDiff, toolResultItem, toolStartItem, upgradeLegacyToolItem } from '../src/toolItems.mjs';

const text = value => ({ type: 'text', text: value });

test('bash maps to commandExecution with output, exit code and cwd', () => {
  const args = { command: 'echo hi; ls; exit 3', description: 'Say hi' };
  assert.deepEqual(toolStartItem('bash', args, 'c1'), { type: 'commandExecution', id: 'c1', command: 'echo hi; ls; exit 3', cwd: null,
    processId: null, source: 'agent', status: 'inProgress', commandActions: [{ type: 'unknown', command: 'echo hi; ls; exit 3' }],
    aggregatedOutput: null, exitCode: null, durationMs: null, pluginId: null, scriptPath: null });
  const done = toolResultItem('bash', args, 'c1', { blocks: [text('hi\nnotes.txt\n[exit code: 3]')], meta: { cwd: '/work' }, durationMs: 50 });
  assert.equal(done.status, 'failed', 'a non-zero exit is a failed command, as under Codex');
  assert.equal(done.exitCode, 3);
  assert.equal(done.aggregatedOutput, 'hi\nnotes.txt\n');
  assert.equal(done.cwd, '/work');
  assert.equal(done.durationMs, 50);
  const ok = toolResultItem('bash', args, 'c1', { blocks: [text('(no output)')] });
  assert.deepEqual([ok.status, ok.exitCode, ok.aggregatedOutput], ['completed', 0, '']);
  const refused = toolResultItem('bash', { command: '' }, 'c2', { isError: true, blocks: [text('Error: invalid command: expected a non-empty string')] });
  assert.deepEqual([refused.status, refused.exitCode], ['failed', null]);
  assert.deepEqual(bashOutcome('started background job bash-a6e01fd8'), { exitCode: null, output: 'started background job bash-a6e01fd8', failed: false });
  assert.equal(bashOutcome('x\n[killed by signal: SIGTERM]').failed, true);
  assert.equal(bashOutcome('x\n[timed out after 1000ms]').exitCode, null);
});

test('read, grep and glob map to commandExecution with read / search / listFiles actions', () => {
  const read = toolResultItem('read', { file_path: 'notes.txt' }, 'r', {
    blocks: [text('<path>/work/notes.txt</path>\n<type>file</type>\n<content>\n1: alpha\n2: beta\n</content>')],
    meta: { path: '/work/notes.txt', offset: 1, lines: [{ number: 1, text: 'alpha' }, { number: 2, text: 'beta' }], totalLines: 2 } });
  assert.equal(read.command, 'Read notes.txt');
  assert.deepEqual(read.commandActions, [{ type: 'read', command: 'Read notes.txt', name: 'notes.txt', path: '/work/notes.txt' }]);
  assert.equal(read.aggregatedOutput, 'alpha\nbeta');
  assert.deepEqual([read.status, read.exitCode], ['completed', 0]);
  const grep = toolResultItem('grep', { pattern: 'BETA' }, 'g', { blocks: [text('Found 1 match\n\nnotes.txt\nLine 2: BETA')] });
  assert.deepEqual(grep.commandActions, [{ type: 'search', command: 'Grep "BETA"', query: 'BETA', path: null }]);
  assert.equal(grep.aggregatedOutput, 'Found 1 match\n\nnotes.txt\nLine 2: BETA');
  const glob = toolStartItem('glob', { pattern: '*.txt', path: 'src' }, 'l');
  assert.deepEqual(glob.commandActions, [{ type: 'listFiles', command: 'Glob *.txt in src', path: 'src' }]);
});

test('edit and write map to fileChange with unified hunks from the native diff meta', () => {
  const editArgs = { file_path: 'notes.txt', old_string: 'beta', new_string: 'BETA\nbeta2' };
  assert.deepEqual(toolStartItem('edit', editArgs, 'e'), { type: 'fileChange', id: 'e', status: 'inProgress',
    changes: [{ path: 'notes.txt', kind: { type: 'update', move_path: null }, diff: '@@\n-beta\n+BETA\n+beta2\n' }] });
  const edited = toolResultItem('edit', editArgs, 'e', { blocks: [text('The file /work/notes.txt has been updated successfully.')],
    meta: { path: '/work/notes.txt', diffs: [{ path: '/work/notes.txt', oldText: 'alpha\nbeta\ngamma', newText: 'alpha\nBETA\nbeta2\ngamma' }] } });
  assert.deepEqual(edited, { type: 'fileChange', id: 'e', status: 'completed',
    changes: [{ path: '/work/notes.txt', kind: { type: 'update', move_path: null }, diff: '@@\n alpha\n-beta\n+BETA\n+beta2\n gamma\n' }] });
  const created = toolResultItem('write', { file_path: 'new.txt', content: 'one\ntwo\n' }, 'w', { blocks: [text('Created file')],
    meta: { operation: 'create', path: '/work/new.txt', diffs: [] } });
  assert.deepEqual(created.changes, [{ path: '/work/new.txt', kind: { type: 'add' }, diff: 'one\ntwo\n' }]);
  const updated = toolResultItem('write', { file_path: 'notes.txt', content: 'alpha\nBETA\ngamma\ndelta\n' }, 'w2', {
    meta: { operation: 'update', path: '/work/notes.txt', diffs: [
      { path: '/work/notes.txt', oldText: 'alpha\nBETA\nbeta2\ngamma', newText: 'alpha\nBETA\ngamma\ndelta' },
      { path: '/work/notes.txt', oldText: null, newText: 'tail' }] } });
  assert.deepEqual(updated.changes, [{ path: '/work/notes.txt', kind: { type: 'update', move_path: null },
    diff: '@@\n alpha\n BETA\n-beta2\n gamma\n+delta\n@@\n+tail\n' }]);
  // A refused edit (e.g. FS_NOT_OBSERVED) keeps the proposed change and fails.
  const refused = toolResultItem('edit', editArgs, 'e', { isError: true, blocks: [text('Error: cannot modify')] });
  assert.equal(refused.status, 'failed');
  assert.equal(refused.changes[0].diff, '@@\n-beta\n+BETA\n+beta2\n');
  // Malformed meta falls back to the proposal; huge inputs never go quadratic.
  assert.equal(toolResultItem('edit', editArgs, 'e', { meta: { diffs: [{ path: 1 }] } }).changes[0].path, 'notes.txt');
  assert.equal(lineDiff(Array(2000).fill('a'), Array(2000).fill('b')).length, 4000);
  assert.equal(hunk('same', 'same'), '@@\n same\n');
});

test('web_search and web_fetch map to webSearch; MCP tools to their server; others stay generic', () => {
  assert.deepEqual(toolStartItem('web_search', { queries: ['cutex', 'deepseek'] }, 's'), { type: 'webSearch', id: 's', query: 'cutex | deepseek',
    action: { type: 'search', query: 'cutex | deepseek', queries: ['cutex', 'deepseek'] }, results: null, status: 'inProgress' });
  const found = toolResultItem('web_search', { queries: ['cutex'] }, 's', { blocks: [text('...')],
    meta: { sources: [{ url: 'https://example.com/', title: 'Example', extra: 1 }], truncated: false } });
  assert.deepEqual(found.results, [{ url: 'https://example.com/', title: 'Example' }]);
  const keyless = toolResultItem('web_search', { queries: ['cutex'] }, 's', { isError: true, blocks: [text('Error: DeepSeek search has no API key')] });
  assert.deepEqual([keyless.status, keyless.error.message], ['failed', 'Error: DeepSeek search has no API key']);
  assert.deepEqual(toolStartItem('web_fetch', { url: 'https://example.com/' }, 'f').action, { type: 'openPage', url: 'https://example.com/' });
  const mcp = toolResultItem('mcp__cutex__agent_list', {}, 'm', { blocks: [text('{"ok":true}')], durationMs: 3 });
  assert.deepEqual(mcp, { type: 'mcpToolCall', id: 'm', server: 'cutex', tool: 'agent_list', arguments: {}, status: 'completed',
    result: { content: [text('{"ok":true}')], structuredContent: null }, error: null, durationMs: 3 });
  const failed = toolResultItem('mcp__github__create_issue', { title: 't' }, 'm2', { isError: true, blocks: [text('boom')] });
  assert.deepEqual([failed.server, failed.tool, failed.status, failed.error], ['github', 'create_issue', 'failed', { message: 'boom' }]);
  const other = toolResultItem('job_output', { id: 'j' }, 'o', { blocks: [text('out'), { type: 'image', data: 'YQ==', mimeType: 'image/png' }] });
  assert.deepEqual(other, { type: 'dynamicToolCall', id: 'o', namespace: 'dsh', tool: 'job_output', arguments: { id: 'j' }, status: 'completed',
    contentItems: [{ type: 'inputText', text: 'out' }, { type: 'inputImage', imageUrl: 'data:image/png;base64,YQ==' }], success: true, durationMs: null });
  assert.equal(toolStartItem('todo_write', { todos: [] }, 't'), null, 'the checklist is turn/plan/updated, not a row');
});

test('pre-r265 generic history items read in today\'s shape', () => {
  const legacy = (tool, args, status, textValue) => ({ id: `id-${tool}`, type: 'mcpToolCall', server: 'dsh', tool, arguments: args, status,
    ...(textValue === undefined ? {} : { result: { content: [{ type: 'content', content: text(textValue) }] } }) });
  const upgraded = upgradeLegacyToolItem(legacy('bash', { command: 'ls' }, 'completed', 'a\nb'));
  assert.deepEqual([upgraded.type, upgraded.command, upgraded.aggregatedOutput, upgraded.exitCode], ['commandExecution', 'ls', 'a\nb', 0]);
  assert.equal(upgradeLegacyToolItem(legacy('edit', { file_path: 'x', old_string: 'a', new_string: 'b' }, 'inProgress')).type, 'fileChange');
  assert.deepEqual(upgradeLegacyToolItem(legacy('mcp__cutex__agent_list', {}, 'completed', '{}')).result.content, [text('{}')]);
  assert.equal(upgradeLegacyToolItem(legacy('todo_write', { todos: [] }, 'completed', 'ok')), null);
  const current = toolResultItem('mcp__dsh__x', {}, 'n', {});
  assert.equal(upgradeLegacyToolItem(current), current, 'a current item of an MCP server named dsh is left alone');
  const turnId = 't:turn:1';
  const [turn] = projectTimeline([
    { method: 'turn/started', params: { threadId: 't', turn: { id: turnId, status: 'inProgress', items: [] } } },
    { method: 'item/started', params: { threadId: 't', turnId, item: legacy('todo_write', { todos: [] }, 'inProgress') } },
    { method: 'item/completed', params: { threadId: 't', turnId, item: legacy('bash', { command: 'ls' }, 'completed', 'a') } },
    { method: 'turn/completed', params: { threadId: 't', turn: { id: turnId, status: 'completed', items: [legacy('bash', { command: 'ls' }, 'completed', 'a')] } } },
  ]);
  assert.deepEqual(turn.items.map(item => item.type), ['commandExecution']);
});

// --- Native projection through the production store and history reader ---------------------
const catalog = await loadNativeCatalog();
const sessionId = 'toolcards-native';
const event = (seq, type, data) => ({ seq, time: 1000 + seq * 10, type, data });
const call = (seq, callId, name, args) => event(seq, 'tool/call', { turn: 1, step: seq, callId, name, arguments: JSON.stringify(args) });
const result = (seq, callId, body, extra = {}) => event(seq, 'tool/result', { turn: 1, step: seq, message: { id: `result-${callId}`, role: 'tool',
  source: { kind: 'tool', callId }, toolCallId: callId, content: [text(body)], isError: extra.isError ?? false }, ...(extra.meta ? { meta: extra.meta } : {}),
  ...(extra.error ? { error: extra.error } : {}) });
const recorded = [
  event(0, 'turn/start', { turn: 1 }),
  event(1, 'user/message', { id: 'user-1', role: 'user', source: { kind: 'user' }, content: [text('do the work')] }),
  event(2, 'user/message', { id: 'context-1', role: 'user', content: [text('Current runtime context. ...')],
    source: { kind: 'runtime-context', form: 'snapshot', sections: [{ name: 'working-directory:current', text: 'Current working directory: "/work".' }] } }),
  call(3, 'call-3', 'bash', { command: 'echo hi; ls; exit 3', description: 'Say hi' }),
  result(4, 'call-3', 'hi\nnotes.txt\n[exit code: 3]', { meta: { cwd: '/work' } }),
  call(5, 'call-5', 'edit', { file_path: 'notes.txt', old_string: 'beta', new_string: 'BETA' }),
  result(6, 'call-5', 'The file /work/notes.txt has been updated successfully.', { meta: { path: '/work/notes.txt',
    diffs: [{ path: '/work/notes.txt', oldText: 'alpha\nbeta\ngamma', newText: 'alpha\nBETA\ngamma' }] } }),
  call(7, 'call-7', 'todo_write', { todos: [{ content: 'Step A', status: 'completed' }, { content: 'Step B', status: 'in_progress' }] }),
  event(8, 'todo/write', { todos: [{ content: 'Step A', status: 'completed' }, { content: 'Step B', status: 'in_progress' }] }),
  result(9, 'call-7', 'Updated todo list: 0 pending, 1 in progress, 1 completed.'),
  call(10, 'call-10', 'web_search', { queries: ['cutex deepseek'] }),
  result(11, 'call-10', 'Error: DeepSeek search has no API key', { isError: true, error: { name: 'WebError', code: 'WEB_PROVIDER_CREDENTIAL_MISSING' } }),
  event(12, 'assistant/message', { turn: 1, step: 12, message: { id: 'assistant-1', role: 'assistant', content: [text('done')] } }),
  event(13, 'turn/end', { turn: 1, reason: { kind: 'completed' } }),
];
const logText = events => [
  catalog.encodeCurrentHeader({ version: 4, id: sessionId, createdAt: 1, isSeeded: false, delegationDepth: 0 }, 0),
  ...events.map(entry => catalog.encodeCurrentEvent(entry)),
].map(row => JSON.stringify(row) + '\n').join('');

test('native log projection: bash, edit, checklist and web search as rich items in live events and thread/read', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'dsh-toolcards-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const logPath = join(dir, 'session.v4.jsonl');
  await writeFile(logPath, logText(recorded));
  const database = join(dir, 'history.sqlite');
  const store = new TimelineStore(database);
  const live = [];
  store.onNativeCommitted = events => live.push(...events);
  try {
    assert.equal((await reconcileNativeLog({ logPath, sessionId, catalog, store })).committed, recorded.length);
  } finally { store.close(); }
  const turnId = `${sessionId}:turn:1`;
  const started = live.filter(entry => entry.method === 'item/started').map(entry => entry.params.item.type);
  assert.deepEqual(started, ['commandExecution', 'fileChange', 'webSearch'], 'todo_write has no row');
  const plan = live.find(entry => entry.method === 'turn/plan/updated');
  assert.deepEqual(plan.params, { threadId: sessionId, turnId, explanation: null,
    plan: [{ step: 'Step A', status: 'completed' }, { step: 'Step B', status: 'inProgress' }] });
  const reader = new HistoryReader({ database, threadId: sessionId, cwd: '/work' });
  try {
    const { thread } = reader.handle('thread/read', { threadId: sessionId, includeTurns: true });
    const items = thread.turns[0].items;
    assert.deepEqual(items.map(item => item.type), ['userMessage', 'commandExecution', 'fileChange', 'webSearch', 'agentMessage'],
      'the runtime-context snapshot is not an owner message');
    const command = items[1];
    assert.deepEqual([command.command, command.aggregatedOutput, command.exitCode, command.status, command.cwd, command.durationMs],
      ['echo hi; ls; exit 3', 'hi\nnotes.txt\n', 3, 'failed', '/work', 10]);
    assert.deepEqual(items[2].changes, [{ path: '/work/notes.txt', kind: { type: 'update', move_path: null }, diff: '@@\n alpha\n-beta\n+BETA\n gamma\n' }]);
    assert.equal(items[2].status, 'completed');
    assert.deepEqual([items[3].query, items[3].status], ['cutex deepseek', 'failed']);
  } finally { reader.close(); }
});

// --- ACP fallback path (no native log): same items from DSH's generic tool updates ---------
test('ACP tool updates map to the same items; todo_write becomes turn/plan/updated', async t => {
  const cwd = mkdtempSync(join(tmpdir(), 'dsh-toolcards-acp-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const emitted = [];
  const runtime = new DshRuntime({ command: process.execPath, args: [fileURLToPath(new URL('./fake-acp.mjs', import.meta.url))], cwd,
    database: join(cwd, 'history.sqlite'), env: {}, emit: entry => emitted.push(entry) });
  try {
    await runtime.initialize();
    await runtime.prompt('toolcards');
    const completed = runtime.replay().filter(entry => entry.method === 'item/completed' && entry.params.item.type !== 'userMessage').map(entry => entry.params.item);
    const byId = Object.fromEntries(completed.map(item => [item.id, item]));
    assert.deepEqual([byId['bash-1'].type, byId['bash-1'].exitCode, byId['bash-1'].aggregatedOutput], ['commandExecution', 2, 'FAIL one\n']);
    assert.deepEqual([byId['edit-1'].type, byId['edit-1'].changes[0].diff], ['fileChange', '@@\n-x\n+y\n']);
    assert.deepEqual([byId['mcp-1'].type, byId['mcp-1'].server, byId['mcp-1'].tool], ['mcpToolCall', 'cutex', 'agent_list']);
    assert.deepEqual([byId['other-1'].type, byId['other-1'].status, byId['other-1'].success], ['dynamicToolCall', 'failed', false]);
    assert.equal(byId['todo-1'], undefined);
    const plan = runtime.replay().find(entry => entry.method === 'turn/plan/updated');
    assert.deepEqual(plan.params.plan, [{ step: 'First', status: 'completed' }, { step: 'Second', status: 'inProgress' }]);
    assert.ok(emitted.some(entry => entry.method === 'turn/plan/updated'), 'live clients get the checklist');
  } finally { await runtime.close(); }
});
