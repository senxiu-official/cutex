import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createServer, connect } from 'node:net';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync, readdirSync, chmodSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';
import { createRequire } from 'node:module';
import { DatabaseSync } from 'node:sqlite';
import { locateNativeLog } from '../src/reconcile.mjs';

// Explicit opt-in only. No binary discovery, owner environment, credentials,
// default daemon ports or remote model endpoints are used by this fixture.
const install = process.env.DSH_PRODUCT_INSTALL ?? process.env.DSH_SPIKE_INSTALL;
const binary = process.env.DSH_PRODUCT_DAEMON ?? process.env.DSH_SPIKE_DAEMON;
const delay = ms => new Promise(done => setTimeout(done, ms));
const uuid = () => crypto.randomUUID();

async function freePort() {
  const server = createServer();
  await new Promise(done => server.listen(0, '127.0.0.1', done));
  const port = server.address().port;
  await new Promise(done => server.close(done));
  assert.ok(![24270, 24260, 25370, 25360].includes(port));
  return port;
}
async function poll(read, accepts, description, timeout = 45000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    try { last = await read(); if (accepts(last)) return last; }
    catch (error) { last = error.message; }
    await delay(100);
  }
  throw new Error(`Timed out: ${description}; last=${JSON.stringify(last)}`);
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  child.kill('SIGKILL');
  await exited;
}
function running(pid) {
  try { process.kill(pid, 0); return true; }
  catch (error) { if (error.code === 'ESRCH') return false; throw error; }
}
function fullTurns(turns) {
  return turns.map(({ itemsView, ...turn }) => {
    if (itemsView !== undefined) assert.equal(itemsView, 'full', 'acceptance requires complete item history');
    return turn;
  });
}
function assertNoSecret(root, secret, deferred = null) {
  if (!existsSync(root)) return;
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    // Never follow a junction/symlink outside the fixture's marked home.
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) assertNoSecret(path, secret, deferred);
    else if (entry.isFile()) {
      let bytes;
      try { bytes = readFileSync(path); }
      catch (error) {
        if (deferred && ['EBUSY', 'EACCES', 'EPERM'].includes(error.code)) { deferred.add(path); continue; }
        error.message += ` (${path})`; throw error;
      }
      assert.ok(!bytes.includes(Buffer.from(secret)), `secret persisted in ${path}`);
    }
  }
}

// Fallback cleanup for the fixture's independent adapter. Only a ready record
// inside this fixture and its exact endpoint are trusted, never process names.
async function shutdownAdapter(stateDir, installRoot) {
  if (!stateDir || !existsSync(join(stateDir, 'ready.json'))) return;
  const ready = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
  const launch = JSON.parse(readFileSync(join(stateDir, 'launch.json'), 'utf8'));
  let transport;
  if (ready.endpoint.startsWith('unix://')) {
    const require = createRequire(join(installRoot, 'package.json'));
    const { WebSocket } = require('ws');
    const socket = new WebSocket(`ws+unix:${ready.endpoint.slice('unix://'.length)}:/`, { handshakeTimeout: 3000 });
    socket.on('error', () => {});
    await once(socket, 'open');
    transport = { send: value => socket.send(JSON.stringify(value)), close: () => socket.terminate(), events: socket, event: 'message' };
  } else {
    const require = createRequire(join(installRoot, 'package.json'));
    const { WebSocket } = require('ws');
    const socket = new WebSocket(ready.endpoint, { headers: { Authorization: `Bearer ${launch.token}` }, handshakeTimeout: 3000 });
    socket.on('error', () => {});
    await once(socket, 'open');
    transport = { send: value => socket.send(JSON.stringify(value)), close: () => socket.terminate(), events: socket, event: 'message' };
  }
  try {
    const initialized = once(transport.events, transport.event);
    transport.send({ id: 1, method: 'initialize', params: { capabilities: {} } });
    await Promise.race([initialized, delay(3000)]);
    transport.send({ method: 'initialized' });
    transport.send({ id: 2, method: 'dsh/server/shutdown', params: {} });
    await Promise.race([once(transport.events, 'close'), delay(3000)]);
  } finally { transport.close(); }
}

test('product DSH profile + agent CRUD, lazy facade, secret env, native send, offline history and stable relaunch', {
  skip: !install || !binary, timeout: 300000,
}, async t => {
  const installRoot = resolve(install), daemon = resolve(binary);
  const metadata = JSON.parse(readFileSync(join(installRoot, 'node_modules/@deepseek-ai/dsh/package.json'), 'utf8'));
  assert.equal(metadata.version, '0.2.1-rc.3');
  const base = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, 'product-'));
  if (process.platform === 'win32') {
    const identity = spawnSync('whoami.exe', ['/user', '/fo', 'csv', '/nh'], { encoding: 'utf8' });
    const sid = identity.stdout.match(/S-1-5-21-[0-9-]+/)?.[0];
    assert.ok(sid, 'fixture owner SID missing');
    assert.equal(spawnSync('icacls.exe', [home, '/inheritance:r', '/grant:r', `*${sid}:(OI)(CI)F`, '*S-1-5-18:(OI)(CI)F'], { encoding: 'utf8' }).status, 0);
  } else chmodSync(home, 0o700);
  writeFileSync(join(home, '.cutex-test-private-home'), 'keyless product DSH acceptance\n');
  const configDir = join(home, '.cutex'), cwd = join(home, 'workspace');
  mkdirSync(configDir); mkdirSync(cwd);
  const deploymentDir = join(configDir, 'runtime', 'dsh');
  mkdirSync(deploymentDir, { recursive: true });
  const deployment = { node: process.execPath, adapter: fileURLToPath(new URL('../src/server.mjs', import.meta.url)),
    dsh_cli: join(installRoot, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), install_root: installRoot, version: metadata.version };
  writeFileSync(join(deploymentDir, 'deployment.json'), JSON.stringify(deployment), { mode: 0o600 });
  const patch = join(home, 'offline.patch.yml');
  const mockModule = new URL('./mock-llm.mjs', import.meta.url).href;
  const pluginPath = join(home, 'product-mock-llm.mjs');
  // Validate the actual DSH child's environment without placing the dummy key
  // value in this file, the deployment, the patch, or a provider request.
  writeFileSync(pluginPath, `
import { apply as mockApply } from ${JSON.stringify(mockModule)};
export { inject } from ${JSON.stringify(mockModule)};
export const name = 'cutex-product-keyless-fixture';
export function apply(ctx) {
  if (!process.env.DEEPSEEK_API_KEY?.startsWith('dummy-product-api-key-')) throw new Error('Product key mapping did not reach DSH');
  if (process.env.DSH_PRODUCT_TEST_KEY !== undefined) throw new Error('Product source key leaked beyond explicit child allowlist');
  return mockApply(ctx);
}
`);
  const plugin = pluginPath.replaceAll('\\', '/').replaceAll("'", "''");
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config: { provider: cutex-offline, model: offline }
- id: session-log-deepseek
  config: { enabled: false }
- insert:
    - id: cutex-product-offline-llm
      name: '${plugin}'
`);
  const managementPort = await freePort();
  let busPort = await freePort();
  while (busPort === managementPort) busPort = await freePort();
  const rootToken = 'product-test-root-credential-only', busToken = 'product-test-separate-bus-credential';
  const secretName = 'DSH_PRODUCT_TEST_KEY', secret = `dummy-product-api-key-${uuid()}`;
  const deferredSecretFiles = new Set();
  const scanSecretsWhileRunning = () => assertNoSecret(home, secret, deferredSecretFiles);
  writeFileSync(join(configDir, 'config.json'), JSON.stringify({ agent_bus_enabled: true, agent_bus_port: busPort, agent_bus_token: busToken, management_port: managementPort }));
  const env = Object.fromEntries(['PATH', 'Path', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS']
    .filter(name => process.env[name]).map(name => [name, process.env[name]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, APPDATA: join(home, 'appdata'), LOCALAPPDATA: join(home, 'local'),
    CUTEX_TEST_PRIVATE_HOME: home, CUTEX_TEST_FORBID_REAL_HOME: '1', CUTEX_DSH_SPIKE: '1', CUTEX_DSH_TEST_PATCH: patch, [secretName]: secret });
  const children = [], logs = [];
  function start(args) {
    const child = spawn(daemon, args, { env, cwd, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    children.push(child);
    child.stdout.on('data', chunk => logs.push(String(chunk)));
    child.stderr.on('data', chunk => logs.push(String(chunk)));
    return child;
  }
  const url = `http://127.0.0.1:${managementPort}`;
  async function api(path, body, { method = body === undefined ? 'GET' : 'POST', headers = {}, expected = null } = {}) {
    const response = await fetch(url + path, { method, headers: { Authorization: `Bearer ${rootToken}`, 'Content-Type': 'application/json', ...headers },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(180000) });
    const result = await response.json();
    assert.ok(expected === null ? response.ok : response.status === expected, `${method} ${path}: ${response.status} ${JSON.stringify(result).replaceAll(secret, '[REDACTED]')}`);
    assert.ok(!JSON.stringify(result).includes(secret), 'Management exposed the dummy provider key');
    return result;
  }
  let stateDir, sessionPath, generation = 0, passed = false;
  const trackedSessions = [];
  t.after(async () => {
    if (!passed) {
      const redactLog = text => text.replaceAll(secret, '[REDACTED]').replaceAll(rootToken, '[root]').replaceAll(busToken, '[bus]');
      console.error(redactLog(logs.join('')).slice(-8000));
      for (const tracked of trackedSessions) {
        const log = join(tracked.stateDir, 'adapter.log');
        if (existsSync(log)) console.error(`Private fixture adapter log (${tracked.path}):\n${redactLog(readFileSync(log, 'utf8')).slice(-12000)}`);
        const launchFile = join(tracked.stateDir, 'launch.json');
        if (existsSync(launchFile)) {
          const launch = JSON.parse(readFileSync(launchFile, 'utf8'));
          const { token: _token, env: childEnv, mcpServers, ...metadata } = launch;
          console.error(`Sanitized private launch metadata: ${JSON.stringify({ ...metadata, envKeys: Object.keys(childEnv ?? {}),
            mcpServers: (mcpServers ?? []).map(({ name, command, args, env }) => ({ name, command, args, envNames: (env ?? []).map(entry => entry.name) })) })}`);
        }
      }
    }
    for (const tracked of trackedSessions) {
      try {
        const state = await api(tracked.path);
        await api(tracked.path + '/cutex/requests', { requestId: uuid(), method: 'cutex/runtime/close', params: { expectedRuntimeGeneration: state.runtime.runtimeGeneration, reason: 'product_fixture_cleanup' } });
      } catch {}
      try { await shutdownAdapter(tracked.stateDir, installRoot); } catch {}
    }
    for (const child of [...children].reverse()) await stop(child);
    await delay(200);
    if (!passed && process.env.DSH_PRODUCT_RETAIN_FAILURE === '1') {
      console.error(`Retained stopped private fixture for diagnosis: ${home}`);
      return;
    }
    rmSync(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  });
  let bus = start(['agent', 'serve', '--port', String(busPort)]);
  let management = start(['management', 'serve', '--port', String(managementPort), '--token', rootToken]);
  await poll(() => api('/v2/sessions'), value => value.contractVersion === 2, 'private Management ready');

  // Normal Profile CRUD, including the env-name-only credential contract.
  const { profile } = await api('/v2/profiles', { name: 'DSH Product Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_UNUSED_TEST_KEY' } } });
  assert.equal(profile.cliKind, 'dsh');
  const profilePath = `/v2/profiles/${encodeURIComponent(profile.id)}`;
  const changed = await api(profilePath, { options: { dsh: { model: 'offline', apiKeyEnv: secretName } } }, { method: 'PATCH' });
  assert.equal(changed.profile.options.dsh.apiKeyEnv, secretName);
  const detail = await api(profilePath);
  assert.equal((detail.profile ?? detail).options.dsh.model, 'offline');
  const listing = await api('/v2/profiles');
  assert.ok(listing.profiles.some(value => value.id === profile.id));
  const { profile: disposable } = await api('/v2/profiles', { name: 'DSH Disposable', kind: 'dsh' });
  await api(`/v2/profiles/${encodeURIComponent(disposable.id)}`, undefined, { method: 'DELETE' });
  await api(`/v2/profiles/${encodeURIComponent(disposable.id)}`, undefined, { expected: 404 });

  const created = await api('/v2/human/agents', { requestId: uuid(), name: 'product-offline-dsh', cwd, profile: profile.id, start: false });
  const id = created.cutexSessionId, facade = created.session.native.threadId;
  assert.match(facade, /^[0-9a-f-]{36}$/i);
  sessionPath = `/v2/sessions/${encodeURIComponent(id)}`;
  stateDir = join(deploymentDir, id);
  trackedSessions.push({ path: sessionPath, stateDir });
  function storedRecord() {
    const db = new DatabaseSync(join(configDir, 'cutex-sessions.sqlite3'), { readOnly: true });
    // DatabaseSync otherwise has a zero lock wait. Management can still be
    // publishing a revision just after its HTTP reply; wait for that short
    // transaction rather than making fixture inspection flaky on a busy host.
    try { db.exec('PRAGMA busy_timeout=5000'); return JSON.parse(db.prepare('SELECT record_json FROM sessions WHERE cutex_session_id=?').get(id).record_json); }
    finally { db.close(); }
  }
  async function restartDaemonsWithPrivateRecordChange(mutate) {
    // All observers are stopped before direct corruption of this one private
    // fixture row. Advance the store revision exactly as its SQLite CAS does.
    await stop(management); await stop(bus);
    const db = new DatabaseSync(join(configDir, 'cutex-sessions.sqlite3'));
    try {
      db.exec('PRAGMA busy_timeout=5000');
      db.exec('BEGIN IMMEDIATE');
      const row = db.prepare('SELECT record_json FROM sessions WHERE cutex_session_id=?').get(id);
      const record = JSON.parse(row.record_json);
      mutate(record);
      const revision = Number(db.prepare("SELECT value FROM meta WHERE key='store_revision'").get().value) + 1;
      db.prepare('UPDATE sessions SET record_json=?, rev=? WHERE cutex_session_id=?').run(JSON.stringify(record), revision, id);
      db.prepare("UPDATE meta SET value=? WHERE key='store_revision'").run(String(revision));
      db.exec('COMMIT');
    } catch (error) { db.exec('ROLLBACK'); throw error; }
    finally { db.close(); }
    bus = start(['agent', 'serve', '--port', String(busPort)]);
    management = start(['management', 'serve', '--port', String(managementPort), '--token', rootToken]);
    await poll(() => api('/v2/sessions'), value => value.contractVersion === 2, 'private daemons restarted after fixture row change');
  }
  const unstartedRecord = storedRecord();
  assert.equal(unstartedRecord.dsh_session.thread_id, facade);
  assert.equal(unstartedRecord.dsh_session.native_session_id ?? null, null);
  assert.equal(unstartedRecord.codex_session_id ?? null, null);
  assert.equal(unstartedRecord.app_server_runtime ?? null, null);
  const idle = await api(sessionPath);
  assert.equal(idle.runtime.backend, 'dsh');
  assert.equal(idle.runtime.runtimeGeneration, 0);
  assert.equal(idle.runtime.appServerConnected, false);
  for (const file of ['home', 'ready.json', 'launch.json', 'timeline.sqlite']) assert.equal(existsSync(join(stateDir, file)), false, `start:false created ${file}`);
  const empty = await api(sessionPath + '/history');
  assert.equal(empty.source, 'offline');
  assert.equal(empty.native.result.thread.id, facade);
  assert.deepEqual(empty.native.result.thread.turns, []);
  assert.equal((await api(sessionPath)).runtime.runtimeGeneration, 0);
  assert.equal(existsSync(join(stateDir, 'home')), false);
  assert.equal(existsSync(join(stateDir, 'timeline.sqlite')), false);

  const control = (method, expectedGeneration = generation) => api(sessionPath + '/cutex/requests', {
    requestId: uuid(), method, params: { expectedRuntimeGeneration: expectedGeneration,
      ...(method === 'cutex/runtime/online' ? { openVisibleTerminal: false } : {}), reason: 'product_dsh_acceptance' },
  });
  await control('cutex/runtime/online');
  const online = await poll(() => api(sessionPath), value => value.runtime?.appServerConnected, 'normal product runtime online');
  generation = online.runtime.runtimeGeneration;
  assert.equal(generation, 1);
  assert.equal(online.native.threadId, facade);
  const ready = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
  assert.equal(ready.threadId, facade);
  assert.ok(ready.nativeSessionId && ready.nativeSessionId !== facade);
  assert.equal(storedRecord().dsh_session.native_session_id, ready.nativeSessionId);
  assert.equal(storedRecord().codex_session_id ?? null, null);
  const launch = JSON.parse(readFileSync(join(stateDir, 'launch.json'), 'utf8'));
  assert.equal(launch.model, 'offline');
  assert.deepEqual(launch.secretEnvMappings, [{ source: secretName, target: 'DEEPSEEK_API_KEY' }]);
  assert.equal(launch.env.DEEPSEEK_API_KEY, undefined);
  assert.equal(launch.env[secretName], undefined);
  scanSecretsWhileRunning();

  async function submitAndComplete(text, expectedTurns) {
    await api(sessionPath + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method: 'turn/start', params: { threadId: facade, input: [{ type: 'text', text }] } } } });
    // cutex MCP tools are pre-approved (r265): the turn runs without an ask.
    const done = await poll(() => api(sessionPath + '/history'), value => {
      const turns = value.native?.result?.thread?.turns;
      return turns?.length === expectedTurns && turns.every(turn => turn.status === 'completed');
    }, `product native completed history ${expectedTurns}`);
    assert.equal((await api(sessionPath)).runtime.pendingServerRequests.length, 0, 'no approval for a cutex tool');
    return done;
  }
  const completed = await submitAndComplete('query the private agent list', 1);
  const turns = fullTurns(completed.native.result.thread.turns);
  assert.equal(turns.length, 1);
  assert.ok(turns[0].items.some(item => item.type === 'agentMessage' && item.text.includes('agents')));
  assert.equal(completed.threadId, facade);

  await control('cutex/runtime/close');
  await poll(() => api(sessionPath), value => !value.runtime?.appServerConnected, 'product runtime stopped');
  await poll(async () => existsSync(join(stateDir, 'ready.json')), exists => !exists, 'adapter ready file removed');
  const offlineBefore = await api(sessionPath);
  const launchBefore = readFileSync(join(stateDir, 'launch.json'));
  const timelineBefore = readFileSync(join(stateDir, 'timeline.sqlite'));
  const offline = await api(sessionPath + '/history');
  assert.equal(offline.source, 'offline');
  assert.equal(offline.native.result.thread.id, facade);
  assert.deepEqual(fullTurns(offline.native.result.thread.turns), turns);
  const offlineAfter = await api(sessionPath);
  assert.equal(offlineAfter.runtime.runtimeGeneration, offlineBefore.runtime.runtimeGeneration);
  assert.equal(offlineAfter.runtime.appServerConnected, false);
  assert.equal(existsSync(join(stateDir, 'ready.json')), false);
  assert.deepEqual(readFileSync(join(stateDir, 'launch.json')), launchBefore);
  assert.deepEqual(readFileSync(join(stateDir, 'timeline.sqlite')), timelineBefore, 'offline history mutated the projection database');
  assert.equal(storedRecord().app_server_runtime ?? null, null);
  scanSecretsWhileRunning();

  await control('cutex/runtime/online');
  const restored = await poll(() => api(sessionPath), value => value.runtime?.appServerConnected, 'product native relaunch');
  assert.equal(restored.runtime.runtimeGeneration, generation + 1);
  generation = restored.runtime.runtimeGeneration;
  const resumed = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
  assert.equal(resumed.threadId, facade);
  assert.equal(resumed.nativeSessionId, ready.nativeSessionId);
  assert.equal(storedRecord().dsh_session.native_session_id, ready.nativeSessionId);
  assert.notEqual(resumed.pid, ready.pid);
  assert.deepEqual(fullTurns((await api(sessionPath + '/history')).native.result.thread.turns), turns);
  const afterResume = await submitAndComplete('query the private agent list after native resume', 2);
  const resumedTurns = fullTurns(afterResume.native.result.thread.turns);
  assert.deepEqual(resumedTurns.slice(0, 1), turns, 'resumed prompt changed or duplicated the prior native turn');
  assert.notEqual(resumedTurns[1].id, turns[0].id);
  assert.ok(resumedTurns[1].items.some(item => item.type === 'agentMessage' && item.text.includes('agents')));
  assert.equal(JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8')).nativeSessionId, ready.nativeSessionId);

  // A second normal product agent is the control: cleanup must target one
  // named job/scope, not all Node/DSH processes or all agents on this host.
  const sentinel = await api('/v2/human/agents', { requestId: uuid(), name: 'product-dsh-scope-sentinel', cwd, profile: profile.id, start: true });
  const sentinelPath = `/v2/sessions/${encodeURIComponent(sentinel.cutexSessionId)}`;
  const sentinelStateDir = join(deploymentDir, sentinel.cutexSessionId);
  trackedSessions.push({ path: sentinelPath, stateDir: sentinelStateDir });
  await poll(() => api(sentinelPath), value => value.runtime?.appServerConnected, 'sentinel runtime online');
  const sentinelReady = JSON.parse(readFileSync(join(sentinelStateDir, 'ready.json'), 'utf8'));

  const orphanPid = resumed.pid;
  await restartDaemonsWithPrivateRecordChange(record => {
    record.app_server_runtime = null;
    record.runtime_pid = null;
    record.current_runtime_agent_id = null;
  });
  assert.ok(running(orphanPid), 'fixture did not retain the adapter orphan after daemon restart');
  assert.ok(running(sentinelReady.pid), 'unrelated sentinel did not survive daemon restart');
  assert.equal(storedRecord().app_server_runtime ?? null, null);
  let recoveredReady, orphanRecovered = false, brokenEndpointCleaned = false;
  await t.test('lost binding recovers the named orphan without killing another agent', async () => {
    await control('cutex/runtime/online');
    const reclaimed = await poll(() => api(sessionPath), value => value.runtime?.appServerConnected, 'binding-less named orphan recovery');
    generation++;
    assert.equal(reclaimed.runtime.runtimeGeneration, generation);
    await poll(async () => running(orphanPid), alive => !alive, 'old named orphan adapter terminated');
    recoveredReady = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
    assert.equal(recoveredReady.nativeSessionId, ready.nativeSessionId);
    assert.equal(recoveredReady.threadId, facade);
    assert.notEqual(recoveredReady.pid, orphanPid);
    assert.deepEqual(fullTurns((await api(sessionPath + '/history')).native.result.thread.turns), resumedTurns);
    assert.ok(running(sentinelReady.pid), 'orphan cleanup killed the unrelated sentinel scope');
    orphanRecovered = true;
  });
  if (!orphanRecovered) {
    // Keep the failed subtest failed. Restore only this private fixture to
    // exercise the independent broken-endpoint case in the same execution.
    generation = (await api(sessionPath)).runtime.runtimeGeneration;
    await control('cutex/runtime/close');
    await poll(async () => running(orphanPid), alive => !alive, 'failed-orphan fixture cleanup');
    await control('cutex/runtime/online');
    const restoredFixture = await poll(() => api(sessionPath), value => value.runtime?.appServerConnected, 'fixture restored for broken-endpoint case');
    generation = restoredFixture.runtime.runtimeGeneration;
    recoveredReady = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
    assert.equal(recoveredReady.nativeSessionId, ready.nativeSessionId);
  }

  const deadPort = await freePort();
  const savedNode = storedRecord().dsh_session.node;
  const missingNode = join(home, 'nonexistent-node-component', process.platform === 'win32' ? 'node.exe' : 'node');
  assert.equal(existsSync(missingNode), false);
  assert.equal(existsSync(savedNode), true);
  await restartDaemonsWithPrivateRecordChange(record => {
    assert.equal(record.app_server_runtime.pid, recoveredReady.pid);
    const binding = record.app_server_runtime;
    binding.endpoint = binding.endpoint.startsWith('unix://')
      ? `unix://${join(dirname(binding.endpoint.slice('unix://'.length)), 'missing-fixture-socket')}`
      : `ws://127.0.0.1:${deadPort}`;
    // Simulate an unavailable component only in this stopped-daemon fixture
    // row. Never rename/remove the actual Node executable or installation.
    record.dsh_session.node = missingNode;
  });
  assert.ok(running(recoveredReady.pid), 'broken-endpoint target exited before close');
  await t.test('broken endpoint and missing stored Node still close only the target named scope', async () => {
    assert.equal(storedRecord().dsh_session.node, missingNode);
    assert.equal(existsSync(missingNode), false);
    await control('cutex/runtime/close');
    await poll(async () => running(recoveredReady.pid), alive => !alive, 'broken RPC fallback terminated exact target scope');
    assert.ok(running(sentinelReady.pid), 'broken RPC fallback killed unrelated sentinel scope');
    assert.equal(storedRecord().app_server_runtime ?? null, null);
    brokenEndpointCleaned = true;
  });
  await restartDaemonsWithPrivateRecordChange(record => {
    assert.equal(record.dsh_session.node, missingNode);
    record.dsh_session.node = savedNode;
  });
  assert.equal(storedRecord().dsh_session.node, savedNode);
  assert.equal(existsSync(savedNode), true);
  assert.ok(running(sentinelReady.pid), 'restoring fixture component path disturbed unrelated sentinel');
  // Forced cleanup can leave an old ready file. A successful same-session
  // relaunch also proves the native writer lease was released by scope cleanup.
  await control('cutex/runtime/online');
  const afterForcedClose = await poll(() => api(sessionPath), value => value.runtime?.appServerConnected, 'relaunch after broken endpoint force close');
  generation++;
  assert.equal(afterForcedClose.runtime.runtimeGeneration, generation);
  assert.equal(JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8')).nativeSessionId, ready.nativeSessionId);
  let archivePreserved = false;
  await t.test('retire stops the live DSH scope and restore preserves native binding and offline history', async () => {
    // The target was explicitly brought online after broken-endpoint close;
    // no new prompt is sent, so this exercises archive's live DSH stop branch.
    const active = await api(sessionPath);
    assert.equal(active.lifecycle, 'active');
    assert.equal(active.runtime.appServerConnected, true);
    assert.equal(active.runtime.runtimeGeneration, generation);
    const activeReady = JSON.parse(readFileSync(join(stateDir, 'ready.json'), 'utf8'));
    assert.ok(running(activeReady.pid));
    const sourceBinding = structuredClone(storedRecord().dsh_session);
    const nativeLog = await locateNativeLog(join(stateDir, 'home', 'sessions'), ready.nativeSessionId);
    assert.ok(nativeLog, 'native history source missing before retire');
    const sourceBytes = readFileSync(nativeLog);
    const retiredReceipt = await api(sessionPath + '/cutex/requests', {
      requestId: uuid(), method: 'cutex/session/retire', params: {
        expectedRevision: active.revision, expectedRuntimeGeneration: generation, reason: 'product_archive_acceptance',
      },
    });
    assert.equal(retiredReceipt.cutex.result.lifecycle, 'retired');
    assert.equal(retiredReceipt.cutex.result.runtimeGeneration, generation);
    await poll(async () => running(activeReady.pid), alive => !alive, 'retire terminated exact live DSH adapter');
    assert.ok(running(sentinelReady.pid), 'retire killed unrelated sentinel scope');
    const retired = await api(sessionPath + '?lifecycle=retired');
    assert.equal(retired.lifecycle, 'retired');
    assert.equal(retired.runtime.status, 'offline');
    assert.equal(retired.runtime.runtimeGeneration, generation);
    assert.equal(retired.native.threadId, facade);
    assert.equal(storedRecord().app_server_runtime ?? null, null);
    assert.deepEqual(storedRecord().dsh_session, sourceBinding, 'retire changed the durable DSH source binding');
    assert.equal(existsSync(join(stateDir, 'ready.json')), false);
    const retiredHistory = await api(sessionPath + '/history');
    assert.equal(retiredHistory.source, 'offline');
    assert.equal(retiredHistory.native.result.thread.id, facade);
    assert.deepEqual(fullTurns(retiredHistory.native.result.thread.turns), resumedTurns);
    assert.equal(await locateNativeLog(join(stateDir, 'home', 'sessions'), ready.nativeSessionId), nativeLog);
    assert.deepEqual(readFileSync(nativeLog), sourceBytes, 'retire deleted or changed the native source log');
    const restoreReceipt = await api(sessionPath + '/cutex/requests', {
      requestId: uuid(), method: 'cutex/session/restore', params: { expectedRevision: retired.revision },
    });
    assert.equal(restoreReceipt.cutex.result.lifecycle, 'active');
    assert.equal(restoreReceipt.cutex.result.status, 'offline');
    assert.equal(restoreReceipt.cutex.result.runtimeGeneration, generation);
    const restoredArchive = await api(sessionPath);
    assert.equal(restoredArchive.lifecycle, 'active');
    assert.equal(restoredArchive.runtime.appServerConnected, false);
    assert.equal(restoredArchive.runtime.runtimeGeneration, generation);
    assert.equal(restoredArchive.native.threadId, facade);
    assert.deepEqual(storedRecord().dsh_session, sourceBinding, 'restore changed the durable DSH source binding');
    assert.equal(storedRecord().app_server_runtime ?? null, null);
    assert.equal(existsSync(join(stateDir, 'ready.json')), false);
    assert.deepEqual(readFileSync(nativeLog), sourceBytes, 'restore deleted or changed the native source log');
    assert.deepEqual(fullTurns((await api(sessionPath + '/history')).native.result.thread.turns), resumedTurns);
    archivePreserved = true;
  });
  // A failed archive subtest remains failed; stop only the fixture so later
  // offline/secret checks do not report secondary failures from a live target.
  if (!archivePreserved) await control('cutex/runtime/close');
  const finalOffline = await api(sessionPath + '/history');
  assert.equal(finalOffline.source, 'offline');
  assert.deepEqual(fullTurns(finalOffline.native.result.thread.turns), resumedTurns);
  assert.equal((await api(sessionPath)).runtime.runtimeGeneration, generation);
  assert.equal(existsSync(join(stateDir, 'ready.json')), false);
  const sentinelStatus = await api(sentinelPath);
  await api(sentinelPath + '/cutex/requests', { requestId: uuid(), method: 'cutex/runtime/close', params: { expectedRuntimeGeneration: sentinelStatus.runtime.runtimeGeneration, reason: 'product_sentinel_cleanup' } });
  scanSecretsWhileRunning();
  await stop(management); await stop(bus);
  // Windows can lock live WAL/lease files. Deferral is allowed only above:
  // the final stopped-fixture scan must read every persisted regular file.
  await poll(async () => { assertNoSecret(home, secret); return true; }, Boolean, 'complete secret scan after all fixture processes stop', 10000);
  assert.ok(!logs.join('').includes(secret), 'provider key leaked into daemon logs');
  console.log(JSON.stringify({ cutexSessionId: id, facade, nativeSessionId: ready.nativeSessionId, generation,
    profileCrud: true, startFalseNoAcp: true, managementApprovals: 2, resumedPromptCompleted: true,
    bindingLostOrphanRecovered: orphanRecovered, brokenEndpointExactScopeCleanup: brokenEndpointCleaned, unrelatedSentinelSurvived: true,
    closeWithoutNodeComponent: brokenEndpointCleaned,
    retireStopsExactScope: archivePreserved, archiveSourceBindingUnchanged: archivePreserved, restoreOffline: archivePreserved,
    offlineHistoryNoRuntime: true, secretsNotPersisted: true }));
  passed = orphanRecovered && brokenEndpointCleaned && archivePreserved;
});
