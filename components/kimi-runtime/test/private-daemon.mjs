// Shared harness: a private debug cutex daemon (Bus + Management) with Kimi
// Code agents, inside a marked private home under ~/.cutex-kimi-e2e. Never the
// live install, never default ports, never a real key or network: the provider
// is the scripted offline mock (CUTEX_KIMI_TEST_BASE_URL, debug + private home
// only). Every child is killed on teardown.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createServer } from 'node:net';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync, chmodSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { once } from 'node:events';

export const binary = process.env.KIMI_E2E_DAEMON;
export const delay = ms => new Promise(done => setTimeout(done, ms));
export const uuid = () => crypto.randomUUID();
const LIVE_PORTS = [24270, 24260, 25370, 25360];

async function freePort(taken) {
  for (;;) {
    const server = createServer();
    await new Promise(done => server.listen(0, '127.0.0.1', done));
    const port = server.address().port;
    await new Promise(done => server.close(done));
    if (!LIVE_PORTS.includes(port) && !taken.includes(port)) return port;
  }
}
export async function poll(read, accepts, description, timeout = 60000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    try { last = await read(); if (accepts(last)) return last; } catch (error) { last = error.message; }
    await delay(200);
  }
  throw new Error(`Timed out: ${description}; last=${JSON.stringify(last)?.slice(-3000)}`);
}
// Delivered input projects as `external_event` items (r265); tests read them as their frames.
export const userTexts = turns => turns.flatMap(turn => turn.items)
  .filter(item => item.type === 'userMessage' || (item.type === 'functionCallOutput' && item.name === 'external_event'))
  .map(item => item.type === 'userMessage' ? item.content.filter(block => block.type === 'text').map(block => block.text).join('') : `EXTERNAL_INPUT_JSON\n${item.output}`);
export const agentTexts = turns => turns.flatMap(turn => turn.items).filter(item => item.type === 'agentMessage').map(item => item.text);

/**
 * @param component a built kimi-runtime payload (build.sh stage or extracted archive),
 *   installed with `cutex component install kimi-runtime --from-source`.
 * @param baseUrl the offline provider every API-key Kimi launch talks to.
 * @param secrets strings never printed: redacted from every diagnostic this harness writes.
 */
export async function privateDaemon(t, { label, env: extraEnv = {}, config = {}, component, baseUrl, secrets = [] }) {
  const redact = text => secrets.filter(Boolean).reduce((value, secret) => value.replaceAll(secret, '[redacted]'), String(text));
  const daemon = resolve(binary);
  const base = join(homedir(), '.cutex-kimi-e2e');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, `${label}-`));
  chmodSync(home, 0o700);
  writeFileSync(join(home, '.cutex-test-private-home'), `Kimi ${label} acceptance\n`);
  const configDir = join(home, '.cutex'), cwd = join(home, 'workspace');
  mkdirSync(configDir); mkdirSync(cwd);
  // Kimi's project root is the nearest ancestor with .git: the workspace is its own repository.
  assert.equal(spawnSync('git', ['init', '-q', cwd], { encoding: 'utf8' }).status, 0, 'git init workspace');
  const deploymentDir = join(configDir, 'runtime', 'kimi');
  const managementPort = await freePort([]);
  const busPort = await freePort([managementPort]);
  const rootToken = `${label}-root-credential-only-${uuid()}`;
  writeFileSync(join(configDir, 'config.json'), JSON.stringify({ agent_bus_enabled: true, agent_bus_port: busPort, agent_bus_token: `${label}-bus-${uuid()}`,
    management_port: managementPort, agent_security: { cross_project: 'host' }, ...config }));
  const env = Object.fromEntries(['PATH', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'].filter(name => process.env[name]).map(name => [name, process.env[name]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, TMPDIR: home, CUTEX_TEST_PRIVATE_HOME: home, CUTEX_TEST_FORBID_REAL_HOME: '1',
    CUTEX_KIMI_TEST_BASE_URL: baseUrl });
  {
    const base = join(home, 'install-base');
    mkdirSync(base);
    writeFileSync(join(base, 'install.json'), JSON.stringify({ schema: 'cutex/install/v1', os: 'linux', serviceManager: 'fixture-no-services', channel: 'main',
      createdAt: new Date().toISOString(), activeRelease: `kimi-${label}-fixture` }));
    Object.assign(env, { CUTEX_INSTALL_BASE: base, CUTEX_KIMI_NODE: process.execPath });
    const installed = spawnSync(daemon, ['component', 'install', 'kimi-runtime', '--from-source', resolve(component)], { env, cwd, encoding: 'utf8', timeout: 20 * 60 * 1000 });
    assert.equal(installed.status, 0, `component install: ${installed.stderr?.slice(-3000)}`);
    const result = JSON.parse(installed.stdout);
    assert.equal(result.component.state, 'ready', JSON.stringify(result.component).slice(0, 2000));
    assert.ok(existsSync(join(deploymentDir, 'deployment.json')), 'the installed component published its deployment');
  }
  Object.assign(env, extraEnv);
  const children = [], logs = [], agents = [];
  let passed = false;
  const start = args => {
    const child = spawn(daemon, args, { env, cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    children.push(child);
    child.stdout.on('data', chunk => logs.push(String(chunk)));
    child.stderr.on('data', chunk => logs.push(String(chunk)));
    return child;
  };
  const url = `http://127.0.0.1:${managementPort}`;
  async function api(path, body, { method = body === undefined ? 'GET' : 'POST', headers = {} } = {}) {
    const response = await fetch(url + path, { method, headers: { Authorization: `Bearer ${rootToken}`, 'Content-Type': 'application/json', ...headers },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(180000) });
    const result = await response.json();
    assert.ok(response.ok, `${method} ${path}: ${response.status} ${JSON.stringify(result)}`);
    return result;
  }
  const history = async agent => (await api(agent.path + '/history')).native?.result?.thread?.turns ?? [];
  const state = agent => api(agent.path);
  const request = (agent, method, params) => api(agent.path + '/cutex/requests', { requestId: uuid(), method, params });
  t.after(async () => {
    if (!passed) {
      console.error(redact(logs.join('').slice(-6000)));
      for (const agent of agents) {
        try { console.error(redact(`history ${agent.name}: ${JSON.stringify(await history(agent)).slice(-4000)}`)); } catch (error) { console.error(redact(`history ${agent.name}: ${error.message}`)); }
        try { const value = await state(agent); console.error(redact(`state ${agent.name}: ${JSON.stringify({ runtime: value.runtime, status: value.status }).slice(0, 3000)}`)); } catch {}
        const log = join(deploymentDir, agent.id, 'adapter.log');
        if (existsSync(log)) console.error(redact(`adapter.log ${agent.name}:\n${readFileSync(log, 'utf8').slice(-6000)}`));
      }
      try { console.error(redact(`asks: ${JSON.stringify(await api('/v2/asks')).slice(0, 6000)}`)); } catch (error) { console.error(redact(`asks: ${error.message}`)); }
    }
    for (const agent of agents) {
      try { await stop(agent, `${label}_cleanup`); } catch {}
    }
    for (const child of children.reverse()) if (child.exitCode === null) { const exited = once(child, 'exit'); child.kill('SIGKILL'); await exited; }
    if (passed || process.env.KIMI_E2E_RETAIN_FAILURE !== '1') rmSync(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
    else console.error(`Retained private fixture: ${home}`);
  });
  const services = () => {
    start(['agent', 'serve', '--port', String(busPort)]);
    start(['management', 'serve', '--port', String(managementPort), '--token', rootToken]);
  };
  services();
  await poll(() => api('/v2/sessions'), value => value.contractVersion === 2, 'private Management ready');
  // r267: a daemon restart (Bus + Management killed and started again); agent runtimes are left to cutex's recovery.
  async function restartServices() {
    for (const child of children.splice(0)) if (child.exitCode === null && child.signalCode === null) { const exited = once(child, 'exit'); child.kill('SIGKILL'); await exited; }
    services();
    await poll(() => api('/v2/sessions'), value => value.contractVersion === 2, 'private Management ready after restart', 120000);
  }

  async function createAgent(name, profile, defaults) {
    const created = await api('/v2/human/agents', { requestId: uuid(), name, cwd, profile, start: false });
    const agent = { name, id: created.cutexSessionId, facade: created.session.native.threadId, path: `/v2/sessions/${encodeURIComponent(created.cutexSessionId)}` };
    agents.push(agent);
    if (defaults) await request(agent, 'cutex/session/defaults/update', { patch: defaults });
    return agent;
  }
  async function online(agent) {
    const generation = (await state(agent)).runtime?.runtimeGeneration ?? 0;
    await request(agent, 'cutex/runtime/online', { expectedRuntimeGeneration: generation, openVisibleTerminal: false, reason: `${label}_acceptance` });
    return poll(() => state(agent), value => value.runtime?.appServerConnected && value.runtime.runtimeGeneration > generation, `${agent.name} online`, 180000);
  }
  async function stop(agent, reason = `${label}_stop`) {
    const value = await state(agent);
    await request(agent, 'cutex/runtime/close', { expectedRuntimeGeneration: value.runtime.runtimeGeneration, reason });
    return poll(() => state(agent), next => !next.runtime?.appServerConnected, `${agent.name} stopped`, 60000);
  }
  // r266: owner input is refused 502 `admission_deferred` (retryable) while the
  // admission authority fence is briefly held; retry like a client does.
  const send = async (agent, text) => {
    for (let attempt = 0; ; attempt++) {
      try { return await api(agent.path + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method: 'turn/start', params: { threadId: agent.facade, input: [{ type: 'text', text }] } } } }); }
      catch (error) { if (attempt >= 20 || !/admission_deferred/.test(error.message)) throw error; await delay(500); }
    }
  };
  const idle = agent => poll(() => state(agent), value => value.runtime?.appServerConnected && !(value.runtime?.activeTurnId) && value.status?.type !== 'active', `${agent.name} idle`).catch(() => {});
  async function answer(agent, request, decision) {
    const generation = (await state(agent)).runtime.runtimeGeneration;
    return api(agent.path + '/app-server/server-request-responses', { responseId: uuid(), native: { message: { id: request.id, result: { decision } } } },
      { headers: { 'If-Match': `"cutex-runtime-generation:${generation}"` } });
  }
  return { api, home, cwd, env, deploymentDir, logs, agents, history, state, request, createAgent, online, stop, send, idle, answer, restartServices, url, rootToken, markPassed: () => { passed = true; } };
}
