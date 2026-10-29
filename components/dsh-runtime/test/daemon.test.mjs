import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createServer } from 'node:net';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { once } from 'node:events';
import { createRequire } from 'node:module';
const install = process.env.DSH_SPIKE_INSTALL;
const binary = process.env.DSH_SPIKE_DAEMON;
const bootDelayText = process.env.DSH_SPIKE_BOOT_DELAY_MS;
const bootDelayMs = bootDelayText === undefined ? 0 : Number(bootDelayText);
if (bootDelayText !== undefined && (!/^[1-9]\d*$/.test(bootDelayText) || !Number.isSafeInteger(bootDelayMs) || bootDelayMs > 120000)) {
  throw new Error('DSH_SPIKE_BOOT_DELAY_MS must be an integer from 1 through 120000');
}
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

async function port() {
  const server = createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  assert.ok(![24270, 24260, 25370, 25360].includes(port));
  return port;
}
async function poll(read, accept, message, timeout = 60000) {
  const end = Date.now() + timeout;
  let last;
  while (Date.now() < end) {
    try { last = await read(); if (accept(last)) return last; } catch {}
    await delay(200);
  }
  throw new Error(`${message}: ${JSON.stringify(last)}`);
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exit = once(child, 'exit');
  child.kill('SIGKILL');
  await exit;
}

test('private cutex daemon: Dsh backend, Management approval, actual restart and timeline replay', {
  skip: !install || !binary || process.platform !== 'win32', timeout: 300000,
}, async t => {
  const base = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, 'daemon-'));
  if (process.platform === 'win32') {
    const identity = spawnSync('whoami.exe', ['/user', '/fo', 'csv', '/nh'], { encoding: 'utf8' });
    const principal = identity.stdout.match(/S-1-5-21-[0-9-]+/)?.[0];
    assert.ok(principal, 'current test user SID unavailable');
    const acl = spawnSync('icacls.exe', [home, '/inheritance:r', '/grant:r', `*${principal}:(OI)(CI)F`, '*S-1-5-18:(OI)(CI)F'], { encoding: 'utf8' });
    assert.equal(acl.status, 0, 'private HOME ACL setup failed');
  }
  const configDir = join(home, '.cutex');
  const cwd = join(home, 'workspace');
  mkdirSync(configDir); mkdirSync(cwd);
  writeFileSync(join(home, '.cutex-test-private-home'), 'DSH private daemon acceptance\n');
  const busPort = await port();
  const managementPort = await port();
  const token = 'dsh-spike-only-mock-root-credential';
  writeFileSync(join(configDir, 'config.json'), JSON.stringify({ agent_bus_enabled: true, agent_bus_port: busPort, agent_bus_token: 'dsh-spike-only-separate-mock-bus-credential', management_port: managementPort }));
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT'].filter(k => process.env[k]).map(k => [k, process.env[k]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'), CUTEX_TEST_PRIVATE_HOME: home, CUTEX_TEST_FORBID_REAL_HOME: '1', CUTEX_DSH_SPIKE: '1' });
  const children = [];
  const logs = [];
  function start(args) {
    const child = spawn(resolve(binary), args, { env, cwd, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    child.stdout.on('data', chunk => logs.push(String(chunk)));
    child.stderr.on('data', chunk => logs.push(String(chunk)));
    children.push(child);
    return child;
  }
  const url = `http://127.0.0.1:${managementPort}`;
  async function api(path, body, headers = {}) {
    const response = await fetch(url + path, { method: body ? 'POST' : 'GET', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', ...headers }, ...(body ? { body: JSON.stringify(body) } : {}), signal: AbortSignal.timeout(180000) });
    const value = await response.json();
    assert.ok(response.ok, `${path}: ${response.status} ${JSON.stringify(value)}`);
    return value;
  }
  let adapterReady;
  let adapterToken;
  let accepted = false;
  t.after(async () => {
    if (!accepted) console.error(logs.join('').replaceAll(token, '[mock-root]').slice(-6000));
    // Adapter is independent of daemon lifetime: shut its exact authenticated
    // endpoint down first, then stop only the fixture's known daemon pids.
    if (adapterReady && adapterToken) {
      try {
        adapterReady = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
        adapterToken = JSON.parse(readFileSync(join(stateDir, 'launch.json'), 'utf8')).token;
        const require = createRequire(join(resolve(install), 'package.json'));
        const { WebSocket } = require('ws');
        const ws = new WebSocket(adapterReady.endpoint, { headers: { Authorization: `Bearer ${adapterToken}` } });
        await once(ws, 'open');
        ws.send(JSON.stringify({ id: 1, method: 'initialize', params: { capabilities: {} } }));
        await once(ws, 'message');
        ws.send(JSON.stringify({ method: 'initialized' }));
        ws.send(JSON.stringify({ id: 2, method: 'dsh/server/shutdown', params: {} }));
        await Promise.race([once(ws, 'close'), delay(5000)]);
        ws.terminate();
      } catch {}
    }
    for (const child of children.reverse()) await stop(child);
    await delay(300);
    rmSync(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  });
  let bus = start(['agent', 'serve', '--port', String(busPort)]);
  let management = start(['management', 'serve', '--port', String(managementPort), '--token', token]);
  await poll(() => api('/v2/sessions'), v => v.contractVersion === 2, 'private Management readiness');
  const patch = join(home, 'offline.patch.yml');
  const plugin = fileURLToPath(new URL('./mock-llm.mjs', import.meta.url)).replaceAll('\\', '/');
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config: { provider: cutex-offline, model: offline }
- id: session-persistence-jsonl
  config:
    root: !!js process.env.DSH_SPIKE_SESSIONS_ROOT
    compression: none
- id: session-log-deepseek
  config: { enabled: false }
- insert:
    - id: cutex-spike-llm
      name: '${plugin}'
`);
  let dshCli = join(resolve(install), 'node_modules/@deepseek-ai/dsh/lib/bin.js');
  const bootLog = join(home, 'native-boots.jsonl');
  if (bootDelayMs) {
    const nativeEntry = pathToFileURL(dshCli).href;
    dshCli = join(home, 'delayed-dsh-entry.mjs');
    // Delay only the private entry point. The pinned native CLI owns ACP,
    // session storage, MCP and model execution after this await.
    writeFileSync(dshCli, `import { appendFileSync } from 'node:fs';
const started = Date.now();
await new Promise(resolve => setTimeout(resolve, ${bootDelayMs}));
appendFileSync(${JSON.stringify(bootLog)}, JSON.stringify({ pid: process.pid, waitedMs: Date.now() - started }) + '\\n');
const { runCli } = await import(${JSON.stringify(nativeEntry)});
await runCli();
`);
  }
  const created = await api('/v2/experimental/dsh/sessions', { name: 'offline-dsh-acceptance', cwd, node: process.execPath,
    adapter: fileURLToPath(new URL('../src/server.mjs', import.meta.url)), dshCli, patch, installRoot: resolve(install) });
  const id = created.cutexSessionId;
  const threadId = created.threadId;
  const stateDir = join(configDir, 'runtime', 'dsh', id);
  adapterReady = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
  adapterToken = JSON.parse(readFileSync(join(stateDir, 'launch.json'), 'utf8')).token;
  const sessionPath = `/v2/sessions/${encodeURIComponent(id)}`;
  await api(sessionPath + '/cutex/requests', { requestId: crypto.randomUUID(), method: 'cutex/runtime/online', params: { expectedRuntimeGeneration: created.runtimeGeneration, openVisibleTerminal: false, reason: 'dsh_spike_acceptance' } });
  const online = await poll(() => api(sessionPath), v => v.runtime?.appServerConnected, 'DSH late adoption');
  assert.equal(online.runtime.backend, 'dsh');
  assert.match(online.native.schema.sha256, /^[a-f0-9]{64}$/);
  assert.equal(online.native.threadId, threadId);
  const generation = online.runtime.runtimeGeneration;
  const native = async (method, params) => api(sessionPath + '/app-server/requests', { requestId: crypto.randomUUID(), native: { message: { id: crypto.randomUUID(), method, params: { threadId, ...params } } } });
  await native('turn/start', { input: [{ type: 'text', text: 'query the private agent list' }] });
  const pending = await poll(() => api(sessionPath), v => v.runtime.pendingServerRequests?.length > 0, 'Management approval pending');
  const request = pending.runtime.pendingServerRequests[0];
  const message = request.native?.message ?? request.native ?? request;
  assert.equal(message.method, 'item/commandExecution/requestApproval');
  await api(sessionPath + '/app-server/server-request-responses', { responseId: crypto.randomUUID(), native: { message: { id: message.id, result: { decision: 'accept' } } } }, { 'If-Match': `"cutex-runtime-generation:${generation}"` });
  const before = await poll(() => api(sessionPath + '/history'), v => v.native?.result?.thread?.turns?.some(turn => turn.status === 'completed'), 'completed DSH history');
  const turns = before.native.result.thread.turns;
  assert.ok(turns.some(turn => turn.items.some(item => item.type === 'agentMessage' && item.text.includes('agents'))), JSON.stringify(turns.flatMap(turn => turn.items).filter(item => item.type === 'agentMessage')));
  assert.equal(turns.length, 1);
  const oldPid = management.pid;
  const oldBusPid = bus.pid;
  await stop(management);
  await stop(bus);
  bus = start(['agent', 'serve', '--port', String(busPort)]);
  management = start(['management', 'serve', '--port', String(managementPort), '--token', token]);
  assert.notEqual(management.pid, oldPid);
  const restored = await poll(() => api(sessionPath), v => v.runtime?.appServerConnected, 'DSH adoption after actual daemon restart');
  assert.equal(restored.runtime.runtimeGeneration, generation);
  const after = await api(sessionPath + '/history');
  assert.deepEqual(after.native.result.thread.turns, turns);
  assert.equal(after.threadId, threadId);
  await api(sessionPath + '/cutex/requests', { requestId: crypto.randomUUID(), method: 'cutex/runtime/close', params: { expectedRuntimeGeneration: generation, reason: 'dsh_spike_acceptance' } });
  await api(sessionPath + '/cutex/requests', { requestId: crypto.randomUUID(), method: 'cutex/runtime/online', params: { expectedRuntimeGeneration: generation, openVisibleTerminal: false, reason: 'dsh_spike_acceptance' } });
  const relaunched = await poll(() => api(sessionPath), v => v.runtime?.appServerConnected, 'native DSH relaunch');
  assert.equal(relaunched.runtime.runtimeGeneration, generation + 1);
  assert.equal(relaunched.native.threadId, threadId);
  assert.deepEqual((await api(sessionPath + '/history')).native.result.thread.turns, turns);
  const originalAdapterPid = adapterReady.pid;
  adapterReady = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
  adapterToken = JSON.parse(readFileSync(join(stateDir, 'launch.json'), 'utf8')).token;
  assert.notEqual(adapterReady.pid, originalAdapterPid);
  if (bootDelayMs) {
    const boots = readFileSync(bootLog, 'utf8').trim().split('\n').map(line => JSON.parse(line));
    assert.equal(boots.length, 2, 'exactly two actual native process boots must pass the delay');
    assert.notEqual(boots[0].pid, boots[1].pid);
    assert.ok(boots.every(boot => boot.waitedMs >= bootDelayMs), 'native CLI imported before the requested delay');
  }
  // A resumed native session must still execute a real mock-provider turn and
  // its MCP request, rather than merely expose the persisted first turn.
  await native('turn/start', { input: [{ type: 'text', text: 'query the private agent list after native resume' }] });
  const resumedPending = await poll(() => api(sessionPath), v => v.runtime.pendingServerRequests?.length > 0, 'resumed Management approval pending');
  const resumedRequest = resumedPending.runtime.pendingServerRequests[0];
  const resumedMessage = resumedRequest.native?.message ?? resumedRequest.native ?? resumedRequest;
  assert.equal(resumedMessage.method, 'item/commandExecution/requestApproval');
  await api(sessionPath + '/app-server/server-request-responses', { responseId: crypto.randomUUID(), native: { message: { id: resumedMessage.id, result: { decision: 'accept' } } } }, { 'If-Match': `"cutex-runtime-generation:${generation + 1}"` });
  const resumedHistory = await poll(() => api(sessionPath + '/history'), v => v.native?.result?.thread?.turns?.length === 2 && v.native.result.thread.turns.every(turn => turn.status === 'completed'), 'resumed native model and MCP execution');
  const resumedTurns = resumedHistory.native.result.thread.turns;
  assert.deepEqual(resumedTurns[0], turns[0]);
  assert.ok(resumedTurns[1].items.some(item => item.type === 'agentMessage' && item.text.includes('agents')), 'resumed mock model did not consume the actual MCP tool result');
  assert.equal(resumedHistory.threadId, threadId);
  console.log(JSON.stringify({ cutexSessionId: id, threadId, runtimeGeneration: generation, resumedGeneration: generation + 1, oldDaemonPid: oldPid, newDaemonPid: management.pid, oldBusPid, newBusPid: bus.pid, originalAdapterPid, resumedAdapterPid: adapterReady.pid, historyTurns: resumedTurns.length, approvalsViaManagement: 2, bootDelayMs, busPort, managementPort }));
  accepted = true;
});
