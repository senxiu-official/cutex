// Real `kimi acp` (pinned install) + this adapter + the offline mock: the
// app-server surface the cutex daemon uses. Skipped without the pinned install.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { writeFileSync, readFileSync, existsSync } from 'node:fs';
import { install, kimiMain, privateKimi, connect, poll, delay, READ_ONLY_HOOK } from './harness.mjs';

if (install) process.env.KIMI_INSTALL_ROOT ??= install;
const { startServer } = install ? await import('../src/server.mjs') : {};

async function adapter(t, kimi, extra = {}) {
  const threadId = extra.threadId ?? randomUUID();
  const config = {
    command: process.execPath, args: [kimiMain, 'acp'], cwd: kimi.dirs.cwd, env: kimi.env, kimiHome: kimi.dirs.kimiHome,
    database: join(kimi.dirs.state, 'timeline.sqlite'), readyFile: join(kimi.dirs.state, 'ready.json'), token: `tok-${randomUUID()}-${randomUUID()}`,
    threadId, runtimeId: `kimi.${randomUUID()}`, model: 'mock/k', liveness: { interval: 1000, probeTimeout: 1000 }, stopGrace: 3000, ...extra,
  };
  const server = await startServer(config);
  kimi.onClose(() => server.close());
  const client = await connect(server.endpoint, config.token);
  kimi.onClose(() => client.close());
  return { server, client, config, threadId };
}

const turnDone = (client, turnId) => poll(() => client.notifications.find(n => n.method === 'turn/completed' && n.params.turn.id === turnId), Boolean, `turn ${turnId} completed`, 60000);
const send = async (client, threadId, text, extra = {}) => (await client.request('turn/start', { threadId, input: [{ type: 'text', text }], ...extra })).turn.id;

test('a turn streams text, completes, and its history and token usage are kept', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'hello' });
  const { client, threadId } = await adapter(t, kimi);
  const started = await client.request('thread/start', {});
  assert.equal(started.modelProvider, 'kimi');
  assert.equal(started.model, 'mock/k');
  const turnId = await send(client, threadId, 'hello there');
  const done = await turnDone(client, turnId);
  assert.equal(done.params.turn.status, 'completed');
  const deltas = client.notifications.filter(n => n.method === 'item/agentMessage/delta' && n.params.turnId === turnId).map(n => n.params.delta).join('');
  assert.equal(deltas, 'echo: hello there');
  const read = await client.request('thread/read', { threadId, includeTurns: true });
  const turn = read.thread.turns.find(entry => entry.id === turnId);
  assert.deepEqual(turn.items.map(item => item.type), ['userMessage', 'agentMessage']);
  assert.equal(turn.items[1].text, 'echo: hello there');
  const usage = await poll(() => client.notifications.filter(n => n.method === 'thread/tokenUsage/updated'), list => list.length >= 1, 'token usage');
  assert.equal(usage.at(-1).params.tokenUsage.last.inputTokens, 100);
  assert.equal(usage.at(-1).params.tokenUsage.last.outputTokens, 10);
  assert.equal(usage.at(-1).params.turnId, turnId);
});

test('Bash waits for the owner; cutex tools, reads and in-workspace edits do not ask', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'approve' });
  const { client, threadId } = await adapter(t, kimi);
  await client.request('thread/start', {});
  writeFileSync(join(kimi.dirs.cwd, 'notes.txt'), 'alpha\nbeta\n');
  const steps = [{ name: 'Read', args: { path: 'notes.txt' } }, { name: 'Edit', args: { path: 'notes.txt', old_string: 'beta', new_string: 'gamma' } },
    { name: 'Bash', args: { command: 'echo shell-ok > shell.txt && echo done' } }];
  const turnId = await send(client, threadId, `STEPS ${JSON.stringify(steps)}`);
  const ask = await poll(() => client.requests.find(r => r.method === 'item/commandExecution/requestApproval'), Boolean, 'Bash approval');
  assert.match(ask.params.command, /echo shell-ok/);
  assert.equal(client.requests.length, 1, 'only Bash asked');
  // No clock: still pending after a while.
  await delay(1500);
  assert.equal(client.notifications.some(n => n.method === 'turn/completed' && n.params.turn.id === turnId), false);
  client.respond(ask.id, { decision: 'accept' });
  const done = await turnDone(client, turnId);
  assert.equal(done.params.turn.status, 'completed');
  assert.equal(readFileSync(join(kimi.dirs.cwd, 'notes.txt'), 'utf8'), 'alpha\ngamma\n');
  assert.ok(existsSync(join(kimi.dirs.cwd, 'shell.txt')));
  const read = await client.request('thread/read', { threadId, includeTurns: true });
  const items = read.thread.turns.find(turn => turn.id === turnId).items;
  const read1 = items.find(item => item.type === 'commandExecution' && item.commandActions[0].type === 'read');
  assert.ok(read1, JSON.stringify(items));
  const edit = items.find(item => item.type === 'fileChange');
  assert.equal(edit.status, 'completed');
  assert.match(edit.changes[0].diff, /-beta\n\+gamma/);
  const bash = items.find(item => item.type === 'commandExecution' && item.commandActions[0].type === 'unknown');
  assert.equal(bash.status, 'completed');
  assert.match(bash.aggregatedOutput, /done/);
});

test('read-only: Write, Edit and any shell command are refused before they run, nobody is asked; reads still work', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'readonly', config: READ_ONLY_HOOK });
  const { client, threadId } = await adapter(t, kimi, { sandboxReadOnly: true });
  await client.request('thread/start', {});
  writeFileSync(join(kimi.dirs.cwd, 'notes.txt'), 'alpha\n');
  const steps = [{ name: 'Write', args: { path: 'w.txt', content: 'x' } }, { name: 'Edit', args: { path: 'notes.txt', old_string: 'alpha', new_string: 'beta' } },
    { name: 'Bash', args: { command: 'echo b > b.txt' } }, { name: 'Bash', args: { command: 'cat notes.txt' } }, { name: 'Read', args: { path: 'notes.txt' } }];
  const turnId = await send(client, threadId, `STEPS ${JSON.stringify(steps)}`);
  const done = await turnDone(client, turnId);
  assert.equal(done.params.turn.status, 'completed');
  assert.equal(client.requests.length, 0, 'nothing asks the owner');
  assert.equal(existsSync(join(kimi.dirs.cwd, 'w.txt')), false);
  assert.equal(existsSync(join(kimi.dirs.cwd, 'b.txt')), false);
  assert.equal(readFileSync(join(kimi.dirs.cwd, 'notes.txt'), 'utf8'), 'alpha\n');
  const items = (await client.request('thread/read', { threadId, includeTurns: true })).thread.turns.find(turn => turn.id === turnId).items;
  const tools = items.filter(item => item.type === 'fileChange' || item.type === 'commandExecution');
  assert.deepEqual(tools.map(item => item.status), ['failed', 'failed', 'failed', 'failed', 'completed'], JSON.stringify(tools).slice(0, 3000));
  assert.match(tools[2].aggregatedOutput, /read-only/);
  // An ask that still reaches the adapter (a Kimi tool outside the hook's reach) is declined, never shown.
  const asked = await send(client, threadId, 'CALL Bash {"command":"echo y > y.txt"}');
  await turnDone(client, asked);
  assert.equal(client.requests.length, 0);
  assert.equal(existsSync(join(kimi.dirs.cwd, 'y.txt')), false);
});

test('read-only without the hook: the adapter still declines every ask (no owner prompt)', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'readonly-ask' });
  const { client, threadId } = await adapter(t, kimi, { sandboxReadOnly: true });
  await client.request('thread/start', {});
  const turnId = await send(client, threadId, 'CALL Bash {"command":"echo z > z.txt"}');
  await turnDone(client, turnId);
  assert.equal(client.requests.length, 0);
  assert.equal(existsSync(join(kimi.dirs.cwd, 'z.txt')), false);
});

test('a declined Bash does not run; TodoList is the turn plan', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'decline' });
  const { client, threadId } = await adapter(t, kimi);
  await client.request('thread/start', {});
  const turnId = await send(client, threadId, 'CALL Bash {"command":"echo nope > declined.txt"}');
  const ask = await poll(() => client.requests.find(r => r.method === 'item/commandExecution/requestApproval'), Boolean, 'approval');
  client.respond(ask.id, { decision: 'decline' });
  await turnDone(client, turnId);
  assert.equal(existsSync(join(kimi.dirs.cwd, 'declined.txt')), false);
  const todo = await send(client, threadId, 'TODO');
  await turnDone(client, todo);
  const plan = client.notifications.find(n => n.method === 'turn/plan/updated' && n.params.turnId === todo);
  assert.deepEqual(plan.params.plan, [{ step: 'first step', status: 'completed' }, { step: 'second step', status: 'inProgress' }]);
  const read = await client.request('thread/read', { threadId, includeTurns: true });
  assert.equal(read.thread.turns.find(turn => turn.id === todo).items.some(item => item.type === 'dynamicToolCall'), false, 'TodoList is no card');
});

test('interrupt ends the turn; a failed provider call fails the turn with its reason', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'interrupt' });
  const { client, threadId } = await adapter(t, kimi);
  await client.request('thread/start', {});
  const turnId = await send(client, threadId, 'SLOW 150');
  await poll(() => client.notifications.find(n => n.method === 'item/agentMessage/delta' && n.params.turnId === turnId), Boolean, 'streaming started');
  await assert.rejects(client.request('turn/start', { threadId, input: [{ type: 'text', text: 'second' }] }), /busy/);
  await client.request('turn/interrupt', { threadId, turnId });
  const done = await turnDone(client, turnId);
  assert.equal(done.params.turn.status, 'interrupted');
  const failed = await send(client, threadId, 'FAIL now');
  const end = await turnDone(client, failed);
  assert.equal(end.params.turn.status, 'failed');
  assert.ok(end.params.turn.error?.message, JSON.stringify(end.params.turn));
  // The session still works.
  const after = await send(client, threadId, 'after the provider error');
  const afterEnd = await turnDone(client, after);
  assert.equal(afterEnd.params.turn.status, 'completed', JSON.stringify(afterEnd.params.turn) + JSON.stringify(kimi.mock.requests.at(-1)?.body?.messages?.slice(-4)));
});

test('model list and a live model/effort change; restart keeps the selection and the history', { skip: !install, timeout: 180000 }, async t => {
  const models = [{ alias: 'mock/k', model: 'mock-kimi', capabilities: ['tool_use'] },
    { alias: 'mock/think', model: 'mock-think', capabilities: ['tool_use', 'thinking', 'image_in'], efforts: ['low', 'high'], defaultEffort: 'high' }];
  const kimi = await privateKimi(t, { label: 'models', models });
  const first = await adapter(t, kimi, { imageModels: ['mock/think'] });
  await first.client.request('thread/start', {});
  const list = await first.client.request('model/list', {});
  assert.deepEqual(list.data.map(model => model.id), ['mock/k', 'mock/think']);
  assert.deepEqual(list.data.map(model => model.inputModalities.includes('image')), [false, true]);
  await assert.rejects(first.client.request('turn/start', { threadId: first.threadId, input: [{ type: 'text', text: 'IMAGE?' }, { type: 'image', url: 'data:image/png;base64,iVBORw0KGgo=' }] }), /does not accept images/);
  const changed = await first.client.request('thread/settings/update', { threadId: first.threadId, model: 'mock/think', effort: 'low' });
  assert.deepEqual([changed.threadSettings.model, changed.threadSettings.effort], ['mock/think', 'low']);
  const turnId = await send(first.client, first.threadId, 'MODEL?');
  await turnDone(first.client, turnId);
  const text = first.client.notifications.filter(n => n.method === 'item/agentMessage/delta' && n.params.turnId === turnId).map(n => n.params.delta).join('');
  assert.match(text, /model=mock-think/);
  const image = (await first.client.request('turn/start', { threadId: first.threadId, input: [{ type: 'text', text: 'IMAGE?' }, { type: 'image', url: `data:image/png;base64,${PNG}` }] })).turn.id;
  await turnDone(first.client, image);
  const imageText = first.client.notifications.filter(n => n.method === 'item/agentMessage/delta' && n.params.turnId === image).map(n => n.params.delta).join('');
  assert.equal(imageText, 'image parts: 1');
  await first.server.close();
  // A new adapter on the same state: same Kimi session, same selection, same history.
  const second = await adapter(t, kimi, { threadId: first.threadId, nativeSessionId: undefined, imageModels: ['mock/think'] });
  const resumed = await second.client.request('thread/resume', { threadId: first.threadId });
  assert.equal(resumed.model, 'mock/think');
  assert.equal(resumed.reasoningEffort, 'low');
  assert.ok(resumed.thread.turns.some(turn => turn.id === turnId && turn.status === 'completed'));
  const again = await send(second.client, first.threadId, 'CONTEXT?');
  await turnDone(second.client, again);
  const context = second.client.notifications.filter(n => n.method === 'item/agentMessage/delta' && n.params.turnId === again).map(n => n.params.delta).join('');
  assert.match(context, /user turns: 3 \[MODEL\? \| IMAGE\?/, 'Kimi resumed its own context');
});

// 1x1 transparent PNG.
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=';

test('cutex MCP tools run without an ask; approval policy never refuses without asking; reasoning streams', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'mcp', models: [{ alias: 'mock/k', model: 'mock-kimi', capabilities: ['tool_use', 'thinking'] }] });
  const mcp = { name: 'cutex', command: process.execPath, args: [fileURLToPath(new URL('./fake-cutex-mcp.mjs', import.meta.url))], env: [] };
  const { client, threadId } = await adapter(t, kimi, { mcpServers: [mcp], approvalPolicy: 'never' });
  await client.request('thread/start', {});
  const turnId = await send(client, threadId, 'CALL cutex:agent_list {}');
  const done = await turnDone(client, turnId);
  assert.equal(done.params.turn.status, 'completed');
  assert.equal(client.requests.length, 0, 'no ask');
  const read = await client.request('thread/read', { threadId, includeTurns: true });
  const items = read.thread.turns.find(turn => turn.id === turnId).items;
  const card = items.find(item => item.type === 'mcpToolCall');
  assert.deepEqual([card.server, card.tool, card.status], ['cutex', 'agent_list', 'completed']);
  assert.match(JSON.stringify(card.result), new RegExp(`thread=${threadId}`));
  // never: Bash is refused at once, nothing runs, nobody is asked.
  const refused = await send(client, threadId, 'CALL Bash {"command":"echo x > never.txt"}');
  await turnDone(client, refused);
  assert.equal(client.requests.length, 0);
  assert.equal(existsSync(join(kimi.dirs.cwd, 'never.txt')), false);
  const think = await send(client, threadId, 'THINK');
  await turnDone(client, think);
  const reasoning = client.notifications.filter(n => n.method === 'item/reasoning/textDelta' && n.params.turnId === think).map(n => n.params.delta).join('');
  assert.equal(reasoning, 'thinking about it step by step');
  const thinkItems = (await client.request('thread/read', { threadId, includeTurns: true })).thread.turns.find(turn => turn.id === think).items;
  assert.deepEqual(thinkItems.map(item => item.type), ['userMessage', 'reasoning', 'agentMessage']);
});

test('deliveries: idle starts a turn, busy waits for the next one, passive rides along; exclusive inputs', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'delivery' });
  const { client, threadId } = await adapter(t, kimi);
  await client.request('thread/start', {});
  const message = (id, content, mode = 'after_turn') => ({ threadId, messageId: id, author: 'cutex.a', recipient: 'cutex.b', deliveryMode: mode, content,
    authorMetadata: { cutexSessionId: 'cutex.a', displayName: 'agent-a' } });
  await client.request('thread/inter_agent_message', message('m1', 'hello idle'));
  const first = await poll(() => client.notifications.find(n => n.method === 'turn/completed'), Boolean, 'delivery turn', 60000);
  const read = async () => (await client.request('thread/read', { threadId, includeTurns: true })).thread.turns;
  let turns = await read();
  const external = turns[0].items.find(item => item.type === 'functionCallOutput');
  assert.equal(external.cutexMessageId, 'm1');
  assert.match(turns[0].items.find(item => item.type === 'agentMessage').text, /received agent_message: hello idle/);
  // Busy: a slow owner turn; a delivery and a passive note wait for the next turn.
  const slow = await send(client, threadId, 'SLOW 60');
  await client.request('thread/inter_agent_message', message('m2', 'while busy'));
  await client.request('thread/inter_agent_message', message('m3', 'passive note', 'passive'));
  await turnDone(client, slow);
  await poll(read, list => list.length === 3 && list[2].status === 'completed', 'next turn carries both');
  turns = await read();
  assert.deepEqual(turns[2].items.filter(item => item.type === 'functionCallOutput').map(item => item.cutexMessageId), ['m2', 'm3']);
  const status = await client.request('thread/inter_agent_message/status', { threadId, messages: [{ messageId: 'm2', semanticSha256: 'a'.repeat(64) }] });
  assert.equal(status.statuses[0].state, 'conflict');
  // Exclusive input (delegation): its own turn, or busy.
  const input = await client.request('dsh/input/submit', { threadId, messageId: 'd1', eventType: 'delegation_task', admission: 'exclusive_turn', source: { kind: 'agent', id: 'agent-a' }, text: 'do the task' });
  assert.equal(input.state, 'processing');
  await assert.rejects(client.request('dsh/input/submit', { threadId, messageId: 'd2', eventType: 'delegation_task', admission: 'exclusive_turn', source: { kind: 'agent', id: 'agent-a' }, text: 'other' }), /busy/);
  await poll(() => client.request('dsh/input/status', { threadId, messageId: 'd1' }), value => value.state === 'completed' && value.contextPersisted, 'input completed');
  assert.equal(first.params.turn.status, 'completed');
});

test('offline history after stop: the read-only reader serves the same turns, never writes', { skip: !install, timeout: 120000 }, async t => {
  const kimi = await privateKimi(t, { label: 'offline' });
  const { server, client, threadId, config } = await adapter(t, kimi);
  await client.request('thread/start', {});
  const one = await send(client, threadId, 'first');
  await turnDone(client, one);
  const two = await send(client, threadId, 'CALL Read {"path":"missing.txt"}');
  await turnDone(client, two);
  const live = (await client.request('thread/read', { threadId, includeTurns: true })).thread.turns;
  await server.close();
  const { HistoryReader } = await import('../src/history-reader.mjs');
  const reader = new HistoryReader({ database: config.database, threadId, cwd: kimi.dirs.cwd });
  try {
    const offline = reader.handle('thread/read', { threadId, includeTurns: true }).thread;
    assert.deepEqual(offline.turns, live);
    assert.equal(offline.modelProvider, 'kimi');
    assert.equal(offline.model, 'mock/k');
    const page = reader.handle('thread/turns/list', { threadId, limit: 1, sortDirection: 'desc', itemsView: 'full' });
    assert.equal(page.data[0].id, two);
    assert.ok(page.nextCursor);
    const failedRead = offline.turns[1].items.find(item => item.type === 'commandExecution');
    assert.equal(failedRead.status, 'failed');
  } finally { reader.close(); }
});
