// Private-daemon acceptance of DSH revert, fork and owner delete through the
// ordinary Management v2 methods (cutex/session/history/revert, cutex/session/fork,
// cutex/session/delete) against real DSH 0.2.1-rc.3 and a keyless offline LLM.
// Explicit opt-in only: DSH_PRODUCT_INSTALL=<install root> DSH_PRODUCT_DAEMON=<debug cutex>.
// The private home is created under $HOME/.cutex-dsh-spike; run with HOME set to a scratch directory.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync, chmodSync, readdirSync, readlinkSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';
import { DatabaseSync } from 'node:sqlite';

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
async function poll(read, accepts, description, timeout = 60000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    try { last = await read(); if (accepts(last)) return last; }
    catch (error) { last = error.message; }
    await delay(100);
  }
  throw new Error(`Timed out: ${description}; last=${JSON.stringify(last)?.slice(0, 2000)}`);
}
async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  child.kill('SIGKILL');
  await exited;
}

const realKeyFile = process.env.DSH_STRUCTURE_REAL_KEY_FILE;

// Linux: processes whose cwd or an open file lies inside `dir` (Windows
// refuses to rename such a directory: the offline-history-reader failure).
function holders(dir) {
  if (process.platform !== 'linux') return [];
  const found = [];
  for (const pid of readdirSync('/proc').filter(name => /^\d+$/.test(name))) {
    const inside = link => { try { const target = readlinkSync(link); return target === dir || target.startsWith(dir + '/'); } catch { return false; } };
    let fds = [];
    try { fds = readdirSync(`/proc/${pid}/fd`); } catch {}
    if (inside(`/proc/${pid}/cwd`) || fds.some(fd => inside(`/proc/${pid}/fd/${fd}`))) found.push(Number(pid));
  }
  return found;
}

async function setup(t, { real = false } = {}) {
  const installRoot = resolve(install), daemon = resolve(binary);
  const metadata = JSON.parse(readFileSync(join(installRoot, 'node_modules/@deepseek-ai/dsh/package.json'), 'utf8'));
  assert.equal(metadata.version, '0.2.1-rc.3');
  const base = join(homedir(), '.cutex-dsh-spike');
  mkdirSync(base, { recursive: true });
  const home = mkdtempSync(join(base, 'structure-'));
  chmodSync(home, 0o700);
  writeFileSync(join(home, '.cutex-test-private-home'), 'keyless DSH structure acceptance\n');
  const configDir = join(home, '.cutex'), cwd = join(home, 'workspace');
  mkdirSync(configDir); mkdirSync(cwd);
  const deploymentDir = join(configDir, 'runtime', 'dsh');
  mkdirSync(deploymentDir, { recursive: true });
  writeFileSync(join(deploymentDir, 'deployment.json'), JSON.stringify({ node: process.execPath, adapter: fileURLToPath(new URL('../src/server.mjs', import.meta.url)),
    dsh_cli: join(installRoot, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), install_root: installRoot, version: metadata.version }), { mode: 0o600 });
  // Offline LLM that answers in text and records the user prompts it was given.
  const audit = join(home, 'provider-audit.jsonl');
  const pluginPath = join(home, 'structure-llm.mjs');
  writeFileSync(pluginPath, `
import { appendFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);
class Offline extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'structure fixture' }; }
  async listModels(provider) { return [{ provider, id: 'offline', name: 'Offline', inputModalities: ['text'] }]; }
  async resolveModel(provider, model) { return { provider, id: model, name: model, inputModalities: ['text'], context: { contextWindow: 4096 } }; }
  async *stream(options) {
    const prompts = options.messages.filter(m => m.role === 'user' && m.source?.kind === 'user').map(m => m.content.filter(b => b.type === 'text').map(b => b.text).join(''));
    appendFileSync(${JSON.stringify(audit)}, JSON.stringify(prompts) + '\\n');
    const text = 'answer to ' + prompts.at(-1);
    yield { type: 'block-start', index: 0, blockType: 'text' };
    yield { type: 'text-delta', index: 0, text };
    yield { type: 'block-end', index: 0, block: { type: 'text', text } };
    yield { type: 'usage', usage: { inputTokens: 10, outputTokens: 5 } };
    yield { type: 'finish', reason: { kind: 'stop' } };
  }
}
export const name = 'cutex-structure-offline-llm';
export const inject = ['llm'];
export function apply(ctx) { ctx.llm.registerAdapter(['cutex-offline'], new Offline()); }
`);
  const patch = join(home, 'offline.patch.yml');
  writeFileSync(patch, `
- id: llm-deepseek
  disabled: true
- id: acp
  config: { provider: cutex-offline, model: offline }
- id: session-log-deepseek
  config: { enabled: false }
- insert:
    - id: cutex-structure-offline-llm
      name: '${pluginPath.replaceAll("'", "''")}'
`);
  const managementPort = await freePort();
  let busPort = await freePort();
  while (busPort === managementPort) busPort = await freePort();
  const rootToken = 'structure-test-root-credential-only', busToken = 'structure-test-separate-bus-credential';
  const secretName = 'DSH_STRUCTURE_TEST_KEY';
  // Real-provider mode: the owner's prepaid TEST key, read here and passed only in the daemon's environment.
  const secret = real ? readFileSync(realKeyFile, 'utf8').trim() : `dummy-structure-api-key-${uuid()}`;
  assert.ok(secret.length > 8);
  writeFileSync(join(configDir, 'config.json'), JSON.stringify({ agent_bus_enabled: true, agent_bus_port: busPort, agent_bus_token: busToken, management_port: managementPort }));
  const env = Object.fromEntries(['PATH', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'].filter(name => process.env[name]).map(name => [name, process.env[name]]));
  Object.assign(env, { HOME: home, USERPROFILE: home, TEMP: home, TMP: home, TMPDIR: home, CUTEX_TEST_PRIVATE_HOME: home, CUTEX_TEST_FORBID_REAL_HOME: '1',
    CUTEX_DSH_SPIKE: '1', ...(real ? {} : { CUTEX_DSH_TEST_PATCH: patch }), [secretName]: secret });
  const children = [], logs = [];
  const start = args => {
    const child = spawn(daemon, args, { env, cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    children.push(child);
    child.stdout.on('data', chunk => logs.push(String(chunk)));
    child.stderr.on('data', chunk => logs.push(String(chunk)));
    return child;
  };
  const url = `http://127.0.0.1:${managementPort}`;
  async function call(path, body, method = body === undefined ? 'GET' : 'POST') {
    const response = await fetch(url + path, { method, headers: { Authorization: `Bearer ${rootToken}`, 'Content-Type': 'application/json' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(240000) });
    const text = await response.text();
    assert.ok(!text.includes(secret), 'Management exposed the dummy provider key');
    return { status: response.status, body: text ? JSON.parse(text) : null };
  }
  async function api(path, body, method) {
    const { status, body: result } = await call(path, body, method);
    assert.ok(status >= 200 && status < 300, `${path}: ${status} ${JSON.stringify(result)}`);
    return result;
  }
  const sessionPath = id => `/v2/sessions/${encodeURIComponent(id)}`;
  const cutex = (id, method, params = {}) => call(sessionPath(id) + '/cutex/requests', { requestId: uuid(), method, params });
  const cutexOk = async (id, method, params) => {
    const { status, body } = await cutex(id, method, params);
    assert.ok(status >= 200 && status < 300, `${method}: ${status} ${JSON.stringify(body)}`);
    return body.cutex.result;
  };
  const stored = id => {
    const db = new DatabaseSync(join(configDir, 'cutex-sessions.sqlite3'), { readOnly: true });
    try { db.exec('PRAGMA busy_timeout=5000'); return JSON.parse(db.prepare('SELECT record_json FROM sessions WHERE cutex_session_id=?').get(id)?.record_json ?? 'null'); }
    finally { db.close(); }
  };
  const tracked = new Set();
  const state = { passed: false };
  t.after(async () => {
    if (!state.passed) console.error(logs.join('').replaceAll(secret, '[REDACTED]').slice(-8000));
    for (const id of tracked) {
      try {
        const state = await api(sessionPath(id));
        await cutex(id, 'cutex/runtime/close', { expectedRuntimeGeneration: state.runtime.runtimeGeneration, reason: 'structure_fixture_cleanup' });
      } catch {}
    }
    for (const child of [...children].reverse()) await stop(child);
    await delay(200);
    if (!state.passed && process.env.DSH_PRODUCT_RETAIN_FAILURE === '1') { console.error(`Retained stopped private fixture: ${home}`); return; }
    rmSync(home, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  });
  start(['agent', 'serve', '--port', String(busPort)]);
  start(['management', 'serve', '--port', String(managementPort), '--token', rootToken]);
  await poll(() => api('/v2/sessions'), value => value.contractVersion === 2, 'private Management ready');
  const { profile } = await api('/v2/profiles', { name: real ? 'DSH Structure Real' : 'DSH Structure Offline', kind: 'dsh',
    options: { dsh: real ? { apiKeyEnv: secretName } : { model: 'offline', apiKeyEnv: secretName } } });

  const online = async id => {
    const state = await api(sessionPath(id));
    if (!state.runtime.appServerConnected) await cutexOk(id, 'cutex/runtime/online', { expectedRuntimeGeneration: state.runtime.runtimeGeneration, openVisibleTerminal: false });
    return poll(() => api(sessionPath(id)), value => value.runtime?.appServerConnected, `${id} online`);
  };
  const close = async id => {
    const state = await api(sessionPath(id));
    await cutexOk(id, 'cutex/runtime/close', { expectedRuntimeGeneration: state.runtime.runtimeGeneration, reason: 'structure_acceptance' });
    return poll(() => api(sessionPath(id)), value => !value.runtime?.appServerConnected, `${id} offline`);
  };
  const historyTurns = async id => (await api(sessionPath(id) + '/history')).native.result.thread.turns;
  const prompt = async (id, text, expected) => {
    const facade = (await api(sessionPath(id))).native.threadId;
    await api(sessionPath(id) + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method: 'turn/start', params: { threadId: facade, input: [{ type: 'text', text }] } } } });
    const turns = await poll(() => historyTurns(id), turns => turns.length === expected && turns.every(turn => turn.status === 'completed'), `${id} has ${expected} completed turns`, real ? 180000 : 60000);
    // The timeline commits the turn before the runtime's live terminal reaches the daemon.
    await poll(() => api(sessionPath(id)), value => !value.runtime?.activeTurnId, `${id} idle`);
    return turns;
  };
  const lastPrompts = () => JSON.parse(readFileSync(audit, 'utf8').trim().split('\n').at(-1));
  // One turn, watching only the newest history page (long histories).
  const quickPrompt = async (id, text, number) => {
    const facade = (await api(sessionPath(id))).native.threadId;
    await api(sessionPath(id) + '/app-server/requests', { requestId: uuid(), native: { message: { id: uuid(), method: 'turn/start', params: { threadId: facade, input: [{ type: 'text', text }] } } } });
    const expected = `${stored(id).dsh_session.native_session_id}:turn:${number}`;
    await poll(async () => (await api(sessionPath(id) + '/history?limit=1')).native.result.thread.turns.at(-1),
      turn => turn?.id === expected && turn.status === 'completed', `${expected} completed`);
    await poll(() => api(sessionPath(id)), value => !value.runtime?.activeTurnId, `${id} idle`);
  };
  return { state, api, call, cutex, cutexOk, stored, tracked, online, close, historyTurns, prompt, quickPrompt, lastPrompts, profile, cwd, sessionPath };
}

test('DSH revert, fork and delete through Management v2 on a private daemon', { skip: !install || !binary, timeout: 600000 }, async t => {
  const { state, api, call, cutex, cutexOk, stored, tracked, online, close, historyTurns, prompt, lastPrompts, profile, cwd, sessionPath } = await setup(t);

  const created = await api('/v2/human/agents', { requestId: uuid(), name: 'structure-dsh', cwd, profile: profile.id, start: false });
  const id = created.cutexSessionId;
  tracked.add(id);
  const features = (await api(sessionPath(id))).native.schema.capabilities.features;
  assert.equal(features.threadFork, true);
  assert.equal(features.threadRevert, true);
  assert.equal(features.threadDelete, true);
  await online(id);
  const native = stored(id).dsh_session.native_session_id;
  assert.ok(native);
  for (const [n, text] of ['FIRST', 'SECOND', 'THIRD'].entries()) await prompt(id, text, n + 1);
  const original = (await historyTurns(id)).map(turn => turn.id);
  assert.deepEqual(original, [1, 2, 3].map(n => `${native}:turn:${n}`));

  // Refusals: files cannot be rewound, unknown turns are named.
  const rewind = await cutex(id, 'cutex/session/history/revert', { beforeTurnId: original[1], rewindFiles: true });
  assert.equal(rewind.body.error.code, 'rewind_files_unsupported', JSON.stringify(rewind.body));
  const unknown = await cutex(id, 'cutex/session/history/revert', { beforeTurnId: `${native}:turn:9` });
  assert.equal(unknown.body.error.code, 'turn_not_found', JSON.stringify(unknown.body));
  assert.equal((await api(sessionPath(id))).runtime.appServerConnected, true, 'a refused revert leaves the runtime online');

  // Revert of an online agent: conversation cut, runtime restarted, same identities.
  const generation = (await api(sessionPath(id))).runtime.runtimeGeneration;
  const reverted = await cutexOk(id, 'cutex/session/history/revert', { beforeTurnId: original[1] });
  assert.deepEqual(reverted.removedTurnIds, [original[2], original[1]]);
  assert.equal(reverted.removedTurns, 2);
  assert.equal(reverted.runtimeRestarted, true);
  assert.equal(reverted.runtimeState, 'online');
  assert.deepEqual(reverted.warnings, []);
  assert.equal(reverted.native.method, 'dsh/thread/revert');
  assert.equal(reverted.native.result.filesRestored, false);
  assert.ok(existsSync(reverted.backupPath), 'previous native log kept for recovery');
  assert.ok(reverted.native.result.previousTimeline.startsWith(reverted.native.result.recoveryDirectory) && existsSync(reverted.native.result.previousTimeline), 'previous timeline kept for recovery');
  assert.ok(reverted.runtimeGeneration > generation);
  const afterRevert = await online(id);
  assert.equal(afterRevert.native.threadId, created.session.native.threadId);
  assert.equal(stored(id).dsh_session.native_session_id, native, 'same native session');
  assert.deepEqual((await historyTurns(id)).map(turn => turn.id), [original[0]], 'kept turn keeps its ID');
  await prompt(id, 'AFTER_REVERT', 2);
  assert.deepEqual(lastPrompts(), ['FIRST', 'AFTER_REVERT'], 'the reverted turns are gone from the model context');
  const sourceTurns = (await historyTurns(id)).map(turn => turn.id);

  // Fork after turn 1 into a new agent with its own DSH home.
  const forked = await cutexOk(id, 'cutex/session/fork', { afterTurnId: original[0], name: 'structure-dsh-fork' });
  const forkId = forked.newCutexSessionId;
  tracked.add(forkId);
  assert.equal(forked.forkedThroughTurnId, original[0]);
  assert.equal(forked.sourceThreadId, created.session.native.threadId);
  assert.notEqual(forked.threadId, forked.sourceThreadId);
  assert.equal(forked.session.runtime.backend, 'dsh');
  assert.ok(forked.historyBytes > 0);
  const forkRecord = stored(forkId);
  assert.equal(forkRecord.dsh_session.thread_id, forked.threadId);
  assert.notEqual(forkRecord.dsh_session.native_session_id, native);
  assert.equal(forkRecord.profile, stored(id).profile);
  assert.equal(forkRecord.dsh_session.model, 'offline');
  const forkNative = forkRecord.dsh_session.native_session_id;
  assert.deepEqual((await historyTurns(forkId)).map(turn => turn.id), [`${forkNative}:turn:1`], 'offline history of the unstarted fork');
  await online(forkId);
  await prompt(forkId, 'IN_FORK', 2);
  assert.deepEqual(lastPrompts(), ['FIRST', 'IN_FORK']);
  assert.deepEqual((await historyTurns(id)).map(turn => turn.id), sourceTurns, 'the source is unchanged');

  // Revert of a stopped agent: no restart.
  await close(id);
  // Offline history first (a warm reader on the stopped agent's state), then revert.
  assert.deepEqual((await historyTurns(id)).map(turn => turn.id), sourceTurns);
  assert.ok(holders(stored(id).dsh_session.state_dir).length > 0 || process.platform !== 'linux', 'the offline reader is warm before the revert');
  const offlineRevert = await cutexOk(id, 'cutex/session/history/revert', { beforeTurnId: sourceTurns[1] });
  assert.equal(offlineRevert.runtimeRestarted, false);
  assert.equal(offlineRevert.runtimeState, 'offline');
  assert.equal(offlineRevert.live, false);
  assert.equal(offlineRevert.runtimePid, null);
  assert.equal((await api(sessionPath(id))).runtime.appServerConnected, false, 'a stopped agent stays stopped');
  assert.deepEqual((await historyTurns(id)).map(turn => turn.id), [original[0]], 'offline history is complete without a runtime');
  const sourceState = stored(id).dsh_session.state_dir;
  assert.ok(holders(sourceState).length > 0 || process.platform !== 'linux', 'the offline history reader is warm on the stopped source');

  // Owner delete: the private state moves into the recovery directory.
  const forkState = forkRecord.dsh_session.state_dir;
  const deleted = await cutexOk(forkId, 'cutex/session/delete', {});
  assert.equal(deleted.deleted, true);
  assert.equal(deleted.nativeDelete, 'dshState');
  assert.equal(deleted.runtimeStopped, true);
  assert.equal(existsSync(forkState), false);
  assert.ok(existsSync(join(deleted.backupPath, 'dsh-state', 'timeline.sqlite')));
  assert.ok(existsSync(join(deleted.backupPath, 'dsh-state', 'home', 'sessions')));
  tracked.delete(forkId);
  assert.equal((await call(sessionPath(forkId))).status, 404);
  // Offline history, then delete: the warm reader is stopped (and waited for) before the state moves.
  const deletedSource = await cutexOk(id, 'cutex/session/delete', {});
  assert.equal(deletedSource.nativeDelete, 'dshState');
  assert.deepEqual(holders(join(deletedSource.backupPath, 'dsh-state')), [], 'no process kept the moved DSH state (offline history reader)');
  assert.equal(existsSync(sourceState), false);
  assert.equal(deletedSource.runtimeStopped, false);
  tracked.delete(id);
  assert.equal(stored(id), null);
  state.passed = true;
});

// One tiny real-provider acceptance (3 DeepSeek requests): the reverted turn is
// really gone from the model's context. Opt-in: DSH_STRUCTURE_REAL_KEY_FILE.
test('DSH revert with the real DeepSeek provider forgets the reverted turn', { skip: !install || !binary || !realKeyFile, timeout: 600000 }, async t => {
  const { state, api, cutex, cutexOk, stored, tracked, online, historyTurns, prompt, profile, cwd } = await setup(t, { real: true });
  const created = await api('/v2/human/agents', { requestId: uuid(), name: 'structure-dsh-real', cwd, profile: profile.id, start: false });
  const id = created.cutexSessionId;
  tracked.add(id);
  await online(id);
  await prompt(id, 'Remember the code word APPLE. Reply with just OK.', 1);
  await prompt(id, 'Also remember the code word BANANA. Reply with just OK.', 2);
  const turns = (await historyTurns(id)).map(turn => turn.id);
  const reverted = await cutexOk(id, 'cutex/session/history/revert', { beforeTurnId: turns[1] });
  assert.equal(reverted.runtimeRestarted, true);
  await online(id);
  const answered = await prompt(id, 'List every code word I asked you to remember in this conversation, comma separated, nothing else.', 2);
  const answer = answered.at(-1).items.filter(item => item.type === 'agentMessage').map(item => item.text).join(' ');
  assert.match(answer, /APPLE/i, answer);
  assert.doesNotMatch(answer, /BANANA/i, answer);
  assert.equal(stored(id).dsh_session.native_session_id != null, true);
  const deleted = await cutexOk(id, 'cutex/session/delete', {});
  assert.equal(deleted.nativeDelete, 'dshState');
  tracked.delete(id);
  state.passed = true;
});

// Long history: a revert result lists every kept and removed turn ID, far more
// than a pipe buffer (64 KiB on Linux: ~1300 turn IDs). The helper's output is
// read while it runs (before: the helper blocked on the full pipe, cutex waited
// for its exit, and the revert failed at the 300 s timeout). Opt-in:
// DSH_STRUCTURE_LONG_TURNS=<turns> (e.g. 1500).
const longTurns = Number(process.env.DSH_STRUCTURE_LONG_TURNS ?? 0);
test('DSH revert and fork of a long history through Management v2', { skip: !install || !binary || !longTurns, timeout: 3600000 }, async t => {
  const { state, api, cutexOk, stored, tracked, online, close, quickPrompt, profile, cwd, sessionPath } = await setup(t);
  const created = await api('/v2/human/agents', { requestId: uuid(), name: 'structure-dsh-long', cwd, profile: profile.id, start: false });
  const id = created.cutexSessionId;
  tracked.add(id);
  await online(id);
  for (let n = 1; n <= longTurns; n++) await quickPrompt(id, `T${n}`, n);
  const native = stored(id).dsh_session.native_session_id;
  await close(id);
  // Fork the whole long history (a copy of the full log), then revert the source.
  const forked = await cutexOk(id, 'cutex/session/fork', { afterTurnId: `${native}:turn:${longTurns}`, name: 'structure-dsh-long-fork' });
  tracked.add(forked.newCutexSessionId);
  assert.equal(forked.forkedThroughTurnId, `${native}:turn:${longTurns}`);
  const forkTurns = (await api(sessionPath(forked.newCutexSessionId) + '/history?limit=1')).native.result.thread.turns;
  assert.equal(forkTurns.at(-1).id, `${stored(forked.newCutexSessionId).dsh_session.native_session_id}:turn:${longTurns}`);
  const started = Date.now();
  const reverted = await cutexOk(id, 'cutex/session/history/revert', { beforeTurnId: `${native}:turn:2` });
  assert.equal(reverted.removedTurns, longTurns - 1);
  assert.ok(JSON.stringify(reverted.removedTurnIds).length > 64 * 1024, 'the helper result exceeded a pipe buffer');
  assert.ok(Date.now() - started < 120000, `revert took ${Date.now() - started} ms`);
  assert.deepEqual((await api(sessionPath(id) + '/history')).native.result.thread.turns.map(turn => turn.id), [`${native}:turn:1`]);
  for (const target of [forked.newCutexSessionId, id]) {
    await cutexOk(target, 'cutex/session/delete', {});
    tracked.delete(target);
  }
  state.passed = true;
});
