// Real pinned DSH (--profile acp) + offline scripted LLM behind the adapter's WS server:
// a shell command, a read, file edits and a checklist reach clients and thread/read as
// commandExecution / fileChange / turn/plan/updated, live and from read-only history.
// Keyless and offline. Run: DSH_SPIKE_INSTALL=<install root> node --test test/toolcards-real.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { once } from 'node:events';
import { startServer } from '../src/server.mjs';

const install = process.env.DSH_SPIKE_INSTALL;
const token = 'test-only-toolcards-token-0123456789';
const threadId = '018e0000-0000-7000-8000-00000000c6c6';

async function client(t, endpoint, { resume = true } = {}) {
  const { WebSocket } = createRequire(join(resolve(install), 'package.json'))('ws');
  const ws = new WebSocket(endpoint, { headers: { Authorization: `Bearer ${token}` } });
  const messages = [], waiters = new Set();
  let nextId = 0;
  ws.on('message', raw => {
    const message = JSON.parse(raw);
    messages.push(message);
    // Any DSH ask is accepted: this test is about how the calls look.
    if (typeof message.id === 'string' && message.method?.endsWith('/requestApproval')) ws.send(JSON.stringify({ id: message.id, result: { decision: 'accept' } }));
    for (const waiter of [...waiters]) if (waiter.predicate(message)) { waiters.delete(waiter); clearTimeout(waiter.timer); waiter.resolve(message); }
  });
  await once(ws, 'open');
  t.after(() => ws.terminate());
  const wait = (predicate, ms = 60000) => {
    const found = messages.find(predicate);
    if (found) return Promise.resolve(found);
    return new Promise((done, fail) => {
      const waiter = { predicate, resolve: done, timer: setTimeout(() => { waiters.delete(waiter); fail(new Error(`WS wait timed out; got ${JSON.stringify(messages.map(m => m.method ?? m.id))}`)); }, ms) };
      waiters.add(waiter);
    });
  };
  const request = (method, params = {}) => {
    const id = ++nextId;
    const response = wait(message => message.id === id);
    ws.send(JSON.stringify({ id, method, params }));
    return response.then(message => { if (message.error) throw Object.assign(new Error(message.error.message), message.error); return message.result; });
  };
  await request('initialize', { clientInfo: { name: 'cutex-toolcards-test' }, capabilities: { dshInteractiveApprovals: true } });
  ws.send(JSON.stringify({ method: 'initialized' }));
  if (resume) await request('thread/resume', { threadId });
  return { request, wait, messages };
}

test('real DSH tool calls become commandExecution / fileChange items live and in thread/read', { skip: !install, timeout: 180000 }, async t => {
  const home = mkdtempSync(join(tmpdir(), 'dsh-toolcards-real-'));
  t.after(() => rmSync(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }));
  writeFileSync(join(home, '.cutex-test-private-home'), 'dsh tool-card acceptance\n');
  const cwd = join(home, 'workspace');
  mkdirSync(cwd);
  writeFileSync(join(cwd, 'notes.txt'), 'alpha\nbeta\ngamma\n');
  const root = resolve(install);
  const plugin = fileURLToPath(new URL('./toolcards-mock-llm.mjs', import.meta.url)).replaceAll('\\', '/');
  const patch = join(home, 'offline.patch.yml');
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config:
    provider: cutex-offline
    model: offline
- id: session-persistence-jsonl
  config:
    root: '${join(home, 'sessions').replaceAll('\\', '/')}'
    compression: none
- id: session-log-deepseek
  config:
    enabled: false
- insert:
    - id: cutex-toolcards-llm
      name: '${plugin}'
`);
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT'].filter(key => process.env[key]).map(key => [key, process.env[key]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'), TEMP: home, TMP: home, TMPDIR: home,
    CUTEX_TEST_PRIVATE_HOME: home, DSH_HOME: join(home, 'dsh'), DSH_TELEMETRY_DISABLED: '1', DSH_SPIKE_INSTALL_ROOT: root });
  const config = { command: process.execPath, args: [join(root, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', patch],
    cwd, env, token, threadId, database: join(home, 'timeline.sqlite'), sessionsRoot: join(home, 'sessions') };
  let server = await startServer(config);
  t.after(async () => { if (server) await server.close(); });
  const c = await client(t, server.endpoint);
  const script = [
    ['read', { file_path: 'notes.txt' }],
    ['edit', { file_path: 'notes.txt', old_string: 'beta', new_string: 'BETA' }],
    ['bash', { command: 'echo out; echo err >&2; exit 4', description: 'Exit with four' }],
    ['todo_write', { todos: [{ content: 'Edit notes', status: 'completed' }, { content: 'Write summary', status: 'in_progress' }] }],
    ['write', { file_path: 'summary.txt', content: 'done\n' }],
  ];
  await c.request('turn/start', { threadId, input: [{ type: 'text', text: `SCRIPT ${JSON.stringify(script)}` }] });
  const finished = await c.wait(message => message.method === 'turn/completed');
  assert.equal(finished.params.turn.status, 'completed');
  assert.equal(readFileSync(join(cwd, 'notes.txt'), 'utf8'), 'alpha\nBETA\ngamma\n');

  // Live: the same notifications a cute-codex runtime sends.
  const started = c.messages.filter(m => m.method === 'item/started' && m.params.item.type !== 'userMessage' && m.params.item.type !== 'agentMessage' && m.params.item.type !== 'reasoning').map(m => m.params.item.type);
  assert.deepEqual(started, ['commandExecution', 'fileChange', 'commandExecution', 'fileChange']);
  const plan = c.messages.find(m => m.method === 'turn/plan/updated');
  assert.deepEqual(plan?.params.plan, [{ step: 'Edit notes', status: 'completed' }, { step: 'Write summary', status: 'inProgress' }]);
  assert.equal(plan.params.turnId, finished.params.turn.id);

  const check = items => {
    const tools = items.filter(item => !['userMessage', 'agentMessage', 'reasoning'].includes(item.type));
    assert.deepEqual(tools.map(item => item.type), ['commandExecution', 'fileChange', 'commandExecution', 'fileChange']);
    const [read, edit, bash, write] = tools;
    assert.equal(read.commandActions[0].type, 'read');
    assert.equal(read.aggregatedOutput, 'alpha\nbeta\ngamma');
    assert.equal(edit.status, 'completed');
    assert.equal(edit.changes[0].path, join(cwd, 'notes.txt'));
    assert.equal(edit.changes[0].diff, '@@\n alpha\n-beta\n+BETA\n gamma\n');
    assert.equal(bash.command, 'echo out; echo err >&2; exit 4');
    assert.equal(bash.exitCode, 4);
    assert.equal(bash.status, 'failed');
    assert.equal(bash.aggregatedOutput, 'out\n[stderr]\nerr\n');
    assert.equal(bash.cwd, cwd);
    assert.ok(Number.isSafeInteger(bash.durationMs));
    assert.deepEqual(write.changes, [{ path: join(cwd, 'summary.txt'), kind: { type: 'add' }, diff: 'done\n' }]);
    assert.ok(!items.some(item => item.type === 'userMessage' && JSON.stringify(item).includes('Current runtime context')), 'no runtime-context bubble');
  };
  const live = await c.request('thread/read', { threadId, includeTurns: true });
  check(live.thread.turns[0].items);
  await server.close();
  server = undefined;

  // Read-only history (what cutex serves for a stopped agent): the same items.
  server = await startServer({ ...config, readOnly: true });
  const reader = await client(t, server.endpoint, { resume: false });
  const history = await reader.request('thread/read', { threadId, includeTurns: true });
  check(history.thread.turns[0].items);
});
