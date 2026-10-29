import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fork } from 'node:child_process';
import { once } from 'node:events';
import { DshRuntime, permissionOutcome, cutexMcp } from '../src/runtime.mjs';

const fixture = fileURLToPath(new URL('./fake-acp.mjs', import.meta.url));
function setup(t, extra = {}) {
  const cwd = mkdtempSync(join(tmpdir(), 'cutex-dsh-unit-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const config = { command: process.execPath, args: [fixture], cwd, database: join(cwd, 'history.sqlite'), env: {}, ...extra };
  return { config, runtime: new DshRuntime(config) };
}

test('committed message chunks and tools survive process restart; replay has cursor', async t => {
  const { config, runtime } = setup(t);
  await runtime.initialize();
  await runtime.prompt('hello');
  const events = runtime.replay();
  assert.equal(events.filter(e => e.method === 'item/completed' && e.params.item.type === 'agentMessage')[0].params.item.text, 'hello world');
  assert.ok(events.some(e => e.method === 'item/completed' && e.params.item.type === 'dynamicToolCall' && e.params.item.namespace === 'dsh'));
  const sessionId = runtime.sessionId;
  await runtime.close();
  const second = new DshRuntime(config);
  try {
    await second.initialize();
    assert.equal(second.sessionId, sessionId);
    assert.deepEqual(second.replay(), events);
    assert.deepEqual(second.replay(events[3].seq), events.slice(4));
    await second.prompt('again');
    assert.equal(second.replay().filter(e => e.method === 'turn/completed').length, 2);
  } finally { await second.close(); }
});

test('concurrent prompt refuses steer and cancel completes the current turn', async t => {
  let thought;
  const seen = new Promise(resolve => { thought = resolve; });
  const { runtime } = setup(t, { emit: e => { if (e.method === 'item/reasoning/textDelta') thought(); } });
  try {
    await runtime.initialize();
    const prompt = runtime.prompt('wait');
    await seen;
    await assert.rejects(runtime.prompt('steer'), /no same-turn steer/);
    runtime.cancel();
    assert.equal((await prompt).stopReason, 'cancelled');
    assert.equal(runtime.replay().at(-1).params.turn.status, 'interrupted');
  } finally { await runtime.close(); }
});

test('permission bridges to command approval; persistent decisions never grant', async t => {
  let request;
  const { runtime } = setup(t, { approve: async p => { request = p; return { decision: 'accept' }; } });
  try {
    await runtime.initialize();
    await runtime.prompt('approval');
    assert.equal(request.method, 'item/commandExecution/requestApproval');
    assert.deepEqual(request.params.availableDecisions, ['accept', 'decline', 'cancel']);
  } finally { await runtime.close(); }
  const params = { options: [{ kind: 'allow_once', optionId: 'yes' }, { kind: 'reject_once', optionId: 'no' }, { kind: 'allow_always', optionId: 'always' }] };
  assert.deepEqual(permissionOutcome(params, 'accept'), { outcome: { outcome: 'selected', optionId: 'yes' } });
  for (const decision of ['acceptForSession', {}, null, 'bad']) assert.equal(permissionOutcome(params, decision).outcome.outcome, 'cancelled');
});

test('cutex MCP declaration only passes explicit allowlisted env', () => {
  const env = { CUTEX_AGENT_ID: 'dsh.spike', CUTEX_RUNTIME_GENERATION: '1', CUTEX_AGENT_BUS_URL: 'http://127.0.0.1:24391', CUTEX_AGENT_BUS_TOKEN: 'mock-only', UNRELATED_SECRET: 'must-not-forward' };
  const mcp = cutexMcp(process.execPath, env);
  assert.equal(mcp.name, 'cutex');
  assert.equal(mcp.env.length, 4);
  assert.throws(() => cutexMcp('relative.exe', env), /absolute/);
});

test('transport death preserves partial history and settles the turn', async t => {
  let thought;
  const seen = new Promise(resolve => { thought = resolve; });
  const { runtime } = setup(t, { emit: e => { if (e.method === 'item/reasoning/textDelta') thought(); } });
  await runtime.initialize();
  const prompt = runtime.prompt('wait');
  await seen;
  runtime.rpc.child.kill();
  await assert.rejects(prompt, /ACP exited/);
  assert.equal(runtime.replay().at(-1).params.turn.status, 'failed');
  assert.ok(runtime.replay().some(e => e.method === 'item/reasoning/textDelta'));
  await runtime.close();
});

test('hard controller-process crash retains SQLite prefix and marks interrupted on resume', async t => {
  const { config, runtime } = setup(t);
  await runtime.close();
  const worker = fork(fileURLToPath(new URL('./crash-worker.mjs', import.meta.url)), [config.cwd], { env: {}, stdio: ['ignore', 'ignore', 'pipe', 'ipc'] });
  const [ready] = await once(worker, 'message');
  const closed = once(worker, 'exit');
  // Both pids belong to this fixture; no live service discovery or tree-wide kill.
  worker.kill('SIGKILL');
  process.kill(ready.pid, 'SIGKILL');
  await closed;
  const restarted = new DshRuntime(config);
  try {
    await restarted.initialize();
    assert.equal(restarted.sessionId, ready.sessionId);
    const events = restarted.replay();
    assert.ok(events.some(e => e.method === 'item/reasoning/textDelta'));
    assert.equal(events.at(-1).params.turn.status, 'interrupted');
    assert.equal(events.filter(e => e.method === 'turn/completed').length, 1);
    await restarted.prompt('new turn');
    assert.equal(restarted.replay().at(-1).params.turn.status, 'completed');
  } finally { await restarted.close(); }
});

test('cancel while owner approval is unanswered fails closed and releases the prompt', async t => {
  let requested;
  const seen = new Promise(resolve => { requested = resolve; });
  const { runtime } = setup(t, { approve: async request => { requested(request); return new Promise(() => {}); } });
  try {
    await runtime.initialize();
    const prompt = runtime.prompt('approval');
    const request = await seen;
    runtime.cancel();
    assert.equal((await prompt).stopReason, 'cancelled');
    assert.equal(request.signal.aborted, true);
  } finally { await runtime.close(); }
});

function barrier() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

for (const action of ['cancel', 'resume', 'close', 'replace-turn']) {
  test(`native permission waiting for sync cannot approve after ${action}`, async () => {
    const scanning = barrier(), scanned = barrier();
    const runtime = Object.create(DshRuntime.prototype);
    let approvals = 0;
    Object.assign(runtime, {
      sessionId: 'private-session', turnId: 'old-turn', turnEpoch: Symbol('old-turn'),
      turnController: new AbortController(), sessionsRoot: 'unused', calls: new Map(),
      rpc: { send() {} },
      syncNative: async () => { scanning.resolve(); await scanned.promise; runtime.calls.set('tool', { name: 'fake', arguments: {} }); },
      approve: async () => { approvals++; return { decision: 'accept' }; },
    });
    const permission = runtime.permission({ sessionId: runtime.sessionId, toolCall: { toolCallId: 'tool' }, options: [{ kind: 'allow_once', optionId: 'allow' }] });
    await scanning.promise;
    if (action === 'cancel') runtime.cancel();
    else if (action === 'close') { runtime.closed = true; runtime.turnController.abort(); }
    else if (action === 'resume') { runtime.turnEpoch = Symbol('resume'); }
    else runtime.turnId = 'new-turn';
    scanned.resolve();
    assert.deepEqual(await permission, { outcome: { outcome: 'cancelled' } });
    assert.equal(approvals, 0);
  });
}

test('cancel releases native permission while the scanner is still blocked', async () => {
  const scanning = barrier(), scanned = barrier();
  const runtime = Object.create(DshRuntime.prototype);
  Object.assign(runtime, { sessionId: 'private-session', turnId: 'turn', turnEpoch: Symbol(), turnController: new AbortController(),
    sessionsRoot: 'unused', calls: new Map(), rpc: { send() {} },
    syncNative: () => { scanning.resolve(); return scanned.promise; }, approve: () => assert.fail('cancelled permission reached owner') });
  const permission = runtime.permission({ sessionId: runtime.sessionId, toolCall: { toolCallId: 'tool' } });
  await scanning.promise;
  runtime.cancel();
  assert.deepEqual(await permission, { outcome: { outcome: 'cancelled' } });
  scanned.resolve();
});

test('owner answer cannot cross a turn fence while permission is pending', async () => {
  const asked = barrier(), answer = barrier();
  const runtime = Object.create(DshRuntime.prototype);
  Object.assign(runtime, { sessionId: 'private-session', turnId: 'old-turn', turnEpoch: Symbol(), turnController: new AbortController(),
    calls: new Map([['tool', { name: 'fake', arguments: {} }]]),
    approve: request => { asked.resolve(request); return answer.promise; } });
  const permission = runtime.permission({ sessionId: runtime.sessionId, toolCall: { toolCallId: 'tool' }, options: [{ kind: 'allow_once', optionId: 'allow' }] });
  const request = await asked.promise;
  assert.equal(request.params.turnId, 'old-turn');
  runtime.turnId = 'new-turn';
  answer.resolve({ decision: 'accept' });
  assert.deepEqual(await permission, { outcome: { outcome: 'cancelled' } });
  assert.equal(request.signal.aborted, true);
});

function configOptions(modelValue = 'opaque-default', effortValue = 'opaque-low') {
  return [
    { id: 'model-selector', category: 'model', type: 'select', currentValue: modelValue, options: [{ group: 'provider', options: [
      { name: 'DeepSeek V4 Flash', value: 'opaque-flash' }, { name: 'DeepSeek V4 Pro', value: 'opaque-pro' },
    ] }] },
    { id: 'effort-selector', category: 'thought_level', type: 'select', currentValue: effortValue, options: [
      { name: 'Off', value: 'opaque-off' }, { name: 'Low', value: 'opaque-low' }, { name: 'High', value: 'opaque-high' }, { name: 'Max', value: 'opaque-max' },
    ] },
  ];
}

function configuredRpc(runtime, state, calls) {
  const request = runtime.rpc.request.bind(runtime.rpc);
  runtime.rpc.request = async (method, params, ...rest) => {
    calls.push({ method, params });
    if (['session/new', 'session/resume'].includes(method)) return { sessionId: 'configured-session', configOptions: structuredClone(state.options) };
    if (method === 'session/set_config_option') {
      state.options.find(option => option.id === params.configId).currentValue = params.value;
      return { configOptions: structuredClone(state.options) };
    }
    return request(method, params, ...rest);
  };
}

test('launch selection uses returned opaque values, persists; live changes persist and are re-applied on resume', async t => {
  const { config, runtime } = setup(t, { model: 'deepseek-v4-flash', reasoningEffort: 'high' });
  const state = { options: configOptions() }, calls = [];
  configuredRpc(runtime, state, calls);
  const setCalls = list => list.filter(call => call.method === 'session/set_config_option').map(call => call.params.value);
  try {
    await runtime.initialize(() => []);
    assert.deepEqual(calls.filter(call => call.method === 'session/set_config_option').map(call => call.params), [
      { sessionId: runtime.sessionId, configId: 'model-selector', value: 'opaque-flash' },
      { sessionId: runtime.sessionId, configId: 'effort-selector', value: 'opaque-high' },
    ]);
    assert.equal(runtime.selection.model, 'deepseek-v4-flash');
    assert.equal(runtime.selection.reasoningEffort, 'high');
    assert.equal(runtime.replay().filter(event => event.method === 'dsh/launchSelection').length, 1);
    assert.deepEqual(runtime.modelList().map(model => [model.id, model.isDefault]), [['DeepSeek V4 Flash', true], ['DeepSeek V4 Pro', false]]);
    assert.deepEqual(runtime.modelList()[0].supportedReasoningEfforts.map(effort => effort.reasoningEffort), ['off', 'low', 'high', 'max']);
  } finally { await runtime.close(); }
  const restored = new DshRuntime(config);
  const resumedCalls = [];
  configuredRpc(restored, state, resumedCalls);
  try {
    await restored.initialize();
    assert.equal(restored.selection.reasoningEffort, 'high');
    assert.deepEqual(setCalls(resumedCalls), [], 'an unchanged resume sets nothing');
    assert.equal(restored.replay().filter(event => event.method === 'dsh/launchSelection').length, 1);
    // A live change on the existing chat: applied now, persisted for later.
    const changed = await restored.updateSelection({ model: 'DeepSeek V4 Pro', effort: 'low' });
    assert.deepEqual([changed.model, changed.reasoningEffort, changed.modelValue, changed.effortValue], ['DeepSeek V4 Pro', 'low', 'opaque-pro', 'opaque-low']);
    assert.deepEqual(setCalls(resumedCalls), ['opaque-pro', 'opaque-low']);
    await assert.rejects(restored.updateSelection({ effort: 'medium' }), /unsupported reasoning effort/);
    await assert.rejects(restored.updateSelection({ model: 'missing-model' }), /unavailable/);
  } finally { await restored.close(); }
  // The launch request only seeds a fresh session: the persisted change wins.
  const relaunched = new DshRuntime({ ...config, model: 'deepseek-v4-flash', reasoningEffort: 'high' });
  // DSH resumed the route of its last request (flash): cutex re-applies the change.
  const drifted = { options: configOptions('opaque-flash', 'opaque-high') }, driftCalls = [];
  configuredRpc(relaunched, drifted, driftCalls);
  try {
    await relaunched.initialize();
    assert.equal(relaunched.selection.modelValue, 'opaque-pro');
    assert.deepEqual(setCalls(driftCalls), ['opaque-pro', 'opaque-low']);
  } finally { await relaunched.close(); }
});

// DSH 0.2.1-rc.3's catalog as ACP lists it: route values, display names.
function catalogOptions() {
  const options = configOptions('opaque-default');
  options[0].options[0].options = [
    { name: 'DeepSeek-V4.1-Flash', value: JSON.stringify(['deepseek-official', 'deepseek-flash']) },
    { name: 'DeepSeek-V4-Pro', value: JSON.stringify(['deepseek-official', 'deepseek-v4-pro']) },
  ];
  return options;
}

test('effort-only launch defaults to flash and unavailable choices fail closed', async t => {
  const { runtime } = setup(t, { reasoningEffort: 'off' });
  configuredRpc(runtime, { options: catalogOptions() }, []);
  try { await runtime.initialize(); assert.equal(runtime.selection.model, 'deepseek-flash'); assert.equal(runtime.selection.modelValue, JSON.stringify(['deepseek-official', 'deepseek-flash'])); assert.equal(runtime.selection.effortValue, 'opaque-off'); }
  finally { await runtime.close(); }
  // An agent created with the pre-r265 default keeps starting: DeepSeek serves it by Flash.
  const legacy = setup(t, { model: 'deepseek-v4-flash', reasoningEffort: 'high' }).runtime;
  configuredRpc(legacy, { options: catalogOptions() }, []);
  try { await legacy.initialize(); assert.equal(legacy.selection.model, 'deepseek-v4-flash'); assert.equal(legacy.selection.modelValue, JSON.stringify(['deepseek-official', 'deepseek-flash'])); }
  finally { await legacy.close(); }
  const invalid = setup(t, { model: 'missing-model' }).runtime;
  configuredRpc(invalid, { options: configOptions() }, []);
  try { await assert.rejects(invalid.initialize(), /unavailable/); }
  finally { await invalid.close(); }
});

test('secrets are removed before raw ACP and authoritative native bodies reach SQLite', async t => {
  const secret = 'fake-deepseek-key-only-for-tests';
  const { runtime } = setup(t, { env: { DEEPSEEK_API_KEY: secret } });
  try {
    await runtime.initialize();
    await runtime.prompt(secret);
    runtime.update({ sessionId: runtime.sessionId, update: { sessionUpdate: 'config_option_update', configOptions: [{ name: secret }] } });
    let previousCursor = null;
    for (const [nativeSeq, type, data] of [
      [0, 'turn/start', { turn: 1 }],
      [1, 'tool/call', { turn: 1, step: 1, callId: 'native-tool', name: 'echo', arguments: JSON.stringify({ text: secret }) }],
    ]) {
      const cursor = { version: 1, sessionId: runtime.sessionId, nativeSeq, hash: nativeSeq.toString(16).padStart(64, '0') };
      runtime.store.commitNative({ sessionId: runtime.sessionId, nativeSeq, nativeId: JSON.stringify([runtime.sessionId, nativeSeq]),
        event: { seq: nativeSeq, type, data }, updates: [{ rawInput: secret }], previousCursor, cursor });
      previousCursor = cursor;
    }
    for (const table of ['events', 'native_events', 'native_items', 'native_cursors']) {
      assert.ok(!JSON.stringify(runtime.store.db.prepare(`SELECT * FROM ${table}`).all()).includes(secret), table);
    }
    assert.ok(JSON.stringify(runtime.replay()).includes('[REDACTED]'));
  } finally { await runtime.close(); }
});
