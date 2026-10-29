import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, existsSync, chmodSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { duplexPair } from 'node:stream';
import { connect as connectNet } from 'node:net';
import { setTimeout as delay } from 'node:timers/promises';
import { startServer as launchServer, configFromEnv, ndjsonTransport } from '../src/server.mjs';
import { DshRuntime } from '../src/runtime.mjs';

const require = createRequire(new URL('../../../.dsh-install/package.json', import.meta.url));
const { WebSocket } = require('ws');
const token = 'test-only-bearer-token-0123456789';
const fixture = fileURLToPath(new URL('./fake-acp.mjs', import.meta.url));
const serverFile = fileURLToPath(new URL('../src/server.mjs', import.meta.url));
// Echo the actual ACP permission outcome so the WS test proves the decision
// reached the child rather than merely observing that a prompt completed.
const approvalFixture = `
import { createInterface } from 'node:readline';
const sessionId = 'native-approval-session';
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\\n');
let prompt;
for await (const line of createInterface({ input: process.stdin })) {
 const m = JSON.parse(line);
 if (m.method === 'initialize') send({ id:m.id, result:{ protocolVersion:1, agentCapabilities:{sessionCapabilities:{resume:{}}} } });
 else if (m.method === 'session/new' || m.method === 'session/resume') send({id:m.id,result:{sessionId}});
 else if (m.method === 'session/close') send({id:m.id,result:{}});
 else if (m.method === 'session/prompt') {
  prompt = m.id;
  send({method:'session/update', params:{sessionId,update:{sessionUpdate:'tool_call',toolCallId:'tool-1',title:'fixture',rawInput:{},status:'in_progress'}}});
  send({id:'permission',method:'session/request_permission',params:{sessionId,toolCall:{toolCallId:'tool-1'},options:[{kind:'allow_once',optionId:'allow'},{kind:'reject_once',optionId:'reject'}]}});
 } else if (!m.method && m.id === 'permission') {
  send({method:'session/update',params:{sessionId,update:{sessionUpdate:'agent_message_chunk',messageId:'outcome',content:{type:'text',text:JSON.stringify(m.result)}}}});
  send({id:prompt,result:{stopReason:'end_turn'}});
 }
}`;
const servers = new Map();
async function startServer(cfg) {
  const server = await launchServer(cfg);
  servers.set(server, cfg.cwd);
  return server;
}
function config(t) {
  const cwd = mkdtempSync(join(tmpdir(), 'dsh-server-test-'));
  t.after(async () => {
    for (const [server, directory] of servers) if (directory === cwd) { await server.close(); servers.delete(server); }
    rmSync(cwd, { recursive: true, force: true });
  });
  return { command: process.execPath, args: [fixture], cwd, database: join(cwd, 'history.sqlite'),
    env: { HOME: cwd, USERPROFILE: cwd }, readyFile: join(cwd, 'ready.json'), token };
}
async function client(t, server, interactive = false, subscribe = true) {
  const ws = new WebSocket(server.endpoint, { headers: { Authorization: `Bearer ${token}` } });
  const messages = [];
  const waiters = new Set();
  let nextId = 0;
  ws.on('message', raw => {
    const message = JSON.parse(raw);
    messages.push(message);
    for (const waiter of [...waiters]) if (waiter.predicate(message)) { waiters.delete(waiter); clearTimeout(waiter.timer); waiter.resolve(message); }
  });
  await once(ws, 'open');
  t.after(() => ws.terminate());
  function wait(predicate) {
    const found = messages.find(predicate);
    if (found) return Promise.resolve(found);
    return new Promise((resolve, reject) => {
      const waiter = { predicate, resolve, timer: setTimeout(() => { waiters.delete(waiter); reject(new Error('WS message timed out')); }, 5000) };
      waiters.add(waiter);
    });
  }
  function request(method, params = {}) {
    const id = ++nextId;
    const response = wait(message => message.id === id);
    ws.send(JSON.stringify({ id, method, params }));
    return response.then(message => { if (message.error) throw Object.assign(new Error(message.error.message), message.error); return message.result; });
  }
  await request('initialize', { clientInfo: { name: 'cutex-test' }, capabilities: { dshInteractiveApprovals: interactive } });
  ws.send(JSON.stringify({ method: 'initialized' }));
  if (subscribe) await request('thread/resume', { threadId: server.threadId });
  return { ws, request, wait, messages, answer: (id, decision) => ws.send(JSON.stringify({ id, result: { decision } })) };
}
const start = (c, text) => c.request('turn/start', { threadId: c.threadId, input: [{ type: 'text', text }] });

test('authenticated native identity, projected history, busy interrupt, disconnect and restart replay', async t => {
  const cfg = config(t);
  let server = await startServer(cfg);
  t.after(() => server.close());
  const ready = JSON.parse(readFileSync(cfg.readyFile));
  assert.deepEqual(ready, { pid: server.pid, endpoint: server.endpoint, threadId: server.threadId, runtimeId: server.runtimeId });
  assert.match(ready.endpoint, /^ws:\/\/127\.0\.0\.1:\d+$/);
  assert.equal(ready.threadId, '018e0000-0000-7000-8000-0000a000037f');
  assert.ok(!JSON.stringify(ready).includes(token));
  const c = await client(t, server); c.threadId = server.threadId;
  assert.deepEqual(await c.request('thread/loaded/list'), { data: [server.threadId] });
  await assert.rejects(c.request('thread/resume', { threadId: 'other' }), /unknown native/);
  await start(c, `hello ${token}`);
  await c.wait(m => m.method === 'turn/completed');
  const history = await c.request('thread/read', { threadId: server.threadId, includeTurns: true });
  const items = history.thread.turns[0].items;
  assert.ok(items.some(i => i.type === 'userMessage'));
  assert.equal(items.find(i => i.type === 'agentMessage').text, 'hello world');
  assert.ok(items.some(i => i.type === 'dynamicToolCall' && i.tool === 'example' && i.status === 'completed'));
  assert.deepEqual({ used: history.thread.dshContext.used, size: history.thread.dshContext.size }, { used: 42, size: 2048 });
  assert.equal(history.thread.dshContext.source, 'acp');
  assert.equal(history.thread.dshContext.billing, 'unavailable');
  assert.equal(history.thread.tokenUsage, undefined);
  assert.ok(!JSON.stringify(history).includes(token));
  assert.ok(!c.messages.some(m => m.method === 'dsh/sessionUpdate'));
  assert.ok(!JSON.stringify(c.messages).includes(token));
  const turn = (await start(c, 'wait')).turn;
  await c.wait(m => m.method === 'item/reasoning/textDelta');
  assert.deepEqual(await c.request('thread/busy', { threadId: server.threadId }), { busy: true, turnId: turn.id });
  await assert.rejects(start(c, 'steer'), /busy/);
  c.ws.close(); await once(c.ws, 'close');
  const reconnected = await client(t, server);
  assert.equal((await reconnected.request('dsh/runtime/status', { threadId: server.threadId })).activeTurnId, turn.id);
  await reconnected.request('turn/interrupt', { threadId: server.threadId, turnId: turn.id });
  await reconnected.wait(m => m.method === 'turn/completed' && m.params.turn.id === turn.id);
  const before = await reconnected.request('thread/read', { threadId: server.threadId, includeTurns: true });
  await server.close();
  assert.equal(existsSync(cfg.readyFile), false);
  server = await startServer(cfg);
  const restarted = await client(t, server);
  assert.deepEqual(await restarted.request('thread/read', { threadId: server.threadId, includeTurns: true }), before);
  assert.notEqual(server.runtimeId, ready.runtimeId);
  // r265: the live adapter pages history through the timeline index, like the offline reader.
  const page = await restarted.request('thread/turns/list', { threadId: server.threadId, sortDirection: 'asc', itemsView: 'full', limit: 1 });
  assert.deepEqual(page.data.map(turn => turn.id), [before.thread.turns[0].id]);
  assert.ok(page.nextCursor);
  const rest = await restarted.request('thread/turns/list', { threadId: server.threadId, sortDirection: 'asc', itemsView: 'full', limit: 1, cursor: page.nextCursor });
  assert.deepEqual(rest.data.map(turn => turn.status), ['interrupted']);
  const pagedItems = await restarted.request('thread/items/list', { threadId: server.threadId, turnId: before.thread.turns[0].id, sortDirection: 'asc' });
  assert.ok(pagedItems.data.some(entry => entry.item.type === 'agentMessage'));
  const resumed = await restarted.request('thread/resume', { threadId: server.threadId });
  assert.deepEqual(resumed.thread.turns.map(turn => turn.id), before.thread.turns.map(turn => turn.id));
  assert.equal(resumed.thread.turnsTruncated, undefined);
  assert.equal((await restarted.request('dsh/runtime/status', { threadId: server.threadId })).capabilities.revert, true);
});

for (const order of ['native-first', 'acp-first']) {
  test(`native terminal notifications wait for both barriers (${order}); immediate queued turn is accepted`, async t => {
    const cfg = config(t);
    let runtime;
    const initialize = DshRuntime.prototype.initialize;
    t.mock.method(DshRuntime.prototype, 'initialize', async function (...args) {
      const result = await initialize.apply(this, args);
      runtime = this;
      // Only the scan/ACP scheduling is controlled. Projection and persistence
      // use the real production TimelineStore and post-commit callback.
      this.sessionsRoot = cfg.cwd;
      this.syncNative = async () => {};
      return result;
    });
    const server = await startServer(cfg);
    const c = await client(t, server); c.threadId = server.threadId;
    let settleACP;
    const acp = new Promise(done => { settleACP = done; });
    const request = runtime.rpc.request.bind(runtime.rpc);
    runtime.rpc.request = (method, ...args) => method === 'session/prompt' ? acp : request(method, ...args);
    const first = (await start(c, 'first')).turn;
    function commit(type, data) {
      const previousCursor = runtime.store.nativeCursor(runtime.sessionId);
      const nativeSeq = (previousCursor?.nativeSeq ?? -1) + 1;
      runtime.store.commitNative({ sessionId: runtime.sessionId, nativeSeq, nativeId: JSON.stringify([runtime.sessionId, nativeSeq]),
        event: { seq: nativeSeq, type, time: nativeSeq, data }, updates: [], previousCursor,
        cursor: { version: 1, sessionId: runtime.sessionId, nativeSeq, hash: nativeSeq.toString(16).padStart(64, '0') } });
    }
    commit('turn/start', { turn: 1 });
    let releaseScan;
    if (order === 'acp-first') {
      const scanning = new Promise(done => { releaseScan = done; });
      runtime.syncNative = () => scanning;
      settleACP({ stopReason: 'end_turn' });
      // Drain ACP continuation deterministically before committing its log.
      await new Promise(done => setImmediate(done));
    }
    commit('turn/end', { turn: 1, reason: { kind: 'completed' } });
    assert.equal(runtime.replay().filter(e => e.method === 'turn/completed').length, 1, 'native authority already committed');
    const snapshot = await c.request('thread/read', { threadId: server.threadId, includeTurns: true });
    assert.equal(snapshot.thread.turns[0].status, 'inProgress');
    assert.equal(snapshot.thread.status.type, 'active');
    assert.equal((await c.request('thread/busy', { threadId: server.threadId })).busy, true);
    assert.ok(!c.messages.some(m => m.method === 'turn/completed'));
    assert.ok(!c.messages.some(m => m.method === 'thread/status/changed' && m.params.status.type === 'idle'));
    let queued;
    c.ws.on('message', raw => {
      const m = JSON.parse(raw);
      if (m.method === 'turn/completed' && m.params.turn.id === first.id) queued = start(c, 'queued immediately');
    });
    // The second prompt completes from its own native facts below.
    if (order === 'native-first') settleACP({ stopReason: 'end_turn' });
    else releaseScan();
    await c.wait(m => m.method === 'turn/completed' && m.params.turn.id === first.id);
    const second = (await queued).turn;
    assert.notEqual(second.id, first.id);
    commit('turn/start', { turn: 2 });
    commit('turn/end', { turn: 2, reason: { kind: 'completed' } });
    await c.wait(m => m.method === 'turn/completed' && m.params.turn.id === second.id);
    assert.equal(c.messages.filter(m => m.method === 'turn/completed' && m.params.turn.id === first.id).length, 1);
    assert.equal(runtime.replay().filter(e => e.method === 'turn/completed').length, 2);
  });
}

test('approval fans out only to interactive subscribers; observer cannot answer, first answer wins', async t => {
  const server = await startServer({ ...config(t), args: ['--input-type=module', '-e', approvalFixture] }); t.after(() => server.close());
  const owner = await client(t, server, true); owner.threadId = server.threadId;
  const second = await client(t, server, true);
  const observer = await client(t, server);
  const unsubscribed = await client(t, server, true, false);
  await start(owner, 'approval');
  const approval = await owner.wait(m => m.method === 'item/commandExecution/requestApproval');
  const forwarded = await second.wait(m => m.method === 'item/commandExecution/requestApproval');
  assert.equal(forwarded.id, approval.id);
  assert.deepEqual(approval.params.availableDecisions, ['accept', 'decline', 'cancel']);
  observer.answer(approval.id, 'accept');
  assert.equal((await owner.request('dsh/runtime/status', { threadId: server.threadId })).pendingApprovals, 1);
  assert.ok(!observer.messages.some(m => m.method === approval.method));
  assert.ok(!unsubscribed.messages.some(m => m.method === approval.method));
  second.answer(approval.id, 'accept'); owner.answer(approval.id, 'decline');
  await owner.wait(m => m.method === 'turn/completed');
  await owner.wait(m => m.method === 'serverRequest/resolved' && m.params.requestId === approval.id);
  assert.equal((await owner.request('dsh/runtime/status', { threadId: server.threadId })).pendingApprovals, 0);
  const history = await owner.request('thread/read', { threadId: server.threadId, includeTurns: true });
  assert.deepEqual(JSON.parse(history.thread.turns[0].items.find(i => i.type === 'agentMessage').text), { outcome: { outcome: 'selected', optionId: 'allow' } });
});

test('unsupported persistent approval decision cancels the native permission', async t => {
  const server = await startServer({ ...config(t), args: ['--input-type=module', '-e', approvalFixture] }); t.after(() => server.close());
  const owner = await client(t, server, true); owner.threadId = server.threadId;
  await start(owner, 'approval');
  const approval = await owner.wait(m => m.method === 'item/commandExecution/requestApproval');
  owner.answer(approval.id, 'acceptForSession');
  await owner.wait(m => m.method === 'turn/completed');
  const history = await owner.request('thread/read', { threadId: server.threadId, includeTurns: true });
  assert.deepEqual(JSON.parse(history.thread.turns[0].items.find(i => i.type === 'agentMessage').text), { outcome: { outcome: 'cancelled' } });
});

test('pending approval transfers to reconnecting controller; cancel aborts unresolved approval', async t => {
  const server = await startServer(config(t)); t.after(() => server.close());
  const first = await client(t, server, true); first.threadId = server.threadId;
  const turn = (await start(first, 'approval')).turn;
  const approval = await first.wait(m => m.method === 'item/commandExecution/requestApproval');
  const next = await client(t, server, true);
  assert.equal((await next.wait(m => m.method === approval.method)).id, approval.id);
  first.ws.close(); await once(first.ws, 'close');
  assert.equal((await next.request('dsh/runtime/status', { threadId: server.threadId })).pendingApprovals, 1);
  await next.request('turn/interrupt', { threadId: server.threadId, turnId: turn.id });
  await next.wait(m => m.method === 'serverRequest/resolved');
  await next.wait(m => m.method === 'turn/completed');
});

test('an approval waits without a clock: no owner yet, owner disconnect, reconnect answers; interrupt ends it', async t => {
  const server = await startServer({ ...config(t), args: ['--input-type=module', '-e', approvalFixture] }); t.after(() => server.close());
  const observer = await client(t, server); observer.threadId = server.threadId;
  await start(observer, 'approval');
  await new Promise(resolve => setTimeout(resolve, 150));
  assert.equal((await observer.request('dsh/runtime/status', { threadId: server.threadId })).pendingApprovals, 1, 'no owner client: still pending');
  assert.ok(!observer.messages.some(m => m.method === 'turn/completed'));
  const owner = await client(t, server, true); owner.threadId = server.threadId;
  const approval = await owner.wait(m => m.method === 'item/commandExecution/requestApproval');
  owner.ws.close(); await once(owner.ws, 'close');
  await new Promise(resolve => setTimeout(resolve, 100));
  assert.equal((await observer.request('dsh/runtime/status', { threadId: server.threadId })).pendingApprovals, 1, 'owner disconnect keeps it pending');
  const again = await client(t, server, true); again.threadId = server.threadId;
  assert.equal((await again.wait(m => m.method === approval.method)).id, approval.id);
  again.answer(approval.id, 'accept');
  const outcome = await observer.wait(m => m.method === 'item/completed' && m.params.item.type === 'agentMessage');
  assert.match(outcome.params.item.text, /"optionId":"allow"/);
  // A second ask nobody answers ends with its turn.
  const turn = (await start(again, 'approval')).turn;
  await again.wait(m => m.method === 'item/commandExecution/requestApproval' && m.id !== approval.id);
  await again.request('turn/interrupt', { threadId: server.threadId, turnId: turn.id });
  await observer.wait(m => m.method === 'turn/completed' && m.params.turn.id === turn.id);
});

test('strict bearer handshake rejects missing, wrong, query token and browser origin', async t => {
  const server = await startServer(config(t)); t.after(() => server.close());
  for (const [suffix, headers, status] of [ ['', {}, 401], ['', { Authorization: 'Bearer wrong' }, 401],
    [`?token=${token}`, {}, 401], ['', { Authorization: `Bearer ${token}`, Origin: 'http://localhost' }, 403] ]) {
    const ws = new WebSocket(server.endpoint + suffix, { headers });
    const code = await new Promise((resolve, reject) => {
      ws.on('unexpected-response', (_req, res) => { res.resume(); resolve(res.statusCode); ws.terminate(); });
      ws.on('open', () => reject(new Error('unauthorized socket opened')));
      ws.on('error', () => {});
    });
    assert.equal(code, status);
  }
});

test('CLI explicit config publishes ready IPC and authenticated shutdown exits', async t => {
  const cfg = config(t);
  const file = join(cfg.cwd, 'config.json');
  const { token: _, ...disk } = cfg;
  writeFileSync(file, JSON.stringify(disk));
  assert.throws(() => configFromEnv({}), /DSH_CUTEX_CONFIG/);
  assert.equal(configFromEnv({ DSH_CUTEX_CONFIG: file, DSH_CUTEX_TOKEN: token }).token, token);
  writeFileSync(file, JSON.stringify({ ...disk, token }));
  assert.equal(configFromEnv({ DSH_CUTEX_CONFIG: file }).token, token);
  assert.equal(configFromEnv({ DSH_CUTEX_CONFIG: file, DSH_CUTEX_TOKEN: 'explicit-override' }).token, 'explicit-override');
  const child = spawn(process.execPath, [serverFile], { env: { HOME: cfg.cwd, USERPROFILE: cfg.cwd, DSH_CUTEX_CONFIG: file, DSH_CUTEX_TOKEN: token }, windowsHide: true, stdio: 'pipe' });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  const exited = once(child, 'exit');
  for (let count = 0; !existsSync(cfg.readyFile) && count < 200; count++) await delay(25);
  assert.ok(existsSync(cfg.readyFile), 'CLI ready file missing');
  const server = JSON.parse(readFileSync(cfg.readyFile));
  const c = await client(t, server);
  assert.deepEqual(await c.request('dsh/server/shutdown'), {});
  const [code] = await exited;
  assert.equal(code, 0);
  assert.equal(existsSync(cfg.readyFile), false);
});

test('explicit secret name mapping is memory-only, validates sources, and never inherits unrelated env', t => {
  const cfg = config(t), file = join(cfg.cwd, 'mapped-config.json');
  const disk = { ...cfg, secretEnvMappings: [{ source: 'PROFILE_KEY', target: 'DEEPSEEK_API_KEY' }] };
  writeFileSync(file, JSON.stringify(disk));
  const secret = 'fake-profile-secret-for-mapping';
  const mapped = configFromEnv({ DSH_CUTEX_CONFIG: file, PROFILE_KEY: secret, UNRELATED_SECRET: 'never-inherit' });
  assert.equal(mapped.env.DEEPSEEK_API_KEY, secret);
  assert.equal(mapped.env.PROFILE_KEY, undefined);
  assert.equal(mapped.env.UNRELATED_SECRET, undefined);
  assert.ok(!readFileSync(file, 'utf8').includes(secret));
  for (const value of [undefined, '', '  ']) assert.throws(() => configFromEnv({ DSH_CUTEX_CONFIG: file, PROFILE_KEY: value }), /missing or blank/);
  // Earlier keys of the profile (cutex: DSH_CUTEX_REDACT) are redacted by the adapter, never handed to DSH.
  const retired = configFromEnv({ DSH_CUTEX_CONFIG: file, PROFILE_KEY: secret, DSH_CUTEX_REDACT: JSON.stringify(['fake-retired-key-1', '', 7]) });
  assert.deepEqual(retired.redact, ['fake-retired-key-1']);
  assert.ok(!Object.values(retired.env).includes('fake-retired-key-1'));
  assert.equal(configFromEnv({ DSH_CUTEX_CONFIG: file, PROFILE_KEY: secret, DSH_CUTEX_REDACT: 'not json' }).redact, undefined);
  for (const mapping of [{ source: 'PROFILE_KEY', target: 'PATH' }, { source: 'bad-name', target: 'DEEPSEEK_API_KEY' }, { source: 'PROFILE_KEY', target: 'DEEPSEEK_API_KEY', value: secret }]) {
    writeFileSync(file, JSON.stringify({ ...disk, secretEnvMappings: [mapping] }));
    assert.throws(() => configFromEnv({ DSH_CUTEX_CONFIG: file, PROFILE_KEY: secret }), /invalid secret/);
  }
});

test('live launch selection cannot be changed by resume or turn overrides', async t => {
  const cfg = config(t);
  const initialize = DshRuntime.prototype.initialize;
  t.mock.method(DshRuntime.prototype, 'initialize', async function (...args) {
    const result = await initialize.apply(this, args);
    this.selection = { model: 'deepseek-v4-flash', reasoningEffort: 'high' };
    return result;
  });
  const server = await startServer(cfg);
  const c = await client(t, server);
  const response = await c.request('thread/resume', { threadId: server.threadId, model: 'deepseek-v4-flash', reasoningEffort: 'high' });
  assert.equal(response.model, 'deepseek-v4-flash');
  assert.equal(response.reasoningEffort, 'high');
  await assert.rejects(c.request('thread/resume', { threadId: server.threadId, model: 'deepseek-v4-pro' }), /thread\/settings\/update/);
  await assert.rejects(c.request('turn/start', { threadId: server.threadId, effort: 'low', input: [{ type: 'text', text: 'must not run' }] }), /thread\/settings\/update/);
});

test('facade identity fences clients and MCP while native replay keeps its own session identity', async t => {
  const cfg = { ...config(t), threadId: 'facade-conversation-uuid', mcpServers: [{ name: 'cutex', command: process.execPath, args: [], env: [] }] };
  let runtime, mcp;
  const initialize = DshRuntime.prototype.initialize;
  t.mock.method(DshRuntime.prototype, 'initialize', async function (factory) {
    runtime = this;
    return initialize.call(this, nativeId => { mcp = factory(nativeId); return mcp; });
  });
  let server = await startServer(cfg);
  const native = runtime.sessionId;
  assert.notEqual(native, cfg.threadId);
  assert.equal(server.threadId, cfg.threadId);
  assert.equal(server.nativeSessionId, native);
  assert.equal(mcp[0].env.find(entry => entry.name === 'CUTEX_MCP_THREAD_ID').value, cfg.threadId);
  const c = await client(t, server, true); c.threadId = server.threadId;
  assert.deepEqual(await c.request('thread/loaded/list'), { data: [cfg.threadId] });
  await assert.rejects(c.request('thread/read', { threadId: native }), /unknown native/);
  await start(c, 'approval');
  const approval = await c.wait(m => m.method === 'item/commandExecution/requestApproval');
  assert.equal(approval.params.threadId, cfg.threadId);
  c.answer(approval.id, 'accept');
  await c.wait(m => m.method === 'turn/completed');
  assert.ok(c.messages.filter(m => m.params?.threadId).every(m => m.params.threadId === cfg.threadId));
  const history = await c.request('thread/read', { threadId: cfg.threadId, includeTurns: true });
  assert.equal(history.thread.id, cfg.threadId);
  assert.ok(runtime.replay().filter(e => e.params.threadId).every(e => e.params.threadId === native));
  await server.close();
  server = await startServer(cfg);
  assert.equal(server.nativeSessionId, native);
  assert.equal(server.threadId, cfg.threadId);
});

test('readOnly WS server opens history without invoking DshRuntime or accepting prompts', async t => {
  const cfg = config(t);
  const writer = await startServer(cfg);
  const c = await client(t, writer); c.threadId = writer.threadId;
  await start(c, 'history'); await c.wait(m => m.method === 'turn/completed');
  await writer.close();
  t.mock.method(DshRuntime.prototype, 'initialize', () => assert.fail('readOnly must not spawn DSH'));
  const server = await startServer({ ...cfg, readOnly: true, threadId: 'reader-facade', nativeSessionId: writer.threadId, command: 'must-not-execute' });
  const reader = await client(t, server, false, false);
  const result = await reader.request('thread/read', { threadId: server.threadId, includeTurns: true });
  assert.equal(result.thread.id, 'reader-facade');
  assert.equal(result.thread.turns[0].items.find(i => i.type === 'agentMessage').text, 'hello world');
  await assert.rejects(reader.request('turn/start', { threadId: server.threadId, input: [{ type: 'text', text: 'never execute' }] }), /read-only/);
});

test('product CLI maps key only into the child allowlist and redacts echoed output before persistence', async t => {
  const cfg = config(t), file = join(cfg.cwd, 'product-config.json');
  const secret = 'keyless-test-product-secret-123456789';
  const childFixture = `
import { createInterface } from 'node:readline';
const sessionId = 'offline-product-native';
const send = value => process.stdout.write(JSON.stringify({jsonrpc:'2.0',...value})+'\\n');
const configOptions = [{id:'model',category:'model',type:'select',currentValue:'deepseek-flash',options:[{name:'DeepSeek-V4.1-Flash',value:'deepseek-flash'}]}];
for await (const line of createInterface({input:process.stdin})) {
 const m=JSON.parse(line);
 if(m.method==='initialize') send({id:m.id,result:{protocolVersion:1,agentCapabilities:{sessionCapabilities:{resume:{}}}}});
 else if(m.method==='session/new'||m.method==='session/resume') send({id:m.id,result:{sessionId,configOptions}});
 else if(m.method==='session/set_config_option') send({id:m.id,result:{configOptions}});
 else if(m.method==='session/close') send({id:m.id,result:{}});
 else if(m.method==='session/prompt') {
  send({method:'session/update',params:{sessionId,update:{sessionUpdate:'agent_message_chunk',messageId:'echo',content:{type:'text',text:JSON.stringify({hasKey:Boolean(process.env.DEEPSEEK_API_KEY),source:process.env.PROFILE_KEY??null,unrelated:process.env.UNRELATED_SECRET??null,echo:process.env.DEEPSEEK_API_KEY})}}}});
  send({id:m.id,result:{stopReason:'end_turn'}});
 }
}`;
  writeFileSync(file, JSON.stringify({ ...cfg, args: ['--input-type=module', '-e', childFixture], secretEnvMappings: [{ source: 'PROFILE_KEY', target: 'DEEPSEEK_API_KEY' }] }));
  const child = spawn(process.execPath, [serverFile], { env: { HOME: cfg.cwd, USERPROFILE: cfg.cwd, DSH_CUTEX_CONFIG: file, PROFILE_KEY: secret, UNRELATED_SECRET: 'must-not-inherit' }, windowsHide: true, stdio: 'pipe' });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  const exited = once(child, 'exit');
  for (let count = 0; !existsSync(cfg.readyFile) && count < 200; count++) await delay(25);
  assert.ok(existsSync(cfg.readyFile));
  const server = JSON.parse(readFileSync(cfg.readyFile));
  const c = await client(t, server); c.threadId = server.threadId;
  await start(c, 'echo fake key'); await c.wait(m => m.method === 'turn/completed');
  const result = await c.request('thread/read', { threadId: server.threadId, includeTurns: true });
  const echo = JSON.parse(result.thread.turns[0].items.find(i => i.type === 'agentMessage').text);
  assert.deepEqual(echo, { hasKey: true, source: null, unrelated: null, echo: '[REDACTED]' });
  assert.equal(result.thread.model, 'deepseek-flash');
  assert.ok(!JSON.stringify(c.messages).includes(secret));
  assert.ok(!readFileSync(file, 'utf8').includes(secret));
  assert.ok(!readFileSync(cfg.readyFile, 'utf8').includes(secret));
  await c.request('dsh/server/shutdown');
  assert.equal((await exited)[0], 0);
  assert.ok(!readFileSync(cfg.database).includes(Buffer.from(secret)));
});

test('NDJSON transport preserves split UTF-8, multiplexes lines and bounds unterminated frames', async t => {
  const [socket, client] = duplexPair();
  t.after(() => { socket.destroy(); client.destroy(); });
  const transport = ndjsonTransport(socket);
  transport.on('error', () => {});
  const frames = [];
  let complete;
  const received = new Promise(done => { complete = done; });
  transport.on('message', raw => { frames.push(JSON.parse(raw.toString())); if (frames.length === 2) complete(); });
  const bytes = Buffer.from(JSON.stringify({ id: 1, text: '中文' }) + '\n' + JSON.stringify({ id: 2 }) + '\n');
  const split = bytes.indexOf(Buffer.from('中文')) + 1;
  client.write(bytes.subarray(0, split));
  client.write(bytes.subarray(split));
  await received;
  assert.deepEqual(frames, [{ id: 1, text: '中文' }, { id: 2 }]);
  const response = once(client, 'data');
  transport.send(JSON.stringify({ id: 1, result: '中文' }));
  assert.equal((await response)[0].toString(), '{"id":1,"result":"中文"}\n');
  const closed = once(socket, 'close');
  client.write(Buffer.alloc(8 * 1024 * 1024 + 1, 'x'));
  await closed;
  assert.notEqual(transport.readyState, WebSocket.OPEN);
});

test('Unix socket serves the daemon WebSocket client with private mode, approvals, reconnect and no bearer handshake', { skip: process.platform === 'win32' }, async t => {
  const cfg = config(t);
  chmodSync(cfg.cwd, 0o700);
  const socketPath = join(cfg.cwd, 's');
  const server = await startServer({ ...cfg, token: undefined, socketPath, threadId: 'unix-facade' });
  assert.equal(server.endpoint, `unix://${socketPath}`);
  assert.equal(statSync(socketPath).mode & 0o777, 0o600);
  // A browser Origin is refused even on the private socket.
  const browser = new WebSocket(`ws+unix:${socketPath}:/`, { headers: { Origin: 'http://localhost' } });
  browser.on('error', () => {});
  await once(browser, 'unexpected-response');
  async function open() {
    const socket = new WebSocket(`ws+unix:${socketPath}:/`);
    t.after(() => socket.terminate());
    socket.on('error', () => {});
    let next = 0;
    const messages = [];
    const waiters = [];
    socket.on('message', raw => {
      const m = JSON.parse(raw); messages.push(m);
      for (const waiter of [...waiters]) if (waiter.predicate(m)) { waiters.splice(waiters.indexOf(waiter), 1); waiter.done(m); }
    });
    const wait = predicate => messages.some(predicate) ? Promise.resolve(messages.find(predicate)) : new Promise(done => { waiters.push({ predicate, done }); });
    const peer = { send: text => socket.send(text) };
    const request = (method, params = {}) => {
      const id = ++next, answer = wait(m => m.id === id);
      peer.send(JSON.stringify({ id, method, params }));
      return answer.then(m => { if (m.error) throw new Error(m.error.message); return m.result; });
    };
    await once(socket, 'open');
    await request('initialize', { capabilities: { dshInteractiveApprovals: true } });
    peer.send(JSON.stringify({ method: 'initialized' }));
    await request('thread/resume', { threadId: server.threadId });
    return { socket: { destroy: () => socket.terminate() }, peer, request, wait };
  }
  const first = await open();
  await first.request('turn/start', { threadId: server.threadId, input: [{ type: 'text', text: 'approval' }] });
  const approval = await first.wait(m => m.method === 'item/commandExecution/requestApproval');
  assert.equal(approval.params.threadId, server.threadId);
  first.peer.send(JSON.stringify({ id: approval.id, result: { decision: 'accept' } }));
  await first.wait(m => m.method === 'turn/completed');
  first.socket.destroy();
  const next = await open();
  assert.equal((await next.request('thread/read', { threadId: server.threadId, includeTurns: true })).thread.turns.length, 1);
  await next.request('dsh/server/shutdown');
  await server.close();
  assert.equal(existsSync(socketPath), false);
});

test('Unix socket refuses unsafe parents and preexisting paths without unlinking', { skip: process.platform === 'win32' }, async t => {
  const cfg = config(t), socketPath = join(cfg.cwd, 's');
  chmodSync(cfg.cwd, 0o700);
  writeFileSync(socketPath, 'unknown-owner-file');
  await assert.rejects(startServer({ ...cfg, socketPath }), /already exists/);
  assert.equal(readFileSync(socketPath, 'utf8'), 'unknown-owner-file');
  chmodSync(cfg.cwd, 0o755);
  await assert.rejects(startServer({ ...cfg, socketPath: join(cfg.cwd, 'other') }), /0700/);
});

test('image input is refused clearly when the DSH model does not declare it', async t => {
  const server = await startServer(config(t)); t.after(() => server.close());
  const c = await client(t, server); c.threadId = server.threadId;
  await assert.rejects(c.request('turn/start', { threadId: server.threadId, input: [{ type: 'text', text: 'look' }, { type: 'image', url: 'data:image/png;base64,iVBORw0KGgo=' }] }), /does not accept images/);
  await assert.rejects(c.request('turn/start', { threadId: server.threadId, input: [{ type: 'mention', name: 'x' }] }), /only text and image input/);
  const models = await c.request('model/list', { limit: 100 });
  assert.deepEqual(models, { data: [], nextCursor: null }, 'the fixture offers no model option');
});
